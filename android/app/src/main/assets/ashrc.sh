# Written by Tuxport beside /etc/profile.d/tuxport.sh, which names this file in ENV, and replaced
# together with it.
#
# Every interactive busybox ash reads the file ENV names, a login shell after /etc/profile and
# ~/.profile. ash runs nothing before a prompt, so this puts the shell integration marks (OSC 133,
# see tuxport.sh) around whatever PS1 and PS2 are by then: a prompt ~/.profile set keeps its marks,
# and a nested ash (ash, su without -l) gets them too. A PS1 set later still goes without.
if [ -n "${BB_ASH_VERSION-}" ]; then
    # The prompt reports the last command's status (D) and starts on a fresh line (A, its cursor
    # moved with the arrows), and the typed command starts after it (B). ash says nothing when the
    # command ends, and breaks one longer than the screen's width into rows itself, so the prompts
    # ask Tuxport's terminal to read ash's line feeds (tuxport_lf=output): one ash breaks a line
    # with goes on with the command, and any other ends it and starts the output
    # Set the sanitized directory title after the visible prompt: ash's \w can print literal
    # controls from a path, which must not leave a different title behind.
    case ${PS1-} in
    *'133;A'*) ;;
    *) PS1='\[\e]133;D;$?\a\e]133;A;cl=line;tuxport_lf=output\a\]'${PS1-}'\[\e]133;B\a\e]2;${PWD//[[:cntrl:]]/?}\a\]' ;;
    esac
    case ${PS2-} in
    *'133;P'*) ;;
    *) PS2='\[\e]133;P;k=s;tuxport_lf=output\a\]'${PS2-}'\[\e]133;B\a\]' ;;
    esac
fi
