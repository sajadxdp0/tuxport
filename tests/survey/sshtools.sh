# Tools over ssh: rsync, git (ssh protocol), mosh (UDP, with the client opened in tmux)
export LANG=C.UTF-8
ssh-keygen -A >/dev/null
mkdir -p /root/.ssh; chmod 700 /root/.ssh
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys; chmod 600 /root/.ssh/authorized_keys
cat > /root/.ssh/config <<'C'
Host local
  HostName 127.0.0.1
  Port 2222
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode yes
  LogLevel ERROR
C
rm -f /tmp/sshd2.log; /usr/sbin/sshd -p 2222 -E /tmp/sshd2.log; sleep 1
echo "== rsync"; rm -rf /tmp/rs; rsync -a /usr/share/apk/ local:/tmp/rs/ && diff -r /usr/share/apk /tmp/rs && echo "rsync same"
rsync -a --delete --stats /etc/ local:/tmp/rs-etc/ | grep -E 'Number of files:|Total transferred'
echo "== git"; rm -rf /tmp/repo.git /tmp/clone /tmp/clone2; git init -q --bare /tmp/repo.git
git clone -q local:/tmp/repo.git /tmp/clone && cd /tmp/clone && git config user.email a@b && git config user.name a && echo hi > f && git add f && git commit -qm init && git push -q origin HEAD 2>&1
git clone -q local:/tmp/repo.git /tmp/clone2 && cat /tmp/clone2/f && git -C /tmp/clone2 log --oneline | wc -l
cd /
echo "== git daemon"; git daemon --export-all --base-path=/tmp --port=9418 --detach --pid-file=/tmp/gd.pid; sleep 1; rm -rf /tmp/clone3; git clone -q git://127.0.0.1/repo.git /tmp/clone3 && cat /tmp/clone3/f; kill $(cat /tmp/gd.pid)
echo "== mosh"; T="tmux -S /tmp/tmux-mosh"; $T kill-server 2>/dev/null
$T new-session -d -s m -x 100 -y 30 "mosh --ssh='ssh -F /root/.ssh/config' local"; sleep 4
$T send-keys -t m 'echo mosh-$((40+2))' Enter; sleep 2
$T capture-pane -p -t m | grep -v '^ *$' | tail -3
$T send-keys -t m 'exit' Enter; sleep 2; $T kill-server 2>/dev/null
kill $(cat /var/run/sshd.pid) 2>/dev/null; sleep 0.5
echo "== sshd log"; grep -v 'Accepted\|disconnect\|Disconnected\|listening\|signal 15' /tmp/sshd2.log | head
