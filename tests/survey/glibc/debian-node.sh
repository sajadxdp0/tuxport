# Debian (glibc): the distro's node, npm installing a package with prebuilt glibc binaries (sharp),
# the official glibc build from nodejs.org
cd /tmp; rm -rf nt; mkdir nt; cd nt
node -e 'const os=require("os"); console.log("node", process.version, os.cpus().length, "cpus", (os.totalmem()/2**20|0), "MiB")'
npm init -y >/dev/null; s=$(date +%s); npm install --no-audit --no-fund sharp 2>&1 | tail -2; echo "npm: $? ($(( $(date +%s) - s ))s)"
node -e 'require("sharp")({create:{width:64,height:64,channels:3,background:"#f00"}}).png().toBuffer().then(b=>console.log("sharp png", b.length > 0, require("sharp").versions.vips))'
v=$(curl -fsSL https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt | sed -n 's/.*  \(node-v[0-9.]*-linux-arm64\.tar\.xz\)$/\1/p')
curl -fsSL "https://nodejs.org/dist/latest-v24.x/$v" | tar xJ && d=${v%.tar.xz}
./$d/bin/node -e 'const w=new (require("worker_threads").Worker)("require(\"worker_threads\").parentPort.postMessage(6*7)",{eval:true}); w.on("message",m=>{console.log("official node", process.version, "worker", m); process.exit(0)})'
./$d/bin/node ./$d/lib/node_modules/npm/bin/npm-cli.js --version
