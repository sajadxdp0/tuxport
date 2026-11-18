# CPython's own test suite (python3-tests), without the network; --rerun runs the failures once more
# on their own. The summary follows "== Tests result" at the end
cd /tmp; python3 --version
python3 -m test -j4 --timeout 1200 --rerun -u all,-network,-urlfetch,-gui,-largefile,-audio,-cpu 2>&1
echo "python -m test: $?"
