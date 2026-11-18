# tmux: the server in the background, PTYs, unix sockets, windows and panes
T="tmux -S /tmp/tmux-survey"
$T kill-server 2>/dev/null
$T new-session -d -s s -x 100 -y 30 || echo "new-session failed: $?"
sleep 0.5
$T send-keys -t s 'echo hello from tmux; tty; stty size' Enter
sleep 1
echo "== pane"; $T capture-pane -p -t s | grep -v '^$'
$T split-window -t s 'echo second pane; sleep 30'
$T new-window -t s 'top -b -n 1 | head -3; sleep 30'
sleep 1
echo "== panes"; $T list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height} #{pane_current_command}'
$T resize-window -t s -x 60 -y 20 2>/dev/null
$T send-keys -t s:0.0 'stty size' Enter
sleep 1
echo "== resized"; $T capture-pane -p -t s:0.0 | grep -v '^$' | tail -2
$T kill-server; echo "kill-server: $?"
