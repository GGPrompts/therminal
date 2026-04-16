# Therminal shell integration for Fish
# Emits OSC 133 marks for prompt/command lifecycle and OSC 7 for cwd.
# Sourced automatically when TERM_PROGRAM=therminal is detected.

# Guard against double-sourcing.
if set -q __THERMINAL_SHELL_INTEGRATION
    return 0
end
set -g __THERMINAL_SHELL_INTEGRATION 1

# -- OSC helpers ---------------------------------------------------------------

function __therminal_osc
    printf '\e]%s\e\\' $argv[1]
end

function __therminal_report_cwd
    __therminal_osc "7;file://"(hostname)(pwd)
    # OSC 9;9: emit Windows-native path when running inside WSL so the
    # daemon can use it directly without linux_to_unc() (tn-kkr8).
    if set -q WSL_DISTRO_NAME
        set -l _wpath (wslpath -w (pwd) 2>/dev/null)
        and __therminal_osc "9;9;$_wpath"
    end
end

# -- Prompt hooks --------------------------------------------------------------

# Fish uses event-based hooks for preexec/postexec, and function wrapping for
# the prompt itself (to correctly bracket A...prompt-text...B in order).

set -g __therminal_preexec_fired 0

# Wrap the existing fish_prompt to emit PromptStart (A) before and
# PromptEnd (B) after the prompt text.  This is the only source of A/B marks.
if functions -q fish_prompt
    functions -c fish_prompt __therminal_original_fish_prompt
else
    function __therminal_original_fish_prompt
        printf '> '
    end
end

function fish_prompt
    __therminal_osc '133;A'
    __therminal_original_fish_prompt
    __therminal_osc '133;B'
end

# PreExec (C) — fires when the user submits a command.
function __therminal_preexec --on-event fish_preexec
    set -g __therminal_preexec_fired 1
    __therminal_osc '133;C'
end

# CommandFinished (D) — fires after command completes.
function __therminal_postexec --on-event fish_postexec
    set -l ec $status
    if test "$__therminal_preexec_fired" = "1"
        __therminal_osc "133;D;$ec"
        set -g __therminal_preexec_fired 0
    end
    __therminal_report_cwd
end
