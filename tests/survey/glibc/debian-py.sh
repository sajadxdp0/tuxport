# Debian (glibc): python's venv, pip installing manylinux wheels (numpy, pandas), multithreading,
# multiprocessing
cd /tmp; rm -rf venv; python3 -m venv venv || exit 1
s=$(date +%s); venv/bin/pip install -q --disable-pip-version-check numpy pandas 2>&1 | tail -2; echo "pip: $? ($(( $(date +%s) - s ))s)"
venv/bin/pip list 2>/dev/null | grep -iE 'numpy|pandas'
venv/bin/python - <<'PY'
import numpy as np, pandas as pd, threading, multiprocessing as mp, sqlite3, ssl, hashlib, time
t = time.time()
a = np.random.default_rng(1).random((300, 300)); b = a @ a.T
print("numpy", np.__version__, b.shape, round(float(np.linalg.eigvalsh(b)[-1]), 3) > 0)
df = pd.DataFrame({"k": [i % 5 for i in range(10000)], "v": range(10000)})
print("pandas", pd.__version__, df.groupby("k").v.sum().tolist()[:2])
def sq(x): return x * x
with mp.Pool(4) as p: print("pool", sum(p.map(sq, range(1000))))
r = []; ts = [threading.Thread(target=lambda: r.append(sum(range(100000)))) for _ in range(4)]; [x.start() for x in ts]; [x.join() for x in ts]
print("threads", len(r), "sqlite", sqlite3.connect(":memory:").execute("select 1+1").fetchone()[0], "ssl", ssl.OPENSSL_VERSION.split()[1], "seconds", round(time.time() - t, 1))
PY
