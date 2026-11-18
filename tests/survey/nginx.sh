# nginx: several workers, EPOLLEXCLUSIVE when accept_mutex is off, reload
mkdir -p /tmp/ng/www /run/nginx; echo hello-nginx > /tmp/ng/www/index.html
cat > /tmp/ng/nginx.conf <<'C'
worker_processes 4;
pid /tmp/ng/nginx.pid;
error_log /tmp/ng/error.log info;
events { worker_connections 256; accept_mutex off; }
http { access_log off; server { listen 127.0.0.1:8089; root /tmp/ng/www; } }
C
nginx -c /tmp/ng/nginx.conf; echo "start: $?"; sleep 0.5
for i in 1 2 3; do curl -s http://127.0.0.1:8089/; done
nginx -c /tmp/ng/nginx.conf -s reload; sleep 1; curl -s http://127.0.0.1:8089/
nginx -c /tmp/ng/nginx.conf -s quit; sleep 1
echo "== log"; grep -vE 'notice|info' /tmp/ng/error.log | head
