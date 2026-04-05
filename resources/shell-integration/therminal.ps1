# Therminal shell integration for PowerShell
# Emits OSC 133 marks for prompt/command lifecycle and OSC 7 for cwd.
# Sourced automatically when TERM_PROGRAM=therminal is detected.

# Guard against double-sourcing.
if ($env:__THERMINAL_SHELL_INTEGRATION) {
    return
}
$env:__THERMINAL_SHELL_INTEGRATION = "1"

# -- OSC helpers ---------------------------------------------------------------

function __therminal_osc([string]$payload) {
    $esc = [char]27
    [Console]::Write("${esc}]${payload}${esc}\")
}

function __therminal_report_cwd {
    $cwd = (Get-Location).Path
    # Convert backslashes to forward slashes for the file:// URI.
    $cwd = $cwd -replace '\\', '/'
    # Ensure leading slash on Windows (C:/... -> /C:/...).
    if ($cwd -notmatch '^/') {
        $cwd = "/$cwd"
    }
    __therminal_osc "7;file://$([System.Net.Dns]::GetHostName())$cwd"
}

# -- Prompt hook ---------------------------------------------------------------

# PowerShell's prompt function is the single hook for both prompt rendering
# and post-command status. We wrap the existing prompt.

$__therminal_original_prompt = $null
if (Test-Path Function:\prompt) {
    $__therminal_original_prompt = Get-Item Function:\prompt
}

$__therminal_preexec_fired = $false

function prompt {
    $ec = $LASTEXITCODE
    if ($null -eq $ec) { $ec = 0 }

    # CommandFinished (D) — if a command ran.
    if ($script:__therminal_preexec_fired) {
        __therminal_osc "133;D;$ec"
        $script:__therminal_preexec_fired = $false
    }

    # Report cwd.
    __therminal_report_cwd

    # PromptStart (A).
    __therminal_osc '133;A'

    # Render the original prompt.
    $result = ""
    if ($null -ne $__therminal_original_prompt) {
        $result = & $__therminal_original_prompt
    } else {
        $result = "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }

    # PromptEnd (B).
    __therminal_osc '133;B'

    return $result
}

# -- PreExec via PSReadLine ----------------------------------------------------

# PSReadLine's AcceptLine handler lets us emit C before command execution.
if (Get-Module -Name PSReadLine -ErrorAction SilentlyContinue) {
    # Capture any existing custom Enter handler so we can chain it.
    $__therminal_original_accept_line = $null
    try {
        $handler = Get-PSReadLineKeyHandler -Key Enter -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($handler -and $handler.ScriptBlock) {
            $__therminal_original_accept_line = $handler.ScriptBlock
        }
    } catch {}

    Set-PSReadLineKeyHandler -Key Enter -ScriptBlock {
        $script:__therminal_preexec_fired = $true
        __therminal_osc '133;C'
        if ($null -ne $script:__therminal_original_accept_line) {
            & $script:__therminal_original_accept_line
        } else {
            [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
        }
    }
}
