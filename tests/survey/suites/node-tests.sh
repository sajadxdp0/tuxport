# test/parallel from Node.js's source package, run with the installed node (V8, ICU and OpenSSL in
# deps are not unpacked).
# Each failed test has a line "Path: parallel/<name>"
cd /tmp; v=$(node --version)
[ -d node-$v ] || curl -fsSL "https://nodejs.org/dist/$v/node-$v.tar.xz" | xz -dc | \
    tar x --exclude "node-$v/deps/v8" --exclude "node-$v/deps/icu-small" --exclude "node-$v/deps/openssl"
cd node-$v || exit 1
python3 tools/test.py --shell="$(command -v node)" -j4 -p dots --timeout=600 parallel 2>&1 | tail -n +1
echo "test.py: $?"
