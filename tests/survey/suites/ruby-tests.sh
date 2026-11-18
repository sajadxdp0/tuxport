# The tests in Ruby's source package (core, socket, fiber, fileutils), run with the installed ruby
cd /tmp; v=$(ruby -e 'print RUBY_VERSION')
[ -d ruby-$v ] || curl -fsSL "https://cache.ruby-lang.org/pub/ruby/${v%.*}/ruby-$v.tar.gz" | tar xz
cd ruby-$v || exit 1
# Time limits scaled up 5 times: the *_memory_leak tests have a 10-second limit, and Tuxport, about 3
# times slower, times out (docs/rounds/84-debt-checks.md); the native side is scaled up the same way
ruby -I tool/lib tool/test/runner.rb -j4 --timeout-scale 5 test/ruby test/socket test/fiber test/fileutils 2>&1
echo "runner: $?"
