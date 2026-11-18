# Debian (glibc): once Miniforge (conda-forge's conda) is installed, create an environment with
# numpy and compute something with it
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
command -v curl >/dev/null || { apt-get update -q >/dev/null 2>&1; apt-get install -y -q --no-install-recommends curl ca-certificates >/dev/null 2>&1; }
cd /tmp; s=$(date +%s)
if ! [ -x /opt/conda/bin/conda ]; then
  curl -fsSLo mf.sh https://github.com/conda-forge/miniforge/releases/latest/download/Miniforge3-Linux-aarch64.sh
  bash mf.sh -b -p /opt/conda > /tmp/mf.log 2>&1; echo "install: $? ($(( $(date +%s) - s ))s)"; tail -2 /tmp/mf.log
fi
/opt/conda/bin/conda --version
s=$(date +%s)
/opt/conda/bin/conda create -q -y -n t python=3.12 numpy > /tmp/create.log 2>&1; echo "create: $? ($(( $(date +%s) - s ))s)"; tail -3 /tmp/create.log
/opt/conda/envs/t/bin/python -c 'import numpy as np, sys; print(sys.version.split()[0], np.__version__, int(np.arange(1000).sum()), round(float(np.linalg.det(np.eye(3) * 2)), 6))'
s=$(date +%s)
/opt/conda/bin/conda run -n t python -c 'print("conda run ok")'; echo "conda run: $? ($(( $(date +%s) - s ))s)"
