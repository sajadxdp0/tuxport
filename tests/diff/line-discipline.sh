# difftest-files: line-discipline
# The terminal line discipline against Linux's n_tty, scenario by scenario, using the scenarios and
# trace generator recorded for Starnix (tests/diff/line-discipline, see its README): each one drives
# a PTY pair through writes, reads, termios changes, packet mode, flushes, FIONREAD, poll and
# closes, and its trace is printed one event per line. A scenario that fails (a timeout, an
# unexpected errno) prints only the error's last line, so the others still run.
cd /tmp/difftest-files/line-discipline
python3 - <<'PY'
import json
import subprocess
import sys

for name in json.load(open("scenarios_list.json")):
    out = "/tmp/ld-trace.json"
    r = subprocess.run(
        [sys.executable, "trace_generator.py", "--generate-trace",
         "--input", "scenarios/%s.json" % name, "--output", out],
        capture_output=True, text=True, timeout=300)
    print("==", name)
    if r.returncode != 0:
        lines = r.stderr.strip().splitlines()
        print("  error:", lines[-1] if lines else "exit %d" % r.returncode)
        continue
    for event in json.load(open(out))["events"]:
        print("  " + json.dumps(event, sort_keys=True))
PY
rm -f /tmp/ld-trace.json
