# zig, python's venv and pip (network), socat
cd /tmp; rm -rf zigt; mkdir zigt; cd zigt
cat > hello.zig <<'Z'
const std = @import("std");
pub fn main() !void {
    var t = try std.Thread.spawn(.{}, struct { fn f() void {} }.f, .{});
    t.join();
    std.debug.print("zig hello {d}\n", .{@as(u32, 42)});
}
Z
echo "== zig $(zig version)"; s=$(date +%s); zig build-exe hello.zig 2>&1 | tail -3; echo "build: $? ($(( $(date +%s) - s ))s)"; ./hello
printf '#include <stdio.h>\nint main(){puts("zig cc ok");}\n' > c.c; zig cc -o c c.c 2>&1 | tail -2; ./c
echo "== pip"; rm -rf /tmp/venv; python3 -m venv /tmp/venv && /tmp/venv/bin/pip install -q --disable-pip-version-check requests 2>&1 | tail -2; /tmp/venv/bin/python -c 'import requests, sys; print("requests", requests.__version__)'
echo "== socat"; socat TCP-LISTEN:7001,reuseaddr,fork EXEC:'tr a-z A-Z' & sleep 0.5; echo hello | socat - TCP:127.0.0.1:7001; kill %1
socat -u UDP-RECV:7002 STDOUT & sleep 0.3; echo udp-ok | socat - UDP-SENDTO:127.0.0.1:7002; sleep 0.3; kill %1
