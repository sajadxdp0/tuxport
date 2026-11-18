# Written by Tuxport beside /etc/profile.d/tuxport.sh, which reads this file, and replaced together
# with it.
#
# zsh's shell integration marks and titles (OSC 133 and OSC 2, see tuxport.sh). The profile sets
# the default prompt; these hooks wrap whichever prompt the user or a theme chooses afterwards.
# Alpine's zprofile reads /etc/profile in sh emulation. Register the hooks in local zsh mode so
# array operations work without changing the caller's options. Reading this file again keeps
# other hooks and registers each Tuxport hook only once.
() {
    emulate -L zsh
    # Before each prompt: the end of the last command with its status (D); the marks around PS1
    # (A: the prompt starts, its cursor moved with the arrows; B: the input starts) and PS2 are
    # added again whenever something replaced them (~/.zshrc, a theme), and are sent again at
    # each redraw, as zle redraws the whole prompt. The function moves itself last, to run after
    # the hooks of themes that set PS1 before each prompt
    __tuxport_precmd() {
        local ret=$?
        emulate -L zsh
        printf '\033]133;D;%s\007' "$ret"
        printf '\033]2;%s\007' "${PWD//[[:cntrl:]]/?}"
        case $PS1 in *'133;B'*) ;; *) PS1=$'%{\e]133;A;cl=line\a%}'$PS1$'%{\e]133;B\a%}' ;; esac
        case $PS2 in *'133;B'*) ;; *) PS2=$'%{\e]133;P;k=s\a%}'$PS2$'%{\e]133;B\a%}' ;; esac
        [[ $precmd_functions[-1] == __tuxport_precmd ]] ||
            precmd_functions=(${precmd_functions:#__tuxport_precmd} __tuxport_precmd)
    }
    # Before each command: the start of its output (C), then the literal submitted command.
    __tuxport_preexec() {
        emulate -L zsh
        printf '\033]133;C\007'
        printf '\033]2;%s\007' "${1//[[:cntrl:]]/?}"
    }
    precmd_functions=(${precmd_functions:#__tuxport_precmd} __tuxport_precmd)
    preexec_functions=(${preexec_functions:#__tuxport_preexec} __tuxport_preexec)
}
