# nmap: run as root it uses raw sockets by default (SYN scans, ICMP/ARP host discovery), which an
# app on Android cannot create; NMAP_UNPRIVILEGED makes it use connect scans instead
# (docs/rounds/152-nmap-unprivileged.md).
# Native comparison: `scripts/survey -n alpine:3.24 nmap`; adding
# SURVEY_DOCKER_OPTS=--cap-drop=NET_RAW makes it root without CAP_NET_RAW (like the app on Android)
[ -f /usr/share/nmap/nse_main.lua ] || apk add -q nmap nmap-nselibs nmap-scripts
command -v python3 >/dev/null || apk add -q python3
nmap --version | head -1
python3 -m http.server 2280 --bind 127.0.0.1 >/dev/null 2>&1 &
srv=$!
sleep 1
quiet() { grep -v '^Starting\|^Nmap done\|latency\|^Service detection\|^SF'; }
echo "== root, default"; nmap -n -p 2280,2281 127.0.0.1 2>&1 | quiet
echo "== NMAP_UNPRIVILEGED=1"; NMAP_UNPRIVILEGED=1 nmap -n -p 2280,2281 127.0.0.1 2>&1 | quiet
echo "== host discovery"; NMAP_UNPRIVILEGED=1 nmap -n -sn 127.0.0.1 2>&1 | quiet
echo "== service version"; NMAP_UNPRIVILEGED=1 nmap -n -sV -p 2280 127.0.0.1 2>&1 | quiet
echo "== login shell"; printf 'export NMAP_UNPRIVILEGED=1\n' > /tmp/nmap-profile.sh
ENV=/tmp/nmap-profile.sh sh -ic 'nmap -n -p 2280 127.0.0.1' 2>&1 | quiet
kill $srv
