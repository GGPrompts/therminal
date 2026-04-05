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
end

# -- Prompt hooks --------------------------------------------------------------

# Fish uses event-based hooks: fish_prompt, fish_preexec, fish_postexec.

set -g __therminal_preexec_fired 0

# Wrap fish_prompt to emit PromptStart (A) before and PromptEnd (B) after.
# We use the fish_prompt event rather than replacing the function.

function __therminal_fish_prompt --on-event fish_prompt
    __therminal_osc '133;A'
end

function __therminal_fish_prompt_end --on-event fish_prompt
    # This fires after the prompt function returns. We emit B here.
    # Note: fish_prompt event fires once; we rely on the prompt function
    # outputting between A and B. The B mark goes to stderr so it appears
    # after the prompt text.
    __therminal_osc '133;B' >&2
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

# Wrap the existing fish_prompt to inject B mark at the end.
# We save the original and define a wrapper.
if functions -q fish_prompt
    functions -c fish_prompt __therminal_original_fish_prompt
    function fish_prompt
        __therminal_osc '133;A'
        __therminal_original_fish_prompt
        __therminal_osc '133;B'
    end
end
