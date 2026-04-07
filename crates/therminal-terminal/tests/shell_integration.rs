//! Integration tests for shell integration scripts.
//!
//! Tests marked `#[ignore]` require a real PTY and the corresponding shell
//! installed. Run them with `cargo test -- --ignored`.

use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

/// Source the bash integration script in a subshell and verify that OSC 133
/// marks appear in the PROMPT_COMMAND and PS1 output.
#[test]
fn bash_script_sets_guard_variable() {
    let script_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../resources/shell-integration/therminal.bash"
    );

    // Source the script in an interactive-like bash and check the guard var.
    let output = Command::new("bash")
        .args([
            "-c",
            &format!(
                // Set PS1 so the script has something to append to.
                // Then source and echo the guard variable.
                "export PS1='$ '; source '{}'; echo \"GUARD=$__THERMINAL_SHELL_INTEGRATION\"",
                script_path
            ),
        ])
        .output()
        .expect("failed to run bash");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("GUARD=1"),
        "expected __THERMINAL_SHELL_INTEGRATION=1, got: {}",
        stdout
    );
}

/// Verify the bash script installs __therminal_prompt_command in PROMPT_COMMAND.
#[test]
fn bash_script_installs_prompt_command() {
    let script_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../resources/shell-integration/therminal.bash"
    );

    let output = Command::new("bash")
        .args([
            "-c",
            &format!(
                "export PS1='$ '; source '{}'; echo \"PC=$PROMPT_COMMAND\"",
                script_path
            ),
        ])
        .output()
        .expect("failed to run bash");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("__therminal_prompt_command"),
        "PROMPT_COMMAND should contain __therminal_prompt_command, got: {}",
        stdout
    );
}

/// Verify double-sourcing is a no-op.
#[test]
fn bash_script_double_source_guard() {
    let script_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../resources/shell-integration/therminal.bash"
    );

    let output = Command::new("bash")
        .args([
            "-c",
            &format!(
                // Disable the DEBUG trap before echo to avoid OSC noise in output.
                "export PS1='$ '; source '{}'; source '{}'; trap - DEBUG; echo \"PC=$PROMPT_COMMAND\"",
                script_path, script_path
            ),
        ])
        .output()
        .expect("failed to run bash");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // PROMPT_COMMAND should contain __therminal_prompt_command only once.
    let pc_line = stdout.lines().find(|l| l.contains("PC=")).unwrap_or("");
    let pc_count = pc_line.matches("__therminal_prompt_command").count();
    assert_eq!(
        pc_count, 1,
        "double-source should not duplicate hook, PROMPT_COMMAND line: {}",
        pc_line
    );
}

// ── PTY integration tests ──────────────────────────────────────────────────
//
// These spawn a real PTY via `spawn_shell()` and read output to verify that
// OSC 133 marks are emitted by the auto-sourced integration scripts.
// They require the shell to be installed and a working PTY subsystem.

/// Helper: read from a PTY reader until a timeout, collecting all output.
fn read_pty_output(reader: &mut dyn Read, timeout: Duration) -> String {
    let mut buf = [0u8; 4096];
    let mut output = Vec::new();
    let start = Instant::now();

    // Set non-blocking reads via a small poll loop.
    loop {
        if start.elapsed() >= timeout {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
        // Give the shell a moment to produce output after first read.
        if !output.is_empty() && start.elapsed() >= Duration::from_millis(500) {
            break;
        }
    }
    String::from_utf8_lossy(&output).to_string()
}

/// Spawn a real bash PTY and verify OSC 133 marks are emitted,
/// confirming that the integration script was auto-sourced.
#[test]
#[ignore] // Requires real PTY + bash
fn pty_bash_emits_osc_133_marks() {
    // Only run if bash is available.
    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("bash not found, skipping");
        return;
    }

    // Temporarily set SHELL to bash to ensure spawn_shell picks it up.
    let orig_shell = std::env::var("SHELL").ok();
    // SAFETY: test-only env var mutation; these tests are #[ignore]d and run serially.
    unsafe { std::env::set_var("SHELL", "bash") };

    let result = therminal_terminal::pty::spawn_shell(80, 24);

    // Restore SHELL.
    match orig_shell {
        Some(s) => unsafe { std::env::set_var("SHELL", s) },
        None => unsafe { std::env::remove_var("SHELL") },
    }

    let (master, mut child) = result.expect("failed to spawn bash PTY");
    let mut reader = master
        .try_clone_reader()
        .expect("failed to clone PTY reader");

    // Wait for the prompt to render.
    let output = read_pty_output(&mut reader, Duration::from_secs(3));

    child.kill().ok();
    child.wait().ok();

    // Check for any OSC 133 mark (A=PromptStart, B=PromptEnd, C=PreExec, D=CommandFinished).
    // The specific marks emitted depend on timing, but at least one proves integration is active.
    assert!(
        output.contains("\x1b]133;"),
        "expected OSC 133 marks in bash PTY output (integration not sourced), got:\n{:?}",
        output
    );
}

/// Spawn a real zsh PTY and verify OSC 133 marks are emitted.
#[test]
#[ignore] // Requires real PTY + zsh
fn pty_zsh_emits_osc_133_marks() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("zsh not found, skipping");
        return;
    }

    let orig_shell = std::env::var("SHELL").ok();
    unsafe { std::env::set_var("SHELL", "zsh") };

    let result = therminal_terminal::pty::spawn_shell(80, 24);

    match orig_shell {
        Some(s) => unsafe { std::env::set_var("SHELL", s) },
        None => unsafe { std::env::remove_var("SHELL") },
    }

    let (master, mut child) = result.expect("failed to spawn zsh PTY");
    let mut reader = master
        .try_clone_reader()
        .expect("failed to clone PTY reader");

    let output = read_pty_output(&mut reader, Duration::from_secs(3));

    child.kill().ok();
    child.wait().ok();

    assert!(
        output.contains("\x1b]133;"),
        "expected OSC 133 marks in zsh PTY output (integration not sourced), got:\n{:?}",
        output
    );
}

/// Spawn a real fish PTY and verify OSC 133 marks are emitted.
#[test]
#[ignore] // Requires real PTY + fish
fn pty_fish_emits_osc_133_marks() {
    if Command::new("fish").arg("--version").output().is_err() {
        eprintln!("fish not found, skipping");
        return;
    }

    let orig_shell = std::env::var("SHELL").ok();
    unsafe { std::env::set_var("SHELL", "fish") };

    let result = therminal_terminal::pty::spawn_shell(80, 24);

    match orig_shell {
        Some(s) => unsafe { std::env::set_var("SHELL", s) },
        None => unsafe { std::env::remove_var("SHELL") },
    }

    let (master, mut child) = result.expect("failed to spawn fish PTY");
    let mut reader = master
        .try_clone_reader()
        .expect("failed to clone PTY reader");

    let output = read_pty_output(&mut reader, Duration::from_secs(3));

    child.kill().ok();
    child.wait().ok();

    assert!(
        output.contains("\x1b]133;"),
        "expected OSC 133 marks in fish PTY output (integration not sourced), got:\n{:?}",
        output
    );
}

/// Verify that shell type detection works correctly for common paths.
#[test]
fn shell_type_detection() {
    use therminal_terminal::pty::{ShellType, detect_shell_type};

    assert_eq!(detect_shell_type("/bin/bash"), ShellType::Bash);
    assert_eq!(detect_shell_type("/usr/bin/zsh"), ShellType::Zsh);
    assert_eq!(detect_shell_type("/usr/bin/fish"), ShellType::Fish);
    assert_eq!(detect_shell_type("/usr/bin/pwsh"), ShellType::PowerShell);
    assert_eq!(detect_shell_type("/bin/sh"), ShellType::Unknown);
}
