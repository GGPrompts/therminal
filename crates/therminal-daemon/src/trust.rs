//! Trust tier enforcement for MCP tool dispatch.
//!
//! Maps each MCP tool to a required [`TrustTier`], resolves the connecting
//! agent's tier from config, and enforces access control with audit logging
//! and rate limiting for destructive operations.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use therminal_core::config::{TrustConfig, TrustTier};
use tracing::{info, warn};

// ── Required tier per tool ──────────────────────────────────────────────

/// Permission level required to invoke a given MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    /// Read-only tools: terminal.sessions.list, terminal.sessions.get, terminal.panes.get_content.
    Observer,
    /// Write tools: terminal.sessions.create, terminal.panes.write.
    Writer,
    /// Destructive tools: terminal.sessions.destroy.
    Admin,
}

impl ToolCategory {
    /// The minimum [`TrustTier`] required for this category.
    pub fn required_tier(self) -> TrustTier {
        match self {
            Self::Observer => TrustTier::Sandboxed,
            Self::Writer => TrustTier::Supervised,
            Self::Admin => TrustTier::Trusted,
        }
    }
}

/// Classify an MCP tool name into its permission category.
///
/// Returns `None` for unknown tool names (handled separately as invalid).
pub fn tool_category(tool_name: &str) -> Option<ToolCategory> {
    match tool_name {
        "terminal.sessions.list"
        | "terminal.sessions.get"
        | "terminal.panes.list"
        | "terminal.panes.get_geometry"
        | "terminal.panes.get_content"
        | "terminal.semantic.query_history" => Some(ToolCategory::Observer),
        "terminal.sessions.create" | "terminal.panes.write" => Some(ToolCategory::Writer),
        "terminal.sessions.destroy" => Some(ToolCategory::Admin),
        _ => None,
    }
}

// ── Agent identity resolution ───────────────────────────────────────────

/// Resolved identity of an MCP client.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// Agent name as reported by the MCP client's `Implementation.name`,
    /// or `"unknown"` if not available.
    pub name: String,
}

impl AgentIdentity {
    /// Resolve the agent's [`TrustTier`] by looking up its name in the
    /// per-agent config map, falling back to `default_tier`.
    pub fn resolve_tier(&self, config: &TrustConfig) -> TrustTier {
        config
            .agents
            .get(&self.name)
            .map(|a| a.tier)
            .unwrap_or(config.default_tier)
    }
}

// ── Rate limiter ────────────────────────────────────────────────────────

/// Simple sliding-window rate limiter for destructive operations.
///
/// Tracks timestamps of recent invocations per agent name and rejects
/// calls that exceed the configured maximum per minute.
pub struct RateLimiter {
    /// Max operations per minute per agent. `0` means unlimited.
    max_per_minute: u32,
    /// Per-agent timestamps of recent destructive operations.
    windows: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    /// Create a new rate limiter with the given per-minute cap.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether the agent is allowed to perform a destructive operation.
    ///
    /// Returns `Ok(())` if allowed, or `Err(message)` if rate-limited.
    /// Automatically records the invocation on success.
    pub fn check_and_record(&self, agent_name: &str) -> Result<(), String> {
        if self.max_per_minute == 0 {
            return Ok(()); // Unlimited
        }

        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let one_minute_ago = now - std::time::Duration::from_secs(60);

        let timestamps = windows.entry(agent_name.to_string()).or_default();

        // Prune entries older than 1 minute.
        timestamps.retain(|t| *t > one_minute_ago);

        if timestamps.len() as u32 >= self.max_per_minute {
            Err(format!(
                "rate limit exceeded: agent {:?} has reached {} destructive operations per minute",
                agent_name, self.max_per_minute,
            ))
        } else {
            timestamps.push(now);
            Ok(())
        }
    }

    /// Update the rate limit cap (e.g. on config hot-reload).
    pub fn set_max_per_minute(&mut self, max: u32) {
        self.max_per_minute = max;
    }
}

// ── Trust gate (combines all checks) ───────────────────────────────────

/// Result of a trust enforcement check.
#[derive(Debug)]
pub enum TrustCheckResult {
    /// Tool call is allowed.
    Allowed,
    /// Tool call is denied with a reason.
    Denied(String),
}

/// Run the full trust gate for an MCP tool invocation.
///
/// 1. Resolve agent tier from config.
/// 2. Check tier against required level.
/// 3. For destructive tools, apply rate limiting.
/// 4. Audit-log the result.
pub fn check_tool_access(
    tool_name: &str,
    agent: &AgentIdentity,
    config: &TrustConfig,
    rate_limiter: &RateLimiter,
) -> TrustCheckResult {
    let category = match tool_category(tool_name) {
        Some(c) => c,
        None => {
            // Unknown tool — will be handled as "unknown tool" error by dispatch.
            // Still audit-log it.
            audit_log(agent, tool_name, "unknown_tool");
            return TrustCheckResult::Allowed;
        }
    };

    let agent_tier = agent.resolve_tier(config);
    let required = category.required_tier();

    if !agent_tier.has_access(required) {
        let reason = format!(
            "permission denied: agent {:?} has tier {:?}, tool {:?} requires {:?}",
            agent.name, agent_tier, tool_name, required,
        );
        audit_log_denied(agent, tool_name, &reason);
        return TrustCheckResult::Denied(reason);
    }

    // Rate-limit destructive operations.
    if category == ToolCategory::Admin
        && let Err(reason) = rate_limiter.check_and_record(&agent.name)
    {
        audit_log_denied(agent, tool_name, &reason);
        return TrustCheckResult::Denied(reason);
    }

    audit_log(agent, tool_name, "allowed");
    TrustCheckResult::Allowed
}

// ── Audit logging ──────────────────────────────────────────────────────

fn audit_log(agent: &AgentIdentity, tool: &str, result: &str) {
    info!(
        agent = %agent.name,
        tool = %tool,
        result = %result,
        "MCP tool invocation"
    );
}

fn audit_log_denied(agent: &AgentIdentity, tool: &str, reason: &str) {
    warn!(
        agent = %agent.name,
        tool = %tool,
        reason = %reason,
        "MCP tool invocation denied"
    );
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use therminal_core::config::AgentTrust;

    #[test]
    fn tier_ordering() {
        assert!(TrustTier::Sandboxed < TrustTier::Supervised);
        assert!(TrustTier::Supervised < TrustTier::Trusted);
    }

    #[test]
    fn has_access() {
        assert!(TrustTier::Trusted.has_access(TrustTier::Sandboxed));
        assert!(TrustTier::Trusted.has_access(TrustTier::Trusted));
        assert!(TrustTier::Supervised.has_access(TrustTier::Sandboxed));
        assert!(!TrustTier::Sandboxed.has_access(TrustTier::Supervised));
        assert!(!TrustTier::Supervised.has_access(TrustTier::Trusted));
    }

    #[test]
    fn tool_categories() {
        assert_eq!(
            tool_category("terminal.sessions.list"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.sessions.get"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.panes.list"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.panes.get_geometry"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.panes.get_content"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.semantic.query_history"),
            Some(ToolCategory::Observer)
        );
        assert_eq!(
            tool_category("terminal.sessions.create"),
            Some(ToolCategory::Writer)
        );
        assert_eq!(
            tool_category("terminal.panes.write"),
            Some(ToolCategory::Writer)
        );
        assert_eq!(
            tool_category("terminal.sessions.destroy"),
            Some(ToolCategory::Admin)
        );
        assert_eq!(tool_category("nonexistent"), None);
    }

    #[test]
    fn resolve_tier_default() {
        let config = TrustConfig::default();
        let agent = AgentIdentity {
            name: "unknown-agent".to_string(),
        };
        assert_eq!(agent.resolve_tier(&config), TrustTier::Supervised);
    }

    #[test]
    fn resolve_tier_per_agent() {
        let mut config = TrustConfig::default();
        config.agents.insert(
            "claude".to_string(),
            AgentTrust {
                tier: TrustTier::Trusted,
                allowed_tools: None,
            },
        );
        let agent = AgentIdentity {
            name: "claude".to_string(),
        };
        assert_eq!(agent.resolve_tier(&config), TrustTier::Trusted);
    }

    #[test]
    fn rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(3);
        assert!(limiter.check_and_record("agent1").is_ok());
        assert!(limiter.check_and_record("agent1").is_ok());
        assert!(limiter.check_and_record("agent1").is_ok());
        assert!(limiter.check_and_record("agent1").is_err());
        // Different agent is independent.
        assert!(limiter.check_and_record("agent2").is_ok());
    }

    #[test]
    fn rate_limiter_zero_is_unlimited() {
        let limiter = RateLimiter::new(0);
        for _ in 0..100 {
            assert!(limiter.check_and_record("agent").is_ok());
        }
    }

    #[test]
    fn check_tool_access_sandboxed_can_read() {
        let config = TrustConfig {
            default_tier: TrustTier::Sandboxed,
            ..TrustConfig::default()
        };
        let agent = AgentIdentity {
            name: "test".to_string(),
        };
        let limiter = RateLimiter::new(5);
        assert!(matches!(
            check_tool_access("terminal.sessions.list", &agent, &config, &limiter),
            TrustCheckResult::Allowed
        ));
    }

    #[test]
    fn check_tool_access_sandboxed_cannot_write() {
        let config = TrustConfig {
            default_tier: TrustTier::Sandboxed,
            ..TrustConfig::default()
        };
        let agent = AgentIdentity {
            name: "test".to_string(),
        };
        let limiter = RateLimiter::new(5);
        assert!(matches!(
            check_tool_access("terminal.sessions.create", &agent, &config, &limiter),
            TrustCheckResult::Denied(_)
        ));
    }

    #[test]
    fn check_tool_access_supervised_cannot_destroy() {
        let config = TrustConfig {
            default_tier: TrustTier::Supervised,
            ..TrustConfig::default()
        };
        let agent = AgentIdentity {
            name: "test".to_string(),
        };
        let limiter = RateLimiter::new(5);
        assert!(matches!(
            check_tool_access("terminal.sessions.destroy", &agent, &config, &limiter),
            TrustCheckResult::Denied(_)
        ));
    }

    #[test]
    fn check_tool_access_trusted_can_destroy() {
        let config = TrustConfig {
            default_tier: TrustTier::Trusted,
            ..TrustConfig::default()
        };
        let agent = AgentIdentity {
            name: "test".to_string(),
        };
        let limiter = RateLimiter::new(5);
        assert!(matches!(
            check_tool_access("terminal.sessions.destroy", &agent, &config, &limiter),
            TrustCheckResult::Allowed
        ));
    }

    #[test]
    fn check_tool_access_rate_limited() {
        let config = TrustConfig {
            default_tier: TrustTier::Trusted,
            ..TrustConfig::default()
        };
        let agent = AgentIdentity {
            name: "test".to_string(),
        };
        let limiter = RateLimiter::new(1);
        assert!(matches!(
            check_tool_access("terminal.sessions.destroy", &agent, &config, &limiter),
            TrustCheckResult::Allowed
        ));
        assert!(matches!(
            check_tool_access("terminal.sessions.destroy", &agent, &config, &limiter),
            TrustCheckResult::Denied(_)
        ));
    }
}
