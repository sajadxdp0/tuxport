# redis: event loop, background threads, writing the RDB after fork (copy-on-write), AOF, benchmark
cd /tmp; rm -f /tmp/redis.log /tmp/dump.rdb /tmp/appendonly*
redis-server --port 6380 --daemonize yes --dir /tmp --logfile /tmp/redis.log --save '' --io-threads 2; echo "start: $?"
sleep 0.5
C="redis-cli -p 6380"
$C ping; $C set k v; $C get k; $C incr c; $C lpush l a b c >/dev/null; $C lrange l 0 -1 | tr '\n' ' '; echo
$C debug populate 200000 >/dev/null; $C dbsize
$C bgsave; sleep 0.2; while [ "$($C info persistence | grep rdb_bgsave_in_progress | tr -d '\r' | cut -d: -f2)" = 1 ]; do sleep 0.2; done
$C info persistence | grep -E 'rdb_last_bgsave_status|rdb_last_cow' | tr -d '\r'
ls -la /tmp/dump.rdb | awk '{print $5}'
$C config set appendonly yes >/dev/null; sleep 2; $C info persistence | grep -E 'aof_enabled|aof_last_bgrewrite_status' | tr -d '\r'
redis-benchmark -p 6380 -q -n 50000 -t set,get,lpush,lpop 2>&1 | tr '\r' '\n' | grep 'requests per second'
$C shutdown nosave 2>&1
sleep 0.5
echo "== log"; grep -iE 'warn|error|fail|#' /tmp/redis.log | head -20
