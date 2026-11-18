# valkey: like redis, on arm64 it checks smaps' Shared_Dirty after fork at startup
rm -f /tmp/valkey.log
rm -rf /tmp/valkey; mkdir -p /tmp/valkey
valkey-server --port 6390 --daemonize yes --dir /tmp/valkey --logfile /tmp/valkey.log --save ''; echo "start: $?"; sleep 0.5
valkey-cli -p 6390 ping; valkey-cli -p 6390 shutdown nosave 2>/dev/null
grep -E '#' /tmp/valkey.log | head -5
