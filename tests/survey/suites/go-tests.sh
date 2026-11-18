# Go standard library tests (-short; -count=1 so cached results from last time are not used), 4
# packages in parallel; prints only the failing parts and the number of passing packages. Alpine's
# go package removes the _test.go files from src, so the official build from go.dev (statically
# linked) is used as GOROOT
cd /tmp; v=$(go env GOVERSION)
[ -x go/bin/go ] || curl -fsSL "https://go.dev/dl/$v.linux-arm64.tar.gz" | tar xz
export GOROOT=/tmp/go PATH=/tmp/go/bin:$PATH; go version
go test -short -count=1 -p 4 std >/tmp/go-test.log 2>&1; echo "go test: $?"
grep -E '^(--- FAIL|FAIL|panic:|fatal error:)' /tmp/go-test.log
echo "ok packages: $(grep -c '^ok ' /tmp/go-test.log)"
grep -B2 -A25 -E '^--- FAIL' /tmp/go-test.log | head -400
