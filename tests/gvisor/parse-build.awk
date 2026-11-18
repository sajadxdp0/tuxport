# Prints one line per cc_binary or cc_library of a Bazel BUILD file (gVisor's
# test/syscalls/linux/BUILD), for scripts/build-gvisor-tests:
#   <kind> <name> <its .cc sources, comma-separated or -> <its local deps, comma-separated or ->
function flush() {
    if (kind != "") print kind, name, (srcs == "" ? "-" : srcs), (deps == "" ? "-" : deps)
    kind = ""; name = ""; srcs = ""; deps = ""; field = ""
}
/^(cc_binary|cc_library)\(/ { flush(); kind = substr($0, 1, index($0, "(") - 1); next }
/^\)/ { flush(); next }
kind == "" { next }
/^    [a-z_]+ = / { field = $1 }
# select_arch(): only the arm64 lists (each on one line) count; the default lists are all empty
/^ +(amd64|riscv64) = / { next }
{
    line = $0
    if (field == "name" && match(line, /"[^"]+"/)) name = substr(line, RSTART + 1, RLENGTH - 2)
    while (match(line, /"[^"]*"/)) {
        s = substr(line, RSTART + 1, RLENGTH - 2)
        line = substr(line, RSTART + RLENGTH)
        if (field == "srcs" && s ~ /\.cc$/) srcs = srcs (srcs == "" ? "" : ",") s
        if (field == "deps" && s ~ /^:/) deps = deps (deps == "" ? "" : ",") substr(s, 2)
    }
}
END { flush() }
