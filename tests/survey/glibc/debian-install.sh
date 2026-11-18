# Debian (glibc): apt installs the packages the later workloads need (from the network); run it
# before the other debian-* ones. The store is exported from a docker image
# (docs/rounds/69-glibc-probe.md); for Ubuntu, use ubuntu:24.04 instead:
#
#   c=$(docker create --platform linux/arm64 debian:trixie)
#   docker export $c | gzip -1 > target/glibc/debian-trixie.tar.gz; docker rm $c
#   scripts/dev target/linux-arm64/release/tuxport-run --rootfs target/glibc/debian-store \
#       --import target/glibc/debian-trixie.tar.gz
#   scripts/survey -s target/glibc/debian-store glibc/debian-install glibc/debian-c ...
export DEBIAN_FRONTEND=noninteractive
# In a rootfs from docker export, resolv.conf and hosts are empty (docker mounts them only at run
# time), and the kernel layer writes the DNS stub only when resolv.conf does not exist
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
[ -s /etc/hosts ] || printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n' > /etc/hosts
apt-get update -q 2>&1 | tail -2
apt-get install -y -q --no-install-recommends procps ca-certificates curl xz-utils git file locales \
    python3 python3-venv python3-pip gcc g++ make libc6-dev nodejs npm strace gdb \
    openjdk-21-jdk-headless >/tmp/apt.log 2>&1
echo "apt-get install: $?"; tail -3 /tmp/apt.log
dpkg -l | grep -c '^ii'
