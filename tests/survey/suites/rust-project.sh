# A big Rust project: ripgrep's release build and all its tests (the integration tests start rg
# hundreds of times)
cd /tmp; [ -d ripgrep ] || git clone -q --depth 1 https://github.com/BurntSushi/ripgrep
cd ripgrep || exit 1; git log -1 --format='ripgrep %h %cs'
s=$(date +%s); cargo build -q --release -j4 2>&1 | tail -3; echo "build: $? ($(( $(date +%s) - s ))s)"
s=$(date +%s); cargo test -q --release -j4 --workspace >/tmp/rg-test.log 2>&1; echo "test: $? ($(( $(date +%s) - s ))s)"
grep -E '^test result|FAILED|panicked' /tmp/rg-test.log | sort | uniq -c
./target/release/rg --version | head -1; ./target/release/rg -c fn crates/core | head -3
