# Full-screen programs: run htop, btop, fish, zsh and neovim (:terminal) in tmux and take
# screenshots of the display
export LANG=C.UTF-8
T="tmux -S /tmp/tmux-tui"
$T kill-server 2>/dev/null
$T new-session -d -s s -x 120 -y 40 -e TERM=xterm-256color htop
sleep 3; echo "== htop"; $T capture-pane -p -t s:0 | grep -v '^ *$' | head -6
$T new-window -t s: btop; sleep 4; echo "== btop"; $T capture-pane -p -t s:1 | grep -v '^ *$' | head -8
$T new-window -t s: fish; sleep 2; $T send-keys -t s:2 'echo fish $version; for i in 1 2 3; echo -n $i; end; echo' Enter; sleep 1
echo "== fish"; $T capture-pane -p -t s:2 | grep -v '^ *$' | tail -3
$T new-window -t s: 'zsh -f'; sleep 1; $T send-keys -t s:3 'echo zsh $ZSH_VERSION; print -l ${(s:,:)${:-a,b}}; jobs; sleep 0.1 &; wait; echo done' Enter; sleep 1
echo "== zsh"; $T capture-pane -p -t s:3 | grep -v '^ *$' | tail -4
$T new-window -t s: 'nvim --clean'; sleep 3
$T send-keys -t s:4 ':echo 6*7' Enter; sleep 2; echo "== nvim"; $T capture-pane -p -t s:4 | tail -1
$T send-keys -t s:4 ':terminal' Enter; sleep 4; $T send-keys -t s:4 'i'; sleep 1
$T send-keys -t s:4 'echo nvim-term-$((40+2)); tty' Enter; sleep 3
$T capture-pane -p -t s:4 | grep -E 'nvim-term-42|/dev/pts'
$T list-windows -F '#{window_index} #{pane_current_command} dead=#{pane_dead}'
$T kill-server
echo "== nvim headless"; nvim --clean --headless -c 'lua io.write("nvim " .. vim.version().major .. "." .. vim.version().minor .. " jobs=" .. vim.fn.system("echo hi")) ' -c 'qa' 2>&1
