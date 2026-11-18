# postgres: initdb, startup, transactions, indexes, parallel queries (dynamic shared memory),
# LISTEN/NOTIFY, pgbench
D=/tmp/pg
pkill postgres 2>/dev/null; sleep 0.5
rm -rf $D; mkdir -p $D; chown postgres:postgres $D
su postgres -c "initdb -D $D/data -A trust -U postgres >$D/initdb.log 2>&1"; echo "initdb: $?"; tail -3 $D/initdb.log
su postgres -c "pg_ctl -D $D/data -l $D/log -o '-k $D -c listen_addresses=127.0.0.1 -p 5433' -w start" ; echo "start: $?"
P="psql -X -q -h $D -p 5433 -U postgres"
$P -c "create table t(id serial primary key, v text, n int); insert into t(v, n) select md5(i::text), i % 100 from generate_series(1, 200000) i; create index on t(v);" ; echo "load: $?"
$P -At -c "select count(*), count(distinct n) from t"
$P -At -c "set parallel_setup_cost = 0; set parallel_tuple_cost = 0; set min_parallel_table_scan_size = 0; set max_parallel_workers_per_gather = 2; explain (analyze, costs off, timing off, summary off) select n, count(*) from t group by n" | grep -i 'workers'
$P -At -c "set parallel_setup_cost = 0; set parallel_tuple_cost = 0; set min_parallel_table_scan_size = 0; select sum(length(v)) from t"
$P -At -c "begin; update t set n = n + 1 where id < 100; savepoint a; delete from t where id < 10; rollback to a; commit; select count(*) from t where id < 100 and n > 0"
$P -At -c "vacuum analyze t" -c "select pg_size_pretty(pg_relation_size('t'))"
$P -c "listen ch" -c "select pg_sleep(1)" -c "select 1" | grep -i notification &
sleep 0.3; $P -c "notify ch, 'hi'"; wait
pgbench -h $D -p 5433 -U postgres -q -i -s 2 postgres >/dev/null 2>&1; echo "pgbench init: $?"
pgbench -h $D -p 5433 -U postgres -c 4 -j 2 -T 10 postgres 2>&1 | grep -E 'tps|failed|number of transactions actually'
su postgres -c "pg_ctl -D $D/data -m fast -w stop"; echo "stop: $?"
echo "== log"; grep -vE 'LOG:  (checkpoint|database system|listening|starting|redirecting|shutting|background)' $D/log | head -30
