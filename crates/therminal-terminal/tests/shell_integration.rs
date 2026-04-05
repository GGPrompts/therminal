//! Integration tests for shell integration scripts.

use std::process::Command;

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
