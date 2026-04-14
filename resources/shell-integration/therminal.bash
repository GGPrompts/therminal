# Therminal shell integration for Bash
# Emits OSC 133 marks for prompt/command lifecycle and OSC 7 for cwd.
# Sourced automatically when TERM_PROGRAM=therminal is detected.

# Guard against double-sourcing.
if [[ -n "${__THERMINAL_SHELL_INTEGRATION:-}" ]]; then
    return 0
fi
__THERMINAL_SHELL_INTEGRATION=1

# Emit WSL-side shell PID so the daemon can scope agent detection to this
# pane's process subtree instead of scanning the entire distro (tn-ttie).
# OSC 7337 carries the PID as a plain decimal string; the daemon captures it
# via TherminalInterceptor and stores it on the Pane for tree-walking.
printf '\033]7337;%s\007' "$$"

# -- OSC helpers ---------------------------------------------------------------

__therminal_osc() {
    printf '\e]%s\e\\' "$1"
}

__therminal_report_cwd() {
    __therminal_osc "7;file://$(hostname)${PWD}"
}

# -- Prompt hooks --------------------------------------------------------------

# PromptStart (A) is emitted at the very beginning of PS1.
# PromptEnd (B) is emitted at the very end of PS1, right before user input.
# We wrap the existing prompt rather than replacing it.

__therminal_prompt_start() {
    __therminal_osc '133;A'
}

__therminal_prompt_end() {
    __therminal_osc '133;B'
}

# -- PreExec / CommandFinished -------------------------------------------------

# Use the DEBUG trap for PreExec (C) and PROMPT_COMMAND for CommandFinished (D).

__therminal_last_exit_code=0
__therminal_preexec_fired=0

__therminal_preexec() {
    # The DEBUG trap fires for every simple command. We only want to emit C once
    # per interactive command, not for PS1 evaluation or PROMPT_COMMAND itself.
    if [[ "${__therminal_preexec_fired}" == "1" ]]; then
        return
    fi
    # Skip if we're inside PROMPT_COMMAND.
    if [[ "${BASH_COMMAND}" == "__therminal_prompt_command" ]]; then
        return
    fi
    __therminal_preexec_fired=1
    __therminal_osc '133;C'
}

__therminal_prompt_command() {
    __therminal_last_exit_code=$?

    # CommandFinished (D) — only if a command actually ran.
    if [[ "${__therminal_preexec_fired}" == "1" ]]; then
        __therminal_osc "133;D;${__therminal_last_exit_code}"
    fi
    __therminal_preexec_fired=0

    # Report current working directory.
    __therminal_report_cwd

    # Emit PromptStart before the prompt renders.
    __therminal_prompt_start
}

# Install hooks.
# Prepend our prompt command so it runs first.
if [[ -z "${PROMPT_COMMAND:-}" ]]; then
    PROMPT_COMMAND="__therminal_prompt_command"
elif [[ "${PROMPT_COMMAND}" != *"__therminal_prompt_command"* ]]; then
    PROMPT_COMMAND="__therminal_prompt_command;${PROMPT_COMMAND}"
fi

# Append PromptEnd mark to PS1.
if [[ "${PS1}" != *'133;B'* ]]; then
    PS1="${PS1}\[$(__therminal_prompt_end)\]"
fi

# Install DEBUG trap for preexec, chaining any existing trap.
__therminal_existing_debug_trap=$(trap -p DEBUG | sed "s/trap -- '\\(.*\\)' DEBUG/\\1/")
if [[ -n "$__therminal_existing_debug_trap" ]]; then
    trap "$__therminal_existing_debug_trap; __therminal_preexec" DEBUG
else
    trap '__therminal_preexec' DEBUG
fi
