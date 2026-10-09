//! Subprocesses used for background queries, never interactive shells.

use std::ffi::OsStr;
use std::process::Command;

/// Build a background command without attaching or allocating a Windows console.
/// Redirecting stdout alone does not prevent console creation. Keep this separate
/// from PTY launches, which deliberately provide an interactive console.
pub fn background_command(program: impl AsRef<OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    command
}

#[cfg(all(test, windows))]
mod tests {
    use super::background_command;

    #[test]
    fn background_child_has_no_console_and_preserves_piped_output() {
        let output = background_command("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                r#"Add-Type -Name ConsoleProbe -Namespace Therminal -MemberDefinition '[DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();'; [Console]::Out.WriteLine([Therminal.ConsoleProbe]::GetConsoleWindow().ToInt64()); [Console]::Error.WriteLine('stderr-preserved'); exit 7"#,
            ])
            .output()
            .expect("run Windows console probe");
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "stderr-preserved"
        );
    }
}
