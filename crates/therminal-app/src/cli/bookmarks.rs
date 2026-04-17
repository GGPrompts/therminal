//! `therminal bookmarks` subcommand (tn-co6n).
//!
//! Text-first bookmark surface: prints the `[[bookmarks]]` list from
//! `therminal.toml` as two-column `name  url` lines. URLs land in their
//! own column so the existing URL-hotspot regex picks them up cleanly
//! and they become clickable without any overlay code.
//!
//! An empty config is a success-with-no-output case so scripts that
//! pipe to `awk` / `jq` don't trip on missing data.

use anyhow::Result;
use clap::Args;

use therminal_core::config::{BookmarkEntry, TherminalConfig};

use super::OutputFlags;
use super::format::{write_json, write_tsv_row};

/// Args for `therminal bookmarks`.
#[derive(Args, Debug, Default)]
pub struct BookmarksArgs {
    /// Only show bookmarks whose `category` matches this value.
    #[arg(long)]
    pub category: Option<String>,
    #[command(flatten)]
    pub out: OutputFlags,
}

/// This subcommand is intentionally daemon-less — bookmarks live entirely
/// in `therminal.toml` and the command is a pure config read, so wiring it
/// through `CliCtx` would be wasteful (it would auto-spawn the daemon just
/// to print a list).
pub fn run(args: BookmarksArgs) -> Result<()> {
    let config = TherminalConfig::load();
    let filtered: Vec<&BookmarkEntry> = config
        .bookmarks
        .iter()
        .filter(|b| match &args.category {
            Some(want) => b.category.as_deref() == Some(want.as_str()),
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        // Empty config → no output, exit 0. The caller can detect this via
        // a zero-byte stdout without parsing errors.
        if args.out.json {
            // Still emit an empty JSON array so `jq` consumers see a valid
            // document instead of EOF on stdin.
            return write_json(&Vec::<serde_json::Value>::new());
        }
        return Ok(());
    }

    if args.out.json {
        let rows: Vec<serde_json::Value> = filtered
            .iter()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "url": b.url,
                    "icon": b.icon,
                    "category": b.category,
                })
            })
            .collect();
        return write_json(&rows);
    }

    // TSV: two load-bearing columns — `name` and `url`. The URL is in its
    // own column so hotspot detection picks it up without name-text
    // bleeding into the match. `icon` and `category` ride as optional
    // trailing fields so TSV consumers that only read fields 1 and 2 stay
    // robust.
    let mut stdout = std::io::stdout().lock();
    for b in &filtered {
        write_tsv_row(
            &mut stdout,
            [
                b.name.as_str(),
                b.url.as_str(),
                b.icon.as_deref().unwrap_or(""),
                b.category.as_deref().unwrap_or(""),
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(entries: Vec<BookmarkEntry>) -> TherminalConfig {
        TherminalConfig {
            bookmarks: entries,
            ..TherminalConfig::default()
        }
    }

    #[test]
    fn category_filter_matches_exact() {
        let c = cfg_with(vec![
            BookmarkEntry {
                name: "a".into(),
                url: "https://a".into(),
                icon: None,
                category: Some("x".into()),
            },
            BookmarkEntry {
                name: "b".into(),
                url: "https://b".into(),
                icon: None,
                category: Some("y".into()),
            },
            BookmarkEntry {
                name: "c".into(),
                url: "https://c".into(),
                icon: None,
                category: None,
            },
        ]);

        let want = Some("x".to_string());
        let matched: Vec<&BookmarkEntry> = c
            .bookmarks
            .iter()
            .filter(|b| match &want {
                Some(w) => b.category.as_deref() == Some(w.as_str()),
                None => true,
            })
            .collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "a");
    }

    #[test]
    fn category_none_means_all() {
        let c = cfg_with(vec![BookmarkEntry {
            name: "a".into(),
            url: "https://a".into(),
            icon: None,
            category: None,
        }]);

        let want: Option<String> = None;
        let matched: Vec<&BookmarkEntry> = c
            .bookmarks
            .iter()
            .filter(|b| match &want {
                Some(w) => b.category.as_deref() == Some(w.as_str()),
                None => true,
            })
            .collect();
        assert_eq!(matched.len(), 1);
    }
}
