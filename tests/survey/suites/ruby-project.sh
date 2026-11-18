# A big Ruby project: gem installs Rails (compiling C extensions), then a new app, scaffold,
# migrations, tests, starting the server
cd /tmp; s=$(date +%s)
gem install -N rails >/tmp/gem.log 2>&1; echo "gem install rails: $? ($(( $(date +%s) - s ))s)"; tail -2 /tmp/gem.log
rm -rf blog; s=$(date +%s)
rails new blog -q --skip-git --skip-docker --skip-kamal --skip-thruster --skip-javascript --skip-ci >/tmp/rails-new.log 2>&1; echo "rails new: $? ($(( $(date +%s) - s ))s)"; tail -3 /tmp/rails-new.log
cd blog || exit 1; bin/rails --version
bin/rails g scaffold Post title:string body:text -q >/dev/null && bin/rails db:migrate 2>&1 | tail -2
bin/rails test 2>&1 | tail -4
bin/rails server -p 3001 -b 127.0.0.1 >/tmp/rails-server.log 2>&1 &
for i in $(seq 60); do curl -fs -m 5 -o /dev/null http://127.0.0.1:3001/up && break; sleep 1; done
curl -s -m 30 -o /dev/null -w 'up: %{http_code}\n' http://127.0.0.1:3001/up
bin/rails runner 'Post.create!(title: "hello", body: "world"); puts Post.count'
curl -s -m 30 http://127.0.0.1:3001/posts | grep -c hello
kill $(cat tmp/pids/server.pid); sleep 2; tail -2 /tmp/rails-server.log
