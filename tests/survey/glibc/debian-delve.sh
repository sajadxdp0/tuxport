# Debian (glibc): delve 1.24 debugging Go 1.24 programs. Breakpoints that stop in goroutines,
# listing goroutines, the call stack, local variables, single-stepping, hardware watchpoints; dlv
# debug building and debugging on the spot, a headless server plus dlv connect, attaching to a
# running process
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
if ! command -v dlv >/dev/null || ! command -v go >/dev/null; then
  apt-get update -q >/dev/null 2>&1
  apt-get install -y -q --no-install-recommends delve golang-go gcc libc6-dev >/tmp/apt-dlv.log 2>&1
  echo "apt-get install: $?"
fi
dlv version | head -2 | tail -1; go version
cd /tmp; rm -rf dv; mkdir dv; cd dv
export GOPATH=/tmp/dv/gopath GOCACHE=/tmp/dv/cache GOFLAGS=-mod=mod GOTOOLCHAIN=local
cat > go.mod <<'G'
module example.com/dv

go 1.24
G
cat > main.go <<'G'
package main

import (
	"fmt"
	"os"
	"sync"
	"time"
)

var counter int

func work(n int) int {
	s := 0
	for i := 0; i < n; i++ {
		s += i
	}
	return s
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "wait" {
		for {
			counter++
			time.Sleep(time.Millisecond)
		}
	}
	var wg sync.WaitGroup
	res := make([]int, 4)
	for g := 0; g < 4; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			res[g] = work(1000 * (g + 1))
		}(g)
	}
	wg.Wait()
	counter = res[0] + res[3]
	fmt.Println(res, counter)
}
G
s=$(date +%s)
go build -gcflags='all=-N -l' -o hello . ; echo "build: $? ($(( $(date +%s) - s ))s)"
# The addresses, goroutine numbers and pids delve prints differ every time
strip_addr() { sed -E 's/0x[0-9a-f]+/ADDR/g; s/Goroutine [0-9]+/Goroutine N/; s/goroutine\(([0-9]+)\)/goroutine(N)/; s/[Pp]rocess [0-9]+/process N/'; }
cat > bp.dlv <<'D'
break main.work
continue
goroutine
stack 1 -full
print n
next
next
locals
clearall
continue
exit
D
s=$(date +%s)
echo "== breakpoint in goroutine"
dlv exec ./hello --init bp.dlv --allow-non-terminal-interactive=true </dev/null >b.log 2>&1; echo "exit $? ($(( $(date +%s) - s ))s)"
strip_addr < b.log | grep -E '^> |Thread .* at|n = |s = |i = |^\[|exited with|Breakpoint 1 set' | sed 's/ (hits.*//' | head -12
echo "== goroutines"
cat > gr.dlv <<'D'
break main.work
continue
goroutines
threads
exit
D
dlv exec ./hello --init gr.dlv --allow-non-terminal-interactive=true </dev/null >g.log 2>&1; echo "exit $?"
strip_addr < g.log | grep -cE 'main\.main\.gowrap|main\.main\.func1|main\.work'
grep -cE '^\*? *Thread' g.log
echo "== watchpoint"
cat > wp.dlv <<'D'
break main.main
continue
watch -w main.counter
continue
print main.counter
clearall
continue
exit
D
dlv exec ./hello --init wp.dlv --allow-non-terminal-interactive=true </dev/null >w.log 2>&1; echo "exit $?"
strip_addr < w.log | grep -E 'Watchpoint|watchpoint|^[0-9]+$|exited with' | head -6
echo "== dlv debug"
cat > dd.dlv <<'D'
break main.go:15
continue
print s
exit
D
s=$(date +%s)
dlv debug --init dd.dlv --allow-non-terminal-interactive=true </dev/null >d.log 2>&1; echo "exit $? ($(( $(date +%s) - s ))s)"
strip_addr < d.log | grep -E '^> |^[0-9]+$' | head -3
echo "== headless and connect"
dlv exec ./hello --headless --listen=127.0.0.1:23456 --api-version=2 --accept-multiclient >hs.log 2>&1 &
hp=$!; sleep 3
cat > cn.dlv <<'D'
break main.work
continue
print n
clearall
continue
exit
D
echo y | dlv connect 127.0.0.1:23456 --init cn.dlv --allow-non-terminal-interactive=true >cn.log 2>&1; echo "exit $?"
strip_addr < cn.log | grep -E '^> |n = |^[0-9]+$' | head -3
kill $hp 2>/dev/null; wait $hp 2>/dev/null
head -2 hs.log | strip_addr
echo "== attach"
./hello wait & p=$!; sleep 1
cat > at.dlv <<'D'
print main.counter > 0
goroutines
exit
D
# exit asks whether to end the process, and answering n detaches explicitly. On EOF delve exits
# without detaching, and the kernel detaches when the tracer exits (Tuxport used to deliver the
# attach's SIGSTOP, leaving the process stopped, see round 95)
echo n | dlv attach $p --init at.dlv --allow-non-terminal-interactive=true >a.log 2>&1; echo "exit $?"
grep -E '^(true|false)$' a.log
case $(cut -d' ' -f3 /proc/$p/stat) in [Tt]) echo "stopped after detach" ;; *) echo "running after detach" ;; esac
kill $p; wait $p 2>/dev/null; echo "wait $?"
