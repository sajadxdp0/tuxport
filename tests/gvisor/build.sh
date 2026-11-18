#!/bin/sh
# Runs inside the alpine:3.24 container of scripts/build-gvisor-tests, which passes in the
# environment the pinned versions, the programs to build ($programs, empty for all), the content of
# VERSION ($version) and the tarball's name ($tarball), and mounts this directory at /gvisor and
# the output at /out. Dependencies are built into /deps once (a rerun in the same container reuses
# them).
set -eu

apk add -q clang g++ make cmake linux-headers git curl >/dev/null

# The libraries gVisor's tests use, statically, built with the same compiler and standard as the
# tests (abseil's ABI depends on both)
cmake_flags="-DCMAKE_BUILD_TYPE=Release -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++
    -DCMAKE_CXX_STANDARD=20 -DBUILD_SHARED_LIBS=OFF -DCMAKE_INSTALL_PREFIX=/deps"
if [ ! -f /deps/.done ]; then
    mkdir -p /tmp/src && cd /tmp/src
    fetch() {
        curl -fsSL --retry 3 -o "$1.tar.gz" "$2"
        echo "$3  $1.tar.gz" | sha256sum -c - >/dev/null
        mkdir "$1" && tar xzf "$1.tar.gz" -C "$1" --strip-components 1
    }
    fetch absl "https://github.com/abseil/abseil-cpp/releases/download/$absl_version/abseil-cpp-$absl_version.tar.gz" "$absl_sha256"
    fetch gtest "https://github.com/google/googletest/releases/download/v$gtest_version/googletest-$gtest_version.tar.gz" "$gtest_sha256"
    fetch benchmark "https://github.com/google/benchmark/archive/refs/tags/v$benchmark_version.tar.gz" "$benchmark_sha256"
    # shellcheck disable=SC2086 # the flags must be split
    cmake -S absl -B absl-build $cmake_flags -DABSL_PROPAGATE_CXX_STD=ON -DABSL_BUILD_TESTING=OFF >/dev/null
    # shellcheck disable=SC2086
    cmake -S gtest -B gtest-build $cmake_flags >/dev/null
    # shellcheck disable=SC2086
    cmake -S benchmark -B benchmark-build $cmake_flags -DBENCHMARK_ENABLE_TESTING=OFF \
        -DBENCHMARK_ENABLE_WERROR=OFF >/dev/null
    for d in absl gtest benchmark; do
        cmake --build $d-build -j "$(nproc)" >/dev/null
        cmake --install $d-build >/dev/null
        cp $d/LICENSE /deps/LICENSE.$d
    done
    touch /deps/.done
fi

# gVisor's tests and test library at the pinned commit (git checks them by their hash)
if [ ! -d /tmp/gvisor ]; then
    mkdir /tmp/gvisor && cd /tmp/gvisor
    git init -q
    git remote add origin https://github.com/google/gvisor
    git sparse-checkout set --no-cone /LICENSE /test/syscalls/linux/ /test/util/
    git fetch -q --depth 1 --filter=blob:none origin "$commit"
    git checkout -q FETCH_HEAD
fi
cd /tmp/gvisor

flags="-std=c++20 -O1 -w -D_GNU_SOURCE -D_LARGEFILE64_SOURCE -I/gvisor/compat"
flags="$flags -include linux-compat.h -I. -I/deps/include"
export flags

# The test library: everything in test/util but its own tests, the Fuchsia and non-Linux variants,
# and the Bazel runfiles lookup (replaced by compat/runfiles.cc). errno_safe_allocator is left out:
# replacing musl's malloc in a static binary is not supported
rm -rf /tmp/util && mkdir /tmp/util
ls test/util/*.cc /gvisor/compat/runfiles.cc | grep -v -e '_test\.cc$' -e fuchsia_ \
    -e save_util_other -e test_util_runfiles -e errno_safe_allocator |
    xargs -P "$(nproc)" -I{} sh -c 'clang++ $flags -c -o "/tmp/util/$(basename {} .cc).o" {}'
ar rcs /tmp/libgvisor_util.a /tmp/util/*.o

# The programs to build: those named in $programs (all test programs when it is empty) and every
# helper the tests run (the cc_binary targets whose names do not end in _test, such as the exec
# workloads)
awk -f /gvisor/parse-build.awk test/syscalls/linux/BUILD >/tmp/targets
if [ -n "$programs" ]; then
    echo "$programs" | tr ' ' '\n'
else
    awk '$1 == "cc_binary" && $2 ~ /_test$/ { print $2 }' /tmp/targets
fi | awk 'NF' >/tmp/selected
awk '$1 == "cc_binary" && $2 !~ /_test$/ { print $2 }' /tmp/targets >>/tmp/selected
sort -u -o /tmp/selected /tmp/selected

# The sources of a target: its own and those of the local libraries it depends on, directly or
# not. They are linked as objects: test-case libraries register their tests at startup, so they
# must not be left in an archive for the linker to drop
cat >/tmp/sources.sh <<"EOS"
awk -v n="$1" '
    { srcs[$2] = $3; deps[$2] = $4 }
    function walk(t,   d, i, k) {
        if (seen[t]++) return
        if (srcs[t] != "-") { k = split(srcs[t], s, ","); for (i = 1; i <= k; i++) print s[i] }
        if (deps[t] != "-") { k = split(deps[t], d, ","); for (i = 1; i <= k; i++) walk(d[i]) }
    }
    END { walk(n) }' /tmp/targets | sort -u
EOS
rm -rf /tmp/obj /tmp/stage /tmp/errors && mkdir -p /tmp/obj /tmp/stage/bin /tmp/errors
xargs -n 1 sh /tmp/sources.sh </tmp/selected | sort -u | xargs -P "$(nproc)" -I{} sh -c \
    'clang++ $flags -c -o /tmp/obj/{}.o test/syscalls/linux/{} 2>/tmp/errors/{}.compile || true'

cat >/tmp/one.sh <<"EOS"
# Links cc_binary $1
name=$1
if ! awk -v n="$name" '$2 == n && $3 != "-" { found = 1 } END { exit !found }' /tmp/targets; then
    echo "$name: no such target, or no sources for arm64"
    exit 0
fi
objs=""
for s in $(sh /tmp/sources.sh "$name"); do
    if [ ! -f "/tmp/obj/$s.o" ]; then
        echo "$name: $(grep -m1 -E "error" "/tmp/errors/$s.compile")"
        exit 0
    fi
    objs="$objs /tmp/obj/$s.o"
done
# shellcheck disable=SC2086
if ! clang++ -static -s -o "/tmp/stage/bin/$name" $objs /tmp/libgvisor_util.a -L/deps/lib \
    -Wl,--start-group -lgmock -lgtest -lbenchmark /deps/lib/libabsl_*.a -Wl,--end-group \
    -pthread >"/tmp/errors/$name.link" 2>&1; then
    echo "$name: $(grep -m1 -E "error|undefined" "/tmp/errors/$name.link")"
fi
EOS
xargs -P "$(nproc)" -n 1 sh /tmp/one.sh </tmp/selected | sort >/tmp/stage/SKIPPED.txt
(cd /tmp/stage/bin && ls) >/tmp/stage/BUILT.txt
echo "$version" >/tmp/stage/VERSION
cp LICENSE /tmp/stage/LICENSE.gvisor
cp /deps/LICENSE.* /tmp/stage/
tar -C /tmp/stage -cf "/out/$tarball" .
echo "built $(wc -l </tmp/stage/BUILT.txt), skipped $(wc -l </tmp/stage/SKIPPED.txt)"
