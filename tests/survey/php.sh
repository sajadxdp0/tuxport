# PHP: the command line, the built-in web server
php85 -r 'echo PHP_VERSION, " ", array_sum(range(1, 1000000)), " ", json_encode(["a" => 1]), " ", hash("sha256", "x"), PHP_EOL;'
php85 -r '$p = proc_open("echo child", [1 => ["pipe", "w"]], $pipes); echo trim(stream_get_contents($pipes[1])), " ", proc_close($p), PHP_EOL;'
mkdir -p /tmp/www; echo '<?php echo "hello ", $_GET["n"] ?? "", " ", getmypid() > 0 ? "pid" : "", "\n";' > /tmp/www/index.php
php85 -S 127.0.0.1:8088 -t /tmp/www >/tmp/php-server.log 2>&1 &
sleep 1
curl -s 'http://127.0.0.1:8088/?n=1'; curl -s 'http://127.0.0.1:8088/?n=2'
kill %1; wait
