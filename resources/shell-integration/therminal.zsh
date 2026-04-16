# Therminal shell integration for Zsh
# Emits OSC 133 marks for prompt/command lifecycle and OSC 7 for cwd.
# Sourced automatically when TERM_PROGRAM=therminal is detected.

# Guard against double-sourcing.
if [[ -n "${__THERMINAL_SHELL_INTEGRATION:-}" ]]; then
    return 0
fi
__THERMINAL_SHELL_INTEGRATION=1

# -- OSC helpers ---------------------------------------------------------------

__therminal_osc() {
    printf '\e]%s\e\\' "$1"
}

__therminal_report_cwd() {
    __therminal_osc "7;file://$(hostname)${PWD}"
    # OSC 9;9: emit Windows-native path when running inside WSL so the
    # daemon can use it directly without linux_to_unc() (tn-kkr8).
    if [[ -n "${WSL_DISTRO_NAME:-}" ]]; then
        local _wpath
        _wpath=$(wslpath -w "$PWD" 2>/dev/null) && __therminal_osc "9;9;$_wpath"
    fi
}

# -- Prompt hooks (via precmd / preexec) ---------------------------------------

# Zsh provides precmd and preexec hook arrays natively.

__therminal_last_exit_code=0

__therminal_precmd() {
    local ec=$?
    __therminal_last_exit_code=$ec

    # CommandFinished (D) — only if preexec fired (i.e., a command ran).
    if [[ -n "${__therminal_preexec_fired:-}" ]]; then
        __therminal_osc "133;D;${__therminal_last_exit_code}"
        unset __therminal_preexec_fired
    fi

    # Report cwd.
    __therminal_report_cwd

    # PromptStart (A).
    __therminal_osc '133;A'
}

__therminal_preexec() {
    __therminal_preexec_fired=1
    # PreExec (C).
    __therminal_osc '133;C'
}

# Install via hook arrays (avoids clobbering user hooks).
autoload -Uz add-zsh-hook
add-zsh-hook precmd  __therminal_precmd
add-zsh-hook preexec __therminal_preexec

# Append PromptEnd (B) to the prompt.
# Use %{ %} for zero-width sequences so Zsh counts columns correctly.
if [[ "${PROMPT}" != *'133;B'* ]]; then
    PROMPT="${PROMPT}%{$(__therminal_osc '133;B')%}"
fi
