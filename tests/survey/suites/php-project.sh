# A big PHP project: composer creates a Laravel project (thousands of files), then its bundled
# tests, artisan, the built-in server
cd /tmp; rm -rf lara; s=$(date +%s)
composer create-project -q --no-interaction laravel/laravel lara >/tmp/composer.log 2>&1; echo "create-project: $? ($(( $(date +%s) - s ))s)"; tail -3 /tmp/composer.log
cd lara || exit 1; php artisan --version; find vendor -name '*.php' | wc -l
php artisan make:model -mf Post >/dev/null && php artisan migrate --force 2>&1 | tail -2
php artisan test 2>&1 | tail -4
php artisan serve --port=8001 >/tmp/serve.log 2>&1 &
for i in $(seq 30); do curl -fs -m 5 -o /dev/null http://127.0.0.1:8001/ && break; sleep 1; done
curl -s -m 30 http://127.0.0.1:8001/ | grep -o '<title>[^<]*'; curl -s -m 30 -o /dev/null -w 'up: %{http_code}\n' http://127.0.0.1:8001/up
pkill -f 'artisan serve'; pkill -f 'php.*-S 127.0.0.1:8001'; sleep 1
