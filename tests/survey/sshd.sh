# sshd: host keys, public-key login (root and a regular user), PTY allocation, pipes, scp/sftp, port
# forwarding
ssh-keygen -A >/dev/null
id u >/dev/null 2>&1 || adduser -D -s /bin/sh u
passwd -u u >/dev/null 2>&1 || true
for h in /root /home/u; do
  mkdir -p $h/.ssh; chmod 700 $h/.ssh
done
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys
cp /root/.ssh/id_ed25519.pub /home/u/.ssh/authorized_keys
chown -R u:u /home/u/.ssh; chmod 600 /root/.ssh/authorized_keys /home/u/.ssh/authorized_keys
rm -f /tmp/sshd.log
/usr/sbin/sshd -p 2222 -E /tmp/sshd.log -o PermitRootLogin=prohibit-password -o AllowTcpForwarding=yes
echo "sshd started: $?"
sleep 1
O="-p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR"
echo "== command"; ssh $O root@127.0.0.1 'echo hello; id -u; tty; echo $SSH_CONNECTION | cut -d" " -f1'
echo "== as user"; ssh $O u@127.0.0.1 'id; pwd; tty'
echo "== pty"; ssh $O -tt root@127.0.0.1 'tty; stty size; echo $TERM; exit 3' </dev/null; echo "exit=$?"
echo "== pty as user"; ssh $O -tt u@127.0.0.1 'tty; ls -l $(tty) | cut -d" " -f1,3,4; exit 3' </dev/null; echo "exit=$?"
echo "== stdin"; printf 'abc\ndef\n' | ssh $O root@127.0.0.1 'wc -l'
echo "== scp"; echo payload > /tmp/scp-src; scp -q -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes /tmp/scp-src root@127.0.0.1:/tmp/scp-dst && cat /tmp/scp-dst
echo "== sftp"; printf 'put /tmp/scp-src /tmp/sftp-dst\nls -l /tmp/sftp-dst\n' | sftp -q -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@127.0.0.1 >/dev/null && cat /tmp/sftp-dst
echo "== forward"; ssh $O -f -N -L 2223:127.0.0.1:2222 root@127.0.0.1; sleep 1; ssh -p 2223 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR root@127.0.0.1 'echo via forward'
pkill -f 'ssh .*-L 2223' 
kill $(cat /var/run/sshd.pid) 2>/dev/null
sleep 0.5
echo "== sshd log"; cat /tmp/sshd.log
