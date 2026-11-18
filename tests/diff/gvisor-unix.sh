# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on AF_UNIX sockets: stream socket pairs of every kind (socketpair, path
# and abstract names, blocking or not), and bind, listen, accept and connect with both kinds of
# name. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/unix: buffer sizes are only recorded (a send buffer does not limit what is queued)
SKIP="AllUnixDomainSockets/StreamUnixSocketPairTest.SendBufferOverflow/*"
SKIP="$SKIP:AllUnixDomainSockets/StreamUnixSocketPairTest.IncreasedSocketSendBufUnblocksWrites/*"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" socket_unix_stream_test accept_bind_test
