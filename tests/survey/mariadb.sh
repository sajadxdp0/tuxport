# mariadb: InnoDB uses Linux AIO (io_submit, io_getevents), and on shutdown io_destroy wakes the
# threads waiting for events
rm -rf /tmp/my; mkdir -p /tmp/my /run/mysqld; chown mysql:mysql /tmp/my /run/mysqld
timeout 300 mariadb-install-db --user=mysql --datadir=/tmp/my >/tmp/my-install.log 2>&1; echo "install-db: $?"; tail -2 /tmp/my-install.log
rm -f /tmp/my.err
mariadbd --user=mysql --datadir=/tmp/my --socket=/tmp/my.sock --skip-networking --log-error=/tmp/my.err &
pid=$!
for i in $(seq 60); do [ -S /tmp/my.sock ] && break; sleep 0.5; done
M="mariadb -S /tmp/my.sock -uroot"
$M -e "create database d; use d; create table d.t(id int primary key auto_increment, v varchar(64)) engine=innodb; insert into d.t(v) select md5(seq) from seq_1_to_50000; select count(*), count(distinct v) from d.t; show variables like 'innodb_use_native_aio'; show status like 'Innodb_buffer_pool_pages_data';" 2>&1
$M -e "start transaction; update d.t set v='x' where id < 100; rollback; select count(*) from d.t where v='x'" 2>&1
mariadb-admin -S /tmp/my.sock -uroot shutdown
for i in $(seq 60); do kill -0 $pid 2>/dev/null || break; sleep 0.5; done
kill -0 $pid 2>/dev/null && { echo "mariadbd did not exit after shutdown"; kill -9 $pid; } || echo "mariadbd exited"
echo "== mariadb log"; grep -iE 'error|warn|io_uring|aio' /tmp/my.err | head -10
