# Debian (glibc): headless Chromium. The command line's --dump-dom, --screenshot and --print-to-pdf,
# then driving it with Puppeteer (through the DevTools protocol over WebSocket) and Playwright
# (through pipes on fds 3 and 4): JS in the page, fetch, Web Workers, canvas, typing and clicking,
# screenshots, PDF. Chromium is multi-process (browser, renderer, GPU, network service and so on),
# communicating through AF_UNIX sockets and shared memory, with V8 compiling just in time in the
# renderer
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
if ! command -v chromium >/dev/null || ! command -v npm >/dev/null || ! command -v python3 >/dev/null; then
  apt-get update -q >/dev/null 2>&1
  apt-get install -y -q --no-install-recommends chromium fonts-dejavu-core nodejs npm python3 file \
      >/tmp/apt-cr.log 2>&1
  echo "apt-get install: $?"
fi
chromium --version
cd /tmp; rm -rf cr; mkdir cr; cd cr
cat > page.html <<'H'
<!doctype html>
<html><head><title>t</title></head><body>
<input id="q"><button id="b" onclick="document.getElementById('out').textContent = 'hello ' + document.getElementById('q').value">go</button>
<div id="out"></div>
<canvas id="c" width="100" height="100"></canvas>
<script>
const ctx = document.getElementById('c').getContext('2d');
ctx.fillStyle = 'rgb(200, 0, 0)'; ctx.fillRect(10, 10, 50, 50);
document.title = 'js:' + [1, 2, 3].map(x => x * x).join(',');
function add(id, text) { const p = document.createElement('p'); p.id = id; p.textContent = text; document.body.appendChild(p); }
fetch('/data.json').then(r => r.json()).then(j => add('fetched', j.msg));
const w = new Worker(URL.createObjectURL(new Blob(['onmessage = e => postMessage(e.data * 2)'])));
w.onmessage = e => add('worker', 'worker ' + e.data);
w.postMessage(21);
</script></body></html>
H
echo '{"msg": "fetched ok"}' > data.json
python3 -m http.server 8123 --bind 127.0.0.1 >http.log 2>&1 &
hp=$!; sleep 1
U=http://127.0.0.1:8123/page.html
C="chromium --headless --no-sandbox --disable-gpu"
s=$(date +%s)
echo "== dump-dom"; $C --virtual-time-budget=5000 --dump-dom $U >dom.html 2>dom.err; echo "exit $? ($(( $(date +%s) - s ))s)"
grep -oE '<title>[^<]*</title>|<p id="[a-z]+">[^<]*</p>' dom.html
echo "== screenshot"; $C --screenshot=shot.png --window-size=640,480 $U >/dev/null 2>shot.err; echo "exit $?"
file shot.png | sed 's/^shot.png: //'
echo "== pdf"; $C --print-to-pdf=p.pdf --no-pdf-header-footer $U >/dev/null 2>pdf.err; echo "exit $?"
head -c 5 p.pdf; echo
s=$(date +%s)
npm install --no-audit --no-fund --loglevel=error puppeteer-core playwright-core >npm.log 2>&1
echo "npm install: $? ($(( $(date +%s) - s ))s)"
cat > pp.js <<'J'
const puppeteer = require('puppeteer-core');
(async () => {
  const browser = await puppeteer.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox', '--disable-gpu'] });
  console.log('version', (await browser.version()).split('/')[0]);
  const page = await browser.newPage();
  await page.goto(process.argv[2], { waitUntil: 'load' });
  await page.waitForSelector('#fetched');
  await page.waitForSelector('#worker');
  console.log('title', await page.title());
  console.log('fetched', await page.$eval('#fetched', e => e.textContent));
  console.log('worker', await page.$eval('#worker', e => e.textContent));
  await page.type('#q', 'tuxport');
  await page.click('#b');
  console.log('out', await page.$eval('#out', e => e.textContent));
  console.log('pixel', await page.evaluate(() =>
    Array.from(document.getElementById('c').getContext('2d').getImageData(20, 20, 1, 1).data).join(',')));
  const png = Buffer.from(await page.screenshot());
  console.log('screenshot', png.subarray(1, 4).toString(), png.length > 1000);
  const pdf = Buffer.from(await page.pdf());
  console.log('pdf', pdf.subarray(0, 5).toString());
  const second = await browser.newPage();
  await second.goto('data:text/html,<p id=x>second tab</p>');
  console.log('second', await second.$eval('#x', e => e.textContent), (await browser.pages()).length);
  await browser.close();
  console.log('closed');
})().catch(e => { console.error('error', e); process.exit(1); });
J
cat > pw.js <<'J'
const { chromium } = require('playwright-core');
(async () => {
  const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox', '--disable-gpu'] });
  const page = await (await browser.newContext()).newPage();
  await page.goto(process.argv[2]);
  await page.fill('#q', 'playwright');
  await page.click('#b');
  console.log('out', await page.textContent('#out'));
  console.log('worker', await page.textContent('#worker'));
  console.log('fetched', await page.textContent('#fetched'));
  await browser.close();
  console.log('closed');
})().catch(e => { console.error('error', e); process.exit(1); });
J
s=$(date +%s)
echo "== puppeteer"; timeout 300 node pp.js $U 2>pp.err; echo "exit $? ($(( $(date +%s) - s ))s)"
s=$(date +%s)
echo "== playwright"; timeout 300 node pw.js $U 2>pw.err; echo "exit $? ($(( $(date +%s) - s ))s)"
kill $hp; wait $hp 2>/dev/null
echo "chromium processes left: $(pgrep -c chromium)"
