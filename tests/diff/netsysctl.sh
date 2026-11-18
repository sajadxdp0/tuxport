# docker-run-options: --privileged
# Network namespace sysctls (docs/rounds/177-net-sysctls.md): in a new network namespace, the
# permissions and default values of the commonly used entries under /proc/sys/net, writes (writing
# k integers changes the first k, too many or non-numbers are EINVAL, the congestion control
# algorithm must be one that exists, read-only ones cannot be written), and that each namespace
# has its own copy. The `sysctls:` of docker and compose write these inside the container (Tuxport
# used to lack these files, so the containers would not start). The listing of the whole directory
# is not compared: Linux 6.1 and 6.8 have different entries. Nor is the default maximum of
# tcp_rmem: 6.1 and 6.8 go up to 6 MB, newer kernels (CI's) up to 32 MB (all set by the memory
# size).
exec 2>&1
unshare -n sh -c '
cd /proc/sys/net
for f in core/somaxconn core/optmem_max ipv4/tcp_keepalive_time ipv4/tcp_keepalive_intvl \
    ipv4/tcp_keepalive_probes ipv4/tcp_fin_timeout ipv4/tcp_syncookies ipv4/tcp_rmem ipv4/tcp_wmem \
    ipv4/tcp_congestion_control ipv4/tcp_available_congestion_control ipv4/tcp_tw_reuse \
    ipv4/tcp_max_syn_backlog ipv4/tcp_ehash_entries ipv4/tcp_fastopen_key ipv4/ip_default_ttl \
    ipv4/ip_nonlocal_bind ipv4/ip_local_reserved_ports ipv6/bindv6only unix/max_dgram_qlen; do
    printf "%s %s: %s\n" "$f" "$(stat -c %a $f)" "$(cat $f | tr "\t" " ")"
done
w() {
    printf "%s" "$2" > "$1" 2>/tmp/err
    echo "write $1 <- $2: $? $(sed "s/.*: //" /tmp/err) -> $(cat $1 | tr "\t" " ")"
}
w ipv4/tcp_keepalive_time 600
w ipv4/tcp_rmem 8192
w ipv4/tcp_rmem "1 2 3"
w ipv4/tcp_rmem "1 2 3 4"
w ipv4/tcp_fin_timeout abc
w ipv4/tcp_congestion_control reno
w ipv4/tcp_congestion_control nosuchcc
w ipv4/ip_local_reserved_ports "8080,9000-9010"
w ipv4/tcp_ehash_entries 1
w ipv6/bindv6only 1
w unix/max_dgram_qlen 512
echo "== another namespace has its own"
unshare -n sh -c "cat ipv4/tcp_keepalive_time ipv4/tcp_congestion_control unix/max_dgram_qlen"
' | sed -E 's/ 131072 [0-9]+$/ 131072 MAX/'
