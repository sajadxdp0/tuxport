# docker: dockerd + containerd + runc. The vfs storage driver, no bridge, no iptables; containers
# use host networking. The store must be created separately, not shared with podman's (Alpine's runc
# conflicts with crun):
#   scripts/survey -s target/survey/docker docker
# Native comparison: scripts/survey -n <an alpine image with docker installed> docker,
# SURVEY_DOCKER_OPTS=--privileged
exec 2>&1
[ -x /usr/bin/dockerd ] || apk add -q docker
mkdir -p /etc/docker
echo '{"storage-driver":"vfs","iptables":false,"ip6tables":false,"bridge":"none"}' > /etc/docker/daemon.json
step() { echo; echo "== $*"; }
HTTP_PROXY=${http_proxy:-} HTTPS_PROXY=${https_proxy:-} NO_PROXY=127.0.0.1,localhost dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
step version; docker version --format '{{.Client.Version}} {{.Server.Version}}'; runc --version | head -1
step info; docker info --format '{{.Driver}} {{.CgroupDriver}} {{.CgroupVersion}} {{.DefaultRuntime}}'
docker image inspect alpine:3.24 >/dev/null 2>&1 || docker pull -q alpine:3.24 >/dev/null
step run; docker run --rm --network host alpine:3.24 sh -c 'echo hello; cat /etc/alpine-release'
echo "exit=$?"
# BuildKit reads /proc/stat (prometheus/procfs) while building
step build; mkdir -p /tmp/ctx && printf 'FROM alpine:3.24\nRUN echo built >/built\nCMD cat /built\n' >/tmp/ctx/Dockerfile
docker build -q --network host -t ddemo /tmp/ctx >/dev/null && docker run --rm --network host ddemo
echo "exit=$?"; docker rmi -f ddemo >/dev/null; rm -rf /tmp/ctx
step dockerd errors; grep -E 'level=(error|fatal)' /tmp/dockerd.log | sed 's/^time="[^"]*" //; s/ id=[0-9a-f]*//' | cut -c1-300 | sort -u | head -10
kill $(pidof dockerd); sleep 3
