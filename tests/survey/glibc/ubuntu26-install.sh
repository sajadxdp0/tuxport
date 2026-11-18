# Ubuntu 26.04 (glibc 2.43, coreutils written in Rust): run this first to install the packages
# needed later.
# The store is exported from docker's ubuntu:26.04 (docs/rounds/149-ubuntu26-survey.md):
#
#   cid=$(docker create --platform linux/arm64 ubuntu:26.04)
#   docker export $cid | gzip > target/glibc/ubuntu-26-04.tar.gz; docker rm $cid
#   tuxport-run --rootfs target/glibc/ubuntu26-store --import target/glibc/ubuntu-26-04.tar.gz
export DEBIAN_FRONTEND=noninteractive
# In a rootfs from docker export, resolv.conf and hosts are empty (docs/rounds/69-glibc-probe.md)
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
[ -s /etc/hosts ] || printf '127.0.0.1 localhost\n::1 localhost ip6-localhost\n' > /etc/hosts
. /etc/os-release; echo "$PRETTY_NAME, $(ldd --version | head -1)"
apt-get update -q 2>&1 | tail -1
apt-get install -y -q --no-install-recommends python3 python3-venv gcc g++ libc6-dev make \
    procps psmisc ca-certificates curl git sudo openssh-client openssh-server tar xz-utils zstd \
    less vim-tiny file tmux screen iproute2 iputils-ping strace gdb util-linux bsdextrautils \
    findutils gawk locales man-db >/tmp/apt.log 2>&1
echo "apt-get install: $?"; tail -2 /tmp/apt.log
dpkg -l | grep -c '^ii'
for p in ls sudo tar; do printf '%s: ' $p; readlink -f "$(command -v $p)"; done
ls --version | head -1; sudo --version | head -1; tar --version | head -1
