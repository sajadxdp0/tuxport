//! End-to-end tests: import the Alpine minirootfs into an inode store and run busybox through
//! `ld-musl` (the exit criteria of M1b, M3a and M3b). Each test case runs under Tier 0 and under
//! Tier 1 separately.
//!
//! Interactive test cases run `sh -i` on an in-process PTY with `tuxport-run --pty`, send keys
//! through a pipe (^C and ^Z are just the bytes 0x03 and 0x1a), and wait until the expected text
//! appears in the output before sending the next step.
//!
//! The rootfs is downloaded into `target/rootfs/` by `scripts/fetch-rootfs`, or can be given with
//! the environment variable `TUXPORT_ROOTFS_TARBALL`. Without it these tests are skipped; with
//! `TUXPORT_REQUIRE_ROOTFS=1` (as CI sets) that becomes a failure.
//!
//! Test cases that need outside network (the M4 exit criteria: apk installing packages, git
//! cloning over HTTPS) run only when `TUXPORT_NETWORK_TESTS=1` is set, and the host's `http_proxy`
//! and `https_proxy` are passed to the guest.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TIERS: [&str; 2] = ["0", "1"];
const PATH: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin";

#[test]
fn busybox_lists_the_root_directory() {
    for (tier, out) in run(&["/bin/busybox", "ls", "/"]) {
        assert_ok(tier, &out);
        let names: Vec<&str> = stdout(&out).lines().collect();
        for dir in ["bin", "etc", "lib", "root", "tmp", "usr", "var"] {
            assert!(names.contains(&dir), "tier {tier}: {names:?}");
        }
    }
}

#[test]
fn applets_run_through_symlinks() {
    for (tier, out) in run(&["/bin/ls", "-l", "/bin/sh", "/etc"]) {
        assert_ok(tier, &out);
        assert!(
            stdout(&out).contains("/bin/sh -> /bin/busybox"),
            "tier {tier}: {}",
            stdout(&out)
        );
        assert!(
            stdout(&out).contains("os-release"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn cat_reads_files_and_follows_symlinks() {
    // /etc/os-release is a relative symbolic link to ../usr/lib/os-release
    for (tier, out) in run(&["/bin/cat", "/etc/os-release"]) {
        assert_ok(tier, &out);
        assert!(
            stdout(&out).contains("ID=alpine\n"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn uname_reports_the_abi_contract() {
    for (tier, out) in run(&["/bin/uname", "-srm"]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "Linux 6.1.0-tuxport aarch64\n", "tier {tier}");
    }
}

#[test]
fn checksums_agree_between_tiers() {
    let results = run(&[
        "/usr/bin/sha256sum",
        "/bin/busybox",
        "/lib/ld-musl-aarch64.so.1",
    ]);
    for (tier, out) in &results {
        assert_ok(tier, out);
        assert_eq!(
            stdout(out).lines().count(),
            2,
            "tier {tier}: {}",
            stdout(out)
        );
    }
    if let [(_, t0), (_, t1)] = &results[..] {
        assert_eq!(stdout(t0), stdout(t1));
    }
}

#[test]
fn shell_builtins_redirect_into_the_store() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "cd /tmp && f=redirect && echo hi > $f && echo more >> $f && \
         while read l; do echo line $l; done < $f && pwd",
    ]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "line hi\nline more\n/tmp\n", "tier {tier}");
    }
}

#[test]
fn files_and_directories_can_be_created() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let dir = format!("/tmp/mkdir-{tier}");
        let out = command(store, tier, &["/bin/mkdir", "-p", &format!("{dir}/a/b")])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let out = command(
            store,
            tier,
            &["/bin/cp", "/etc/hostname", &format!("{dir}/a/b/copy")],
        )
        .output()
        .unwrap();
        assert_ok(tier, &out);
        let out = command(store, tier, &["/usr/bin/find", &dir])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let expected = format!("{dir}\n{dir}/a\n{dir}/a/b\n{dir}/a/b/copy\n");
        assert_eq!(stdout(&out), expected, "tier {tier}");
    }
}

#[test]
fn stdin_reaches_the_guest() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut child = command(store, tier, &["/bin/sh", "-c", "read v; echo got $v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"typed\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "got typed\n", "tier {tier}");
    }
}

#[test]
fn idle_readers_of_host_stdin_sleep_through_other_traffic() {
    // Without --pty the guest's stdin is a host pipe: a process blocked in read waits only for the
    // host fd, and the shell's reads and writes on a FIFO do not wake it (its count of voluntary
    // context switches stays the same); after a write to stdin it reads as usual. Read with dd:
    // busybox's read builtin polls first, so it would not be testing a blocking read. Read and
    // write the FIFO with builtins only: musl's fork blocks all signals around clone, and a child
    // exiting then has SIGCHLD blocked by every thread, which still wakes all waiters
    let Some(store) = store() else {
        return;
    };
    // First wait for dd to stop in its read: it has exec'd into dd, waits in the kernel (S), and
    // its switch count has not changed for 0.2 seconds (it may still be starting while the full
    // suite runs in parallel). Read the status with shell builtins only, starting no process and
    // using no pipe
    let script = "mkfifo /tmp/hoststdin.fifo
        exec 3<&0 4<>/tmp/hoststdin.fifo
        rm /tmp/hoststdin.fifo
        dd bs=5 count=1 <&3 >/tmp/hoststdin.out 2>/dev/null &
        r=$!
        n() {
            while read k v w; do
                case $k in Name:) m=$v ;; State:) s=$v ;; voluntary_ctxt_switches:) c=$v ;; esac
            done </proc/$r/status
        }
        n; b=$c; t=0
        while sleep 0.2; n; [ \"$m $s $c\" != \"dd S $b\" ] && [ $t -lt 50 ]; do b=$c; t=$((t + 1)); done
        i=0
        while [ $i -lt 200 ]; do echo x >&4; read l <&4; i=$((i + 1)); done
        n
        echo \"woken $((c - b))\"
        wait $r
        echo \"reader got $(cat /tmp/hoststdin.out)\"
        rm /tmp/hoststdin.out";
    for tier in TIERS {
        let mut child = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let woken = lines.next().unwrap().unwrap();
        child.stdin.take().unwrap().write_all(b"typed\n").unwrap();
        let rest: Vec<String> = lines.map(Result::unwrap).collect();
        assert_eq!(child.wait().unwrap().code(), Some(0), "tier {tier}");
        assert_eq!(woken, "woken 0", "tier {tier}");
        assert_eq!(rest, ["reader got typed"], "tier {tier}");
    }
}

#[test]
fn environment_reaches_the_guest() {
    for (tier, out) in run(&["/usr/bin/env"]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), format!("{PATH}\n"), "tier {tier}");
    }
}

#[test]
fn exit_status_and_errors_are_propagated() {
    for (tier, out) in run(&["/bin/sh", "-c", "exit 7"]) {
        assert_eq!(out.status.code(), Some(7), "tier {tier}: {}", stderr(&out));
    }
    for (tier, out) in run(&["/bin/ls", "/nonexistent"]) {
        assert_eq!(out.status.code(), Some(1), "tier {tier}");
        assert!(
            stderr(&out).contains("No such file or directory"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn fatal_signals_write_core_files() {
    // With RLIMIT_CORE on, a process killed by SIGSEGV leaves an ELF core file (ET_CORE, aarch64)
    // in the current directory; with a limit of 0 nothing is written; %e, %s and %d in
    // core_pattern expand to the program name, the signal and the dumpable mode (1)
    let script = "mkdir -p /tmp/cores && cd /tmp/cores && rm -f core* && ulimit -c unlimited && \
                  sh -c 'kill -SEGV $$'; echo status $?; od -A n -t x1 -N 4 core; \
                  od -A n -t d2 -j 16 -N 4 core; rm core; ulimit -c 0; \
                  sh -c 'kill -SEGV $$'; echo status $?; test -e core && echo present || echo absent; \
                  ulimit -c unlimited; echo 'core.%e.%s.%d' > /proc/sys/kernel/core_pattern; \
                  sh -c 'kill -ABRT $$'; ls core.*; cat /proc/self/coredump_filter";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        let words: Vec<String> = shell(tier, &out)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            words,
            [
                "status",
                "139",
                "7f",
                "45",
                "4c",
                "46",
                "4",
                "183",
                "status",
                "139",
                "absent",
                "core.sh.6.1",
                "00000033"
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn every_kind_of_wait_releases_the_bkl_without_borrows() {
    // Every kind of system call that waits in the kernel waits once (tests/guest/waits.c): the
    // kernel layer of debug builds checks on releasing the BKL that no kernel object is borrowed
    // (crate::cell); if one is, it panics and tuxport-run terminates
    let (Some(store), Some(program)) = (store(), build_static("waits")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/waits && chmod +x /tmp/waits && /tmp/waits";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn tiocsti_needs_the_controlling_terminal_or_cap_sys_admin() {
    // TIOCSTI as Linux 6.1 checks it (tests/guest/tiocsti.c); the differential test cannot
    // compare the unprivileged cases, which newer distribution kernels refuse outright
    let (Some(store), Some(program)) = (store(), build_static("tiocsti")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/tiocsti && chmod +x /tmp/tiocsti && /tmp/tiocsti";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn answers_of_linux_6_1_that_newer_kernels_changed() {
    // Timerfd and signalfd reads, epoll_wait's error order, FUSE clones and loop block sizes as
    // Linux 6.1 answers them (tests/guest/linux61.c); the differential tests compare only what
    // CI's newer native kernel answers the same
    let (Some(store), Some(program)) = (store(), build_static("linux61")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/linux61 && chmod +x /tmp/linux61 && /tmp/linux61";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn kernel_log_is_empty_and_keeps_console_levels() {
    // A new kernel's log is empty (Tuxport writes no boot messages): dmesg prints nothing; the
    // console level dmesg -n changes is visible in /proc/sys/kernel/printk; with dmesg_restrict at
    // 1 ordinary users cannot read it, and only 0 and 1 can be written
    let script = "dmesg; echo rc=$?; dmesg -n 3; cat /proc/sys/kernel/printk; \
        echo '6 4 1' > /proc/sys/kernel/printk; cat /proc/sys/kernel/printk; \
        echo 1 > /proc/sys/kernel/dmesg_restrict; su -s /bin/sh nobody -c dmesg; echo rc=$?; \
        echo 2 > /proc/sys/kernel/dmesg_restrict 2>/dev/null || echo rejected; \
        echo 0 > /proc/sys/kernel/dmesg_restrict; su -s /bin/sh nobody -c dmesg; echo rc=$?";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_eq!(
            stdout(&out),
            "rc=0\n3\t4\t1\t7\n6\t4\t1\t7\nrc=1\nrejected\nrc=0\n",
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("klogctl: Operation not permitted"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn recorded_sysctls_keep_what_is_written() {
    // Sysctls Tuxport only records take what Linux 6.1's handlers take and read it back:
    // swtuxportss 0 to 200, drop_caches write-only, overcommit_memory 0 to 2, tainted only adds
    // flags (of the 19 defined), modules_disabled only goes to 1, modprobe a string;
    // randomize_va_space starts at 0, as nothing is randomized
    let script = "cd /proc/sys; \
        stat -c '%a %n' vm/drop_caches vm/mmap_rnd_bits kernel/tainted vm/overcommit_memory; \
        cat vm/swtuxportss kernel/overflowuid kernel/modprobe kernel/randomize_va_space; \
        echo 10 > vm/swtuxportss; echo 300 > vm/swtuxportss 2>/dev/null || echo rejected; \
        cat vm/swtuxportss; echo 3 > vm/drop_caches; cat vm/drop_caches 2>/dev/null || echo unread; \
        echo 1 > vm/overcommit_memory; cat vm/overcommit_memory; echo 0 > vm/overcommit_memory; \
        echo 2101249 > kernel/tainted; echo 1 > kernel/tainted; cat kernel/tainted; \
        echo 0 > kernel/modules_disabled 2>/dev/null || echo rejected; \
        echo /bin/true > kernel/modprobe; sysctl -n kernel.modprobe";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_eq!(
            stdout(&out),
            "200 vm/drop_caches\n600 vm/mmap_rnd_bits\n644 kernel/tainted\n\
             644 vm/overcommit_memory\n60\n65534\n/sbin/modprobe\n0\nrejected\n10\nunread\n1\n\
             4097\nrejected\n/bin/true\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn kmsg_records_reach_dmesg() {
    // Records written to /dev/kmsg can be read by dmesg (busybox's dmesg uses syslog(2)'s
    // read-all), and one without a trailing newline only once the next one arrives;
    // printk_devkmsg defaults to ratelimit: at most 10 records per open file in 5 seconds, with a
    // record of how many were dropped on close; on is unlimited, off cannot be opened; only root
    // can read /proc/kmsg, which shares its position with syslog reads and is not affected by
    // dmesg -c
    let script = "strip() { sed 's/\\[ *[0-9]*\\.[0-9]*\\] //'; }; \
        echo '<11>user err' > /dev/kmsg; printf 'no newline' > /dev/kmsg; dmesg -r | strip; \
        echo closes > /dev/kmsg; dmesg | strip; cat /proc/sys/kernel/printk_devkmsg; \
        for i in $(seq 12); do echo n$i; done > /dev/kmsg; dmesg | grep -c '] n[0-9]'; \
        dmesg | grep suppressed | strip; \
        echo on > /proc/sys/kernel/printk_devkmsg; \
        for i in $(seq 12); do echo m$i; done > /dev/kmsg; dmesg | grep -c '] m[0-9]'; \
        echo onx > /proc/sys/kernel/printk_devkmsg 2>/dev/null || echo rejected; \
        echo off > /proc/sys/kernel/printk_devkmsg; (echo x > /dev/kmsg) 2>/dev/null || echo off; \
        echo ratelimit > /proc/sys/kernel/printk_devkmsg; cat /proc/sys/kernel/printk_devkmsg; \
        ls -l /dev/kmsg | cut -c1-10; dmesg -c > /dev/null; dmesg | wc -l; \
        dd if=/proc/kmsg bs=4096 count=1 2>/dev/null | head -n 1 | strip; \
        su -s /bin/sh nobody -c 'cat /proc/kmsg' 2>/dev/null || echo denied";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_eq!(
            stdout(&out),
            "<11>user err\nuser err\nno newline\ncloses\nratelimit\n10\n\
             printk: sh: 2 output lines suppressed due to ratelimiting\n12\nrejected\noff\n\
             ratelimit\ncrw-r--r--\n0\n<11>user err\ndenied\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn io_priority_reaches_the_host_thread() {
    // After ionice, the host thread of the program it runs is set too (Android's app seccomp
    // allows ioprio_set): IDLE is copied as is, RT is lowered to BE at the same level
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        for (class, want) in [("-c3", 0x6000u16), ("-c1 -n2", 0x4002)] {
            let script = format!("ionice {class} sh -c 'echo ready; read x'");
            let mut child = command(store, tier, &["/bin/sh", "-c", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "ready\n", "tier {tier}");
            let prios: Vec<u16> = std::fs::read_dir(format!("/proc/{}/task", child.id()))
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .filter_map(tuxport_hal::thread_ioprio)
                .collect();
            child.stdin.take().unwrap().write_all(b"\n").unwrap();
            assert!(child.wait().unwrap().success(), "tier {tier}");
            assert!(
                prios.contains(&want) && prios.iter().all(|p| p >> 13 != 1),
                "tier {tier} {class}: {prios:x?}"
            );
        }
    }
}

#[test]
fn interface_counters_count_guest_traffic() {
    // After sending and receiving on the loopback, the lo counters in three places, /proc/net/dev,
    // sysfs and netlink, are equal, and they grow by exactly the payload (tests/guest/traffic.c)
    let (Some(store), Some(program)) = (store(), build_static("traffic")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/traffic && chmod +x /tmp/traffic && /tmp/traffic";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn core_files_have_the_linux_layout() {
    // A multithreaded process crashes: the core file's program headers, MADV_DONTDUMP, each
    // thread's notes, core_pattern, truncation by RLIMIT_CORE (tests/guest/coredump.c); the
    // program must be copied into the store to run
    let (Some(store), Some(program)) = (store(), build_static("coredump")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/coredump && chmod +x /tmp/coredump && /tmp/coredump";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn clones_share_data_until_written() {
    // FICLONE, FICLONERANGE and whole-file copy_file_range on store files (tests/guest/reflink.c):
    // a clone keeps its contents when either side is written, truncated or mapped shared, its
    // shared mappings made before follow it, it keeps its own times and outlives its source; the
    // differential test cannot compare this, ext4 having no reflink
    let (Some(store), Some(program)) = (store(), build_static("reflink")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/reflink && chmod +x /tmp/reflink && /tmp/reflink";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn unhandled_exceptions_are_reported_in_the_kernel_log() {
    // With debug.exception-trace set, each kind of synchronous exception that ends a process
    // unhandled is reported in the kernel log as arm64 Linux reports it, except under a handler
    // or a tracer, at most 10 times in 5 seconds (tests/guest/crashlog.c); the reports name the
    // program's file, so it is copied into the store to run
    let (Some(store), Some(program)) = (store(), build_static("crashlog")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/crashlog && chmod +x /tmp/crashlog && /tmp/crashlog";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn machines_can_start_with_exception_reports_on() {
    // The app starts its kernel with debug.exception-trace at 1 (MachineOptions::exception_trace)
    // rather than arm64 Linux's 0, and the guest can still turn it off
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Interpreter,
        rootfs: Some(store.to_path_buf()),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: true,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let id = machine.open_console(24, 80).unwrap();
    let script =
        "cd /proc/sys/debug; cat exception-trace; echo 0 > exception-trace; cat exception-trace";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    assert_eq!(read_console(&machine, id), "1\n0\n");
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
}

#[test]
fn console_tells_password_input() {
    // The app draws a lock for the cursor while a console reads lines without echoing them
    // (ICANON on, ECHO off), as programs read passwords (Machine::console_password_input); not in
    // non-canonical mode, where line editors turn echo off too
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Interpreter,
        rootfs: Some(store.to_path_buf()),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: false,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let id = machine.open_console(24, 80).unwrap();
    assert_eq!(machine.console_password_input(id), Ok(false));
    let script = "stty -echo; echo off; read x; stty -icanon; echo raw; read x; \
                  stty icanon echo; echo on; read x";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    // Each mark is printed after its stty has run
    let read_until = |mark: &str| {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&out).contains(mark) {
            let n = machine.read_console(id, &mut buf).unwrap();
            assert!(n > 0, "{}", String::from_utf8_lossy(&out));
            out.extend_from_slice(&buf[..n]);
        }
    };
    read_until("off");
    assert_eq!(machine.console_password_input(id), Ok(true));
    machine.write_console(id, b"secret\n").unwrap();
    read_until("raw");
    assert_eq!(machine.console_password_input(id), Ok(false));
    machine.write_console(id, b"x\n").unwrap();
    read_until("on");
    assert_eq!(machine.console_password_input(id), Ok(false));
    machine.write_console(id, b"x\n").unwrap();
    read_console(&machine, id);
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    machine.close_console(id).unwrap();
    assert_eq!(
        machine.console_password_input(id),
        Err(tuxport_kernel::errno::Errno::ENOENT)
    );
}

#[test]
fn console_tells_background_output() {
    // The app's terminal reads busybox ash's line feeds at its prompt to find where a command's
    // output starts, and must not take those of a job left in the background for the shell's
    // (Machine::read_console_run): output is read in runs by whether the console's foreground
    // process group wrote it
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Interpreter,
        rootfs: Some(store.to_path_buf()),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: false,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let id = machine.open_console(24, 80).unwrap();
    // With job control the job gets a process group of its own while the shell keeps the terminal
    let script = "set -m; echo one; echo job & wait; echo two";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let (n, background) = machine.read_console_run(id, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]);
        match runs.last_mut() {
            Some((run, b)) if *b == background => run.push_str(&text),
            _ => runs.push((text.into_owned(), background)),
        }
    }
    let want = [("one\r\n", false), ("job\r\n", true), ("two\r\n", false)];
    let got: Vec<(&str, bool)> = runs.iter().map(|(run, b)| (run.as_str(), *b)).collect();
    assert_eq!(got, want);
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    machine.close_console(id).unwrap();

    // An interactive shell, as in a tab: only the job's line is the background's; the typed
    // commands' echo, a foreground command's output and every prompt are the foreground's
    let id = machine.open_console(24, 80).unwrap();
    let argv = [b"sh".to_vec(), b"-i".to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    let (mut fore, mut back) = (String::new(), String::new());
    let mut read_until = |mark: &str, background: bool| {
        loop {
            let (n, b) = machine.read_console_run(id, &mut buf).unwrap();
            assert!(n > 0, "{fore:?} {back:?}");
            let run = if b { &mut back } else { &mut fore };
            run.push_str(&String::from_utf8_lossy(&buf[..n]));
            if b == background && run.contains(mark) {
                break;
            }
        }
    };
    machine.write_console(id, b"echo fore-$((1+1))\r").unwrap();
    read_until("fore-2\r\n", false);
    machine
        .write_console(id, b"(sleep 1; echo back-$((1+1))) &\r")
        .unwrap();
    read_until("back-2\r\n", true);
    machine.write_console(id, b"echo last-$((1+1))\r").unwrap();
    read_until("last-2\r\n", false);
    machine.write_console(id, b"exit\r").unwrap();
    while machine.read_console_run(id, &mut buf).unwrap().0 > 0 {}
    assert_eq!(back, "back-2\r\n", "{fore:?}");
    assert!(!fore.contains("back-2"), "{fore:?}");
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    machine.close_console(id).unwrap();
}

#[test]
fn each_step_of_finishing_a_process_survives_kernel_bugs() {
    // When a step of process finish hits a kernel-layer bug (simulated with the test hooks of debug
    // builds), only what that step does is lost, and the later steps are done all the same
    // (tests/guest/finish.c); a controlling terminal is needed, so the program is copied into the
    // store to run
    if !cfg!(debug_assertions) {
        return;
    }
    let (Some(store), Some(program)) = (store(), build_static("finish")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/finish && chmod +x /tmp/finish && /tmp/finish";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .env("TUXPORT_TEST_HOOKS", "1")
            .env("TUXPORT_ON_BUG", "kill")
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
        for step in [
            "releasing AIO contexts",
            "handing its children to init",
            "sending parent-death signals",
            "releasing SysV IPC",
            "releasing the controlling terminal",
            "reporting to its tracer",
            "handing it to its parent",
            "releasing its tracees",
            "signalling orphaned process groups",
        ] {
            let report = format!("was exiting ({step})");
            assert!(
                stderr(&out).contains(&report),
                "tier {tier}: {}",
                stderr(&out)
            );
        }
    }
}

#[test]
fn missing_or_non_executable_programs_fail_to_start() {
    for (tier, out) in run(&["/bin/no-such-program"]) {
        assert_eq!(out.status.code(), Some(127), "tier {tier}");
    }
    for (tier, out) in run(&["/etc/passwd"]) {
        assert_eq!(out.status.code(), Some(126), "tier {tier}");
    }
}

#[test]
fn file_permissions_apply_to_other_users() {
    // After su to nobody: root's 600 files cannot be read, /root cannot be entered, and other
    // users' files in the sticky /tmp cannot be deleted; new files belong to nobody
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "f=/tmp/dac-$$; umask 077; echo secret > $f; \
         su -s /bin/sh nobody -c \"id -u; cat $f || echo denied; ls /root || echo denied; \
         rm -f $f || echo denied; touch $f.n; stat -c %u:%a $f.n; rm $f.n\"; \
         chmod 644 $f; su -s /bin/sh nobody -c \"cat $f\"; rm $f",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "65534\ndenied\ndenied\ndenied\n65534:600\nsecret\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn user_namespace_of_an_unprivileged_user() {
    // nobody creates its own user and mount namespaces (busybox's unshare -r -m): it maps itself
    // to root and has capabilities inside, but no privilege over unmapped files (those of the real
    // root), and cannot create device nodes; it can mount a tmpfs in its own mount namespace,
    // invisible outside. The expected values are upstream Linux's behaviour: a differential test
    // cannot compare it, since Ubuntu's kernel.apparmor_restrict_unprivileged_userns allows no
    // capabilities in such a namespace
    let inside = "id -u; id -g; tr -s ' ' < /proc/self/uid_map; \
                  cat /etc/shadow 2>/dev/null || echo denied; \
                  chown root /tmp 2>/dev/null || echo eperm; stat -c %u:%g /; \
                  mount -t tmpfs t /mnt && touch /mnt/f && stat -c %u:%g /mnt/f; \
                  mknod /mnt/d c 1 3 2>/dev/null || echo eperm; \
                  grep -c ' /mnt ' /proc/self/mountinfo";
    let script = format!(
        "su -s /bin/sh nobody -c \"unshare -r -m sh -c \\\"{inside}\\\"\"; \
         grep -c ' /mnt ' /proc/self/mountinfo"
    );
    for (tier, out) in run(&["/bin/sh", "-c", &script]) {
        assert_eq!(
            stdout(&out),
            "0\n0\n 0 65534 1\ndenied\neperm\n65534:65534\n0:0\neperm\n1\n0\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn pipelines_connect_processes() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "ls / | wc -l; echo hello | tr a-z A-Z | cat; cat /etc/os-release | grep ^ID= | sort | uniq",
    ]) {
        let out = shell(tier, &out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].trim().parse::<u32>().unwrap() >= 10,
            "tier {tier}: {out}"
        );
        assert_eq!(&lines[1..], ["HELLO", "ID=alpine"], "tier {tier}");
    }
}

#[test]
fn subshells_and_command_substitution_fork() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "echo $(echo nested $(echo twice)); (exit 3); echo status $?; false; echo $?",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "nested twice\nstatus 3\n1\n",
            "tier {tier}"
        );
    }
}

#[test]
fn trap_handlers_run_on_signals() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "trap 'echo caught USR1' USR1; kill -USR1 $$; echo after; \
         trap 'echo term; exit 5' TERM; trap 'echo bye' EXIT; kill -TERM $$; echo not reached",
    ]) {
        // The same as the native Alpine busybox
        assert_eq!(
            stdout(&out),
            "caught USR1\nafter\nterm\nbye\n",
            "tier {tier}"
        );
        assert_eq!(out.status.code(), Some(5), "tier {tier}");
    }
    // A SIGTERM without a trap terminates the shell by the default action, and the EXIT trap does
    // not run
    for (tier, out) in run(&["/bin/sh", "-c", "trap 'echo bye' EXIT; kill $$; echo no"]) {
        assert_eq!(stdout(&out), "", "tier {tier}");
        assert_eq!(out.status.code(), Some(128 + 15), "tier {tier}");
    }
}

#[test]
fn find_exec_and_xargs_spawn_programs() {
    // busybox starts the commands of -exec and xargs with vfork
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "find /etc -name hostname -exec cat {} \\; ; echo /etc/hostname | xargs cat",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(out.lines().count(), 2, "tier {tier}: {out}");
        let first = out.lines().next().unwrap();
        assert!(out.lines().all(|l| l == first), "tier {tier}: {out}");
    }
}

#[test]
fn background_jobs_can_be_waited_for_and_killed() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "sleep 0.1 & echo started; wait; echo done; \
         sleep 10 & kill $!; wait $!; echo killed $?",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "started\ndone\nkilled 143\n",
            "tier {tier}"
        );
    }
}

#[test]
fn closed_pipes_stop_writers_with_sigpipe() {
    // yes writes forever: after head exits it can only be terminated by SIGPIPE
    for (tier, out) in run(&["/bin/sh", "-c", "yes | head -n 2; echo end"]) {
        assert_eq!(shell(tier, &out), "y\ny\nend\n", "tier {tier}");
    }
}

#[test]
fn scripts_run_through_their_interpreter() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "cd /tmp && printf '#!/bin/sh -e\\necho script $0 $1\\n' > s && chmod +x s && \
         ./s arg && exec ./s exec",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "script ./s arg\nscript ./s exec\n",
            "tier {tier}"
        );
    }
}

#[test]
fn files_can_be_renamed_linked_and_removed() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let script = format!(
            "cd /tmp && mkdir ns{tier} && cd ns{tier} && echo data > a && mv a b && \
             ln -s b sym && ln b hard && cat sym && stat -c '%h %a' b && chmod 600 b && \
             stat -c '%a' hard && rm b sym && cat hard && rm hard && cd .. && rmdir ns{tier} && \
             ls -d ns{tier} 2>&1"
        );
        let out = command(store, tier, &["/bin/sh", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "tier {tier}: {}", stderr(&out));
        assert_eq!(
            stdout(&out),
            format!("data\n2 644\n600\ndata\nls: ns{tier}: No such file or directory\n"),
            "tier {tier}"
        );
    }
}

#[test]
fn device_nodes_work() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "echo hidden > /dev/null; cat /dev/null; head -c 5 /dev/zero | od -An -tx1; \
         head -c 16 /dev/urandom | wc -c; echo x > /dev/full || echo full",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>(),
            ["00", "00", "00", "00", "00", "16", "full"],
            "tier {tier}: {out}"
        );
    }
}

#[test]
fn unimplemented_paths_are_logged_once() {
    // With TUXPORT_LOG=1 the first hit of each path the kernel layer does not implement is written
    // to stderr (crate::stub): names Linux would have under /proc, /sys and /dev, with the pid
    // replaced, and a device without a driver; a process, fd or CPU that does not exist is no gap
    let Some(store) = store() else {
        return;
    };
    let script = "for i in 1 2; do \
                  cat /proc/sys/kernel/no_such_sysctl /proc/self/no_such_entry \
                  /sys/devices/system/cpu/no_such_file /dev/no_such_device /proc/999999/status \
                  /proc/self/fd/99 /sys/devices/system/cpu/cpu99999/online 2>/dev/null; done; \
                  mknod /tmp/tun c 10 200 && cat /tmp/tun 2>/dev/null; rm -f /tmp/tun; true";
    for tier in TIERS {
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .env("TUXPORT_LOG", "1")
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let lines: Vec<&str> = stderr(&out)
            .lines()
            .filter_map(|l| l.strip_prefix("tuxport: unimplemented "))
            .map(|l| l.split(" (pid ").next().unwrap())
            .collect();
        for expected in [
            "missing /proc/sys/kernel/no_such_sysctl, first by cat",
            "missing /proc/<pid>/no_such_entry, first by cat",
            "missing /sys/devices/system/cpu/no_such_file, first by cat",
            "missing /dev/no_such_device, first by cat",
            "char device 10:200, first by cat",
        ] {
            let n = lines.iter().filter(|l| **l == expected).count();
            assert_eq!(n, 1, "tier {tier}: {expected} in {lines:?}");
        }
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("status") || l.contains("fd/") || l.contains("cpu9")),
            "tier {tier}: {lines:?}"
        );
    }
}

#[test]
fn procfs_describes_processes() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        // The background child may not have exec'd into sleep yet (comm is still sh), as on Linux:
        // wait for it first
        "sleep 5 & i=0; until ps -o comm | grep -qx sleep || [ $i -ge 200 ]; do i=$((i+1)); done; \
         ps -o pid,comm | grep -c sleep; grep ^Name /proc/self/status; \
         readlink /proc/self/exe; cat /proc/self/cmdline | tr '\\0' ' '; echo; \
         df / | tail -n 1 | grep -c ' /$'; grep -c ^processor /proc/cpuinfo | grep -vc ^0; \
         echo piped | cat /dev/stdin; kill $!",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "1",
                "Name:\tgrep",
                // As on Linux, exe is the path with symbolic links resolved
                "/bin/busybox",
                "cat /proc/self/cmdline ",
                "1",
                "1",
                "piped"
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn procfs_fd_paths_resolve_symlinks() {
    // As with Linux's d_path, /proc/<pid>/fd holds paths with symbolic links resolved: valgrind
    // reopens Debian's /lib/ld-linux-aarch64.so.1 (whose last component is a link) by it with
    // O_NOFOLLOW
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "d=/tmp/rp.$$; mkdir -p $d/real/sub; ln -s real $d/link; echo x > $d/real/f; \
         ln -s f $d/real/flink; ln -s new $d/dangling; \
         exec 3<$d/link/flink 4<$d/link/sub/../f 5>$d/dangling; cd $d/link; exec 6<flink; \
         for fd in 3 4 5 6; do readlink /proc/self/fd/$fd | sed \"s|^$d|D|\"; done; rm -rf $d",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["D/real/f", "D/real/f", "D/new", "D/real/f"],
            "tier {tier}"
        );
    }
}

#[test]
fn procfs_task_links_count_threads() {
    // As with Linux's proc_task_getattr, the link count of /proc/<pid>/task is 2 plus the number
    // of threads: Chromium's zygote uses it before fork to decide whether it is single-threaded,
    // and crashes when it is under 3
    for (tier, out) in run(&["/bin/sh", "-c", "stat -c %h /proc/self/task"]) {
        assert_eq!(shell(tier, &out), "3\n", "tier {tier}");
    }
}

#[test]
fn procfs_net_is_under_each_process() {
    // As on Linux, /proc/net is a link to self/net, and the files are under each process's (and
    // thread's) directory
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "readlink /proc/net; head -n 1 /proc/self/net/dev | cut -c1-6; \
         ls /proc/$$/task/$$/net | grep -cx tcp",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["self/net", "Inter-", "1"],
            "tier {tier}"
        );
    }
}

#[test]
fn procfs_marks_deleted_files() {
    // As with Linux's d_path, a file still open after deletion carries " (deleted)" in
    // /proc/<pid>/fd
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "d=/tmp/del.$$; echo x > $d; exec 3<$d; readlink /proc/self/fd/3 | sed \"s|^$d|D|\"; \
         rm $d; readlink /proc/self/fd/3 | sed \"s|^$d|D|\"",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["D", "D (deleted)"],
            "tier {tier}"
        );
    }
}

#[test]
fn sysfs_and_limits_describe_the_machine() {
    // busybox's nproc --all counts the cpu<n> under /sys/devices/system/cpu; /proc/self/limits
    // agrees with ulimit
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "n=$(grep -c ^processor /proc/cpuinfo); test $(nproc --all) = $n && echo nproc; \
         online=$(cat /sys/devices/system/cpu/online); \
         test $online = 0-$((n - 1)) -o $online = 0 -a $n = 1 && echo online; \
         ls /sys/devices/system/cpu/cpu0/topology | grep -c _list; \
         ulimit -n 777; grep 'open files' /proc/self/limits | tr -s ' '; \
         grep -c ' - sysfs ' /proc/self/mountinfo",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["nproc", "online", "5", "Max open files 777 777 files ", "1"],
            "tier {tier}"
        );
    }
}

#[test]
fn network_configuration_is_visible() {
    // netlink (ip), interface ioctls (ifconfig), /proc/net/dev, a resolv.conf pointing at the DNS
    // stub
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "ip addr show lo | grep -o 'inet 127.0.0.1/8'; ifconfig lo | grep -o 'addr:127.0.0.1'; \
         grep -c ' lo:' /proc/net/dev; cat /etc/resolv.conf",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "inet 127.0.0.1/8",
                "addr:127.0.0.1",
                "1",
                "nameserver 127.0.0.53"
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn routing_tables_are_visible() {
    // The local table is synthesised from the interfaces' addresses, and the rules are Linux's
    // default ones; ip route get for a local address goes through lo; /proc/net/route has a
    // header, and sockstat counts sockets (the main table depends on the host's network, so it is
    // not checked here)
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "ip route show table local | grep '^local 127.0.0.1 '; ip rule; ip route get 127.0.0.9; \
         head -c 5 /proc/net/route; echo; head -n 1 /proc/net/sockstat",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().map(str::trim_end).collect::<Vec<_>>(),
            [
                "local 127.0.0.1 dev lo scope host  src 127.0.0.1",
                "0:\tfrom all lookup local",
                "32766:\tfrom all lookup main",
                "32767:\tfrom all lookup default",
                "local 127.0.0.9 dev lo  src 127.0.0.1",
                "Iface",
                "sockets: used 0",
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn syslog_messages_travel_over_a_unix_socket() {
    // syslogd binds a datagram socket on /dev/log, and logger sends to it by path
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "rm -f /tmp/syslog.txt; syslogd -n -O /tmp/syslog.txt & sleep 0.5; \
         test -S /dev/log && echo socket; logger -t tuxport hello; sleep 0.5; kill $!; wait; \
         grep -o 'tuxport: hello' /tmp/syslog.txt",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["socket", "tuxport: hello"],
            "tier {tier}"
        );
    }
}

#[test]
fn flock_excludes_other_holders() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "flock /tmp/flock-test -c 'sleep 1' & sleep 0.3; flock -n /tmp/flock-test -c true; \
         echo busy=$?; wait; flock -n /tmp/flock-test -c true; echo free=$?",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["busy=1", "free=0"],
            "tier {tier}"
        );
    }
}

#[test]
fn stop_and_continue_signals() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        // Keep looking until the state has changed (at most 5 seconds): waiting only 0.2 seconds,
        // when the machine is busy the process has not yet gone back into nanosleep after
        // SIGCONT, and R is read (seen on CI)
        "st() { for i in $(seq 50); do s=$(grep State /proc/$p/status); \
         case \"$s\" in *\"$1\"*) break;; esac; sleep 0.1; done; echo \"$s\"; }; \
         sleep 30 & p=$!; kill -STOP $p; st stopped; \
         kill -CONT $p; st sleeping; kill $p; wait $p; echo $?",
    ]) {
        assert_eq!(
            stdout(&out),
            "State:\tT (stopped)\nState:\tS (sleeping)\n143\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

/// An interactive shell running on an in-process PTY.
struct Interactive {
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<Vec<u8>>>,
    /// Length of the output already matched: `expect` searches only after it.
    seen: usize,
    tier: &'static str,
}

const PROMPT: &str = "tuxport$ ";

impl Interactive {
    fn start(store: &Path, tier: &'static str) -> Interactive {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
            .args([
                "--tier",
                tier,
                "--pty",
                "--env",
                PATH,
                "--env",
                "TERM=xterm",
            ])
            .arg(format!("--env=PS1={PROMPT}"))
            .arg("--rootfs")
            .arg(store)
            .args(["--", "/bin/sh", "-i"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let mut s = Interactive {
            child,
            stdin,
            output,
            seen: 0,
            tier,
        };
        s.expect(PROMPT);
        s
    }

    fn send(&mut self, keys: &[u8]) {
        self.stdin.write_all(keys).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Waits until `text` appears in the new output.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            {
                let out = self.output.lock().unwrap();
                let rest = &out[self.seen..];
                if let Some(i) = rest.windows(text.len()).position(|w| w == text.as_bytes()) {
                    self.seen += i + text.len();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "tier {}: timed out waiting for {text:?}; output so far:\n{}",
                    self.tier,
                    String::from_utf8_lossy(&out)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Exits the shell, returning its exit code. Does not wait forever when the shell refuses to
    /// exit (for example when there are still stopped jobs).
    fn exit(mut self) -> Option<i32> {
        self.send(b"exit\n");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "tier {}: the shell did not exit; output so far:\n{}",
                self.tier,
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Interactive {
    /// Leaves no tuxport-run still waiting for input when a test case fails.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn end_of_piped_input_is_an_eof_on_the_terminal() {
    // With --pty stdin is a pipe: when input ends and the program has read the input sent in, a ^D
    // is sent as script does, and programs that end on reading EOF exit with it (they used to wait
    // forever). The output holds the input echoed by the line discipline, so cat's line appears
    // twice
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        for (args, input, want, times) in [
            (&["/bin/sh", "-i"][..], "echo $((6*7))\n", "42", 1),
            (&["/bin/cat"][..], "one\ntwo\n", "two", 2),
        ] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
                .args(["--tier", tier, "--pty", "--env", PATH, "--rootfs"])
                .arg(store)
                .arg("--")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut out = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            assert!(
                status.is_some_and(|s| s.success()) && out.matches(want).count() == times,
                "tier {tier}: {args:?} exited with {status:?}; output:\n{out}"
            );
        }
    }
}

#[test]
fn end_of_piped_input_survives_leaving_canonical_mode() {
    // The end of file sent after piped input (above) lands in canonical mode when the program is
    // between reads there, as a shell running a command is; the program then leaves canonical
    // mode before reading it, as the shell's line editor does, and must read the VEOF character
    // (tests/guest/eofmode.c) rather than the NUL a typed VEOF becomes on Linux, which the line
    // editor would insert into its line and then wait forever
    let Some(program) = build_static("eofmode") else {
        return;
    };
    for tier in TIERS {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
            .args(["--tier", tier, "--pty", "--"])
            .arg(&program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"x\n").unwrap();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("byte 04"),
            "tier {tier}: exited with {:?}; output:\n{text}",
            out.status
        );
    }
}

#[test]
fn interactive_shell_runs_on_a_terminal() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        s.send(b"echo $((6*7)) | cat; tty; stty size\n");
        s.expect("42\r\n");
        s.expect("/dev/pts/0\r\n");
        s.expect("24 80\r\n");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn ctrl_c_and_ctrl_z_control_the_foreground_job() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        // The child is already the foreground process group when it prints job-42; the printed
        // text must not appear on the command line, or the echo would match first. The job then
        // sits reading the terminal
        s.send(b"sh -c 'echo job-$((6*7)); while read x; do echo got-$x; done'\n");
        s.expect("job-42");
        s.send(b"\x1a");
        s.expect("Stopped");
        s.expect(PROMPT);
        s.send(b"jobs\n");
        s.expect("Stopped");
        s.expect(PROMPT);
        // ash's fg first prints the job's command, then hands it the terminal and lets it
        // continue. Pressing ^C as soon as the command shows, SIGINT may land on the shell itself:
        // the shell returns to the prompt, $? is 130 too, yet the job stays stopped and exit
        // refuses to exit (the same on native Linux). So wait until the job has read a line and is
        // really running in the foreground before pressing ^C. The command fg prints has its quotes
        // turned into double quotes and a backslash before `$`, so only its end is matched
        s.send(b"fg\n");
        s.expect("; done");
        s.send(b"ping\n");
        s.expect("got-ping");
        s.send(b"\x03");
        s.expect(PROMPT);
        s.send(b"echo status $?\n");
        s.expect("status 130");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn vi_edits_a_file() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        let file = format!("/tmp/vi-{tier}");
        s.send(format!("rm -f {file}; vi {file}\n").as_bytes());
        // Switch to the alternate screen and draw the status line
        s.expect("\x1b[?1049h");
        s.expect("100%");
        // vi tells the ESC key from escape sequences by whether bytes follow the ESC right away,
        // so they are sent separately
        s.send(b"ihello from vi");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"oline two");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b":wq\r");
        s.expect("\x1b[?1049l");
        s.expect(PROMPT);
        s.send(format!("cat {file}\n").as_bytes());
        s.expect("hello from vi\r\nline two\r\n");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn apk_installs_packages_and_git_clones_over_https() {
    if std::env::var_os("TUXPORT_NETWORK_TESTS").is_none_or(|v| v != "1") {
        eprintln!("skipping network test: set TUXPORT_NETWORK_TESTS=1 to enable it");
        return;
    }
    let Some(tarball) = find_tarball() else {
        panic!("rootfs tarball not found; run scripts/fetch-rootfs");
    };
    // Installing packages changes many files: use a store of its own, not affecting other test
    // cases
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("network-store");
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
        .arg("--rootfs")
        .arg(&dir)
        .arg("--import")
        .arg(&tarball)
        .output()
        .unwrap();
    assert!(out.status.success(), "import failed: {}", stderr(&out));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tuxport-run"));
    // Tier 0 is too slow, so run only under Tier 1
    cmd.args(["--tier", "1", "--env", PATH, "--env", "HOME=/root"]);
    for var in ["http_proxy", "https_proxy", "no_proxy"] {
        if let Ok(v) = std::env::var(var) {
            cmd.arg("--env").arg(format!("{var}={v}"));
        }
    }
    let out = cmd
        .arg("--rootfs")
        .arg(&dir)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "apk update && apk add git python3 && \
             git clone --depth 1 https://github.com/octocat/Hello-World.git /tmp/hello && \
             cat /tmp/hello/README && \
             python3 -c 'import asyncio, hashlib, sqlite3, ssl; print(\"python ok\")'",
        ])
        .output()
        .unwrap();
    let text = shell("1", &out);
    assert!(text.contains("Hello World!"), "{text}");
    assert!(text.ends_with("python ok\n"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn importing_into_an_existing_store_fails() {
    let (Some(store), Some(tarball)) = (store(), find_tarball()) else {
        return;
    };
    let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
        .arg("--rootfs")
        .arg(store)
        .arg("--import")
        .arg(tarball)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
}

#[test]
fn threads_max_limits_root_too() {
    // threads-max applies to root too: fork returns EAGAIN when the thread count is at the limit,
    // and busybox sh reports "can't fork" and exits; a value written may not be below 20
    let script = "echo 19 > /proc/sys/kernel/threads-max || echo rejected; \
                  echo 25 > /proc/sys/kernel/threads-max; cat /proc/sys/kernel/threads-max; \
                  i=0; while [ $i -lt 40 ]; do sleep 5 & i=$((i + 1)); done; echo not reached";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_eq!(
            stdout(&out),
            "rejected\n25\n",
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("can't fork"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn rlimit_stack_at_execve_sets_the_stack_and_layout() {
    // The initial stack maps only 128 KB more below the arguments; the arguments take at most 1/4
    // of the stack size limit (at least 128 KB); with an unlimited stack size the mmap area leaves
    // MAX_GAP free (ld-musl is mapped below 0x1555_5555_5000)
    let script = "grep stack /proc/self/maps; \
                  (ulimit -s 1024; /bin/busybox true $(seq 20000) 2>/dev/null || echo E2BIG; \
                   /bin/busybox true $(seq 10000) && echo small ok); \
                  /bin/busybox true $(seq 20000) && echo default ok; \
                  (ulimit -s unlimited; grep -m1 ld-musl /proc/self/maps); \
                  grep -m1 ld-musl /proc/self/maps";
    let start_of = |line: &str| u64::from_str_radix(line.split('-').next().unwrap(), 16).unwrap();
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        let text = shell(tier, &out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "tier {tier}: {text}");
        let (lo, hi) = lines[0].split_once(' ').unwrap().0.split_once('-').unwrap();
        let (lo, hi) = (
            u64::from_str_radix(lo, 16).unwrap(),
            u64::from_str_radix(hi, 16).unwrap(),
        );
        assert!(lines[0].ends_with("[stack]"), "tier {tier}: {text}");
        assert!(hi == 1 << 47 && hi - lo <= 256 << 10, "tier {tier}: {text}");
        assert_eq!(
            &lines[1..4],
            ["E2BIG", "small ok", "default ok"],
            "tier {tier}"
        );
        assert!(start_of(lines[4]) < 0x1555_5555_5000, "tier {tier}: {text}");
        assert!(start_of(lines[5]) > 0x7f00_0000_0000, "tier {tier}: {text}");
    }
}

#[test]
fn lowering_oom_score_adj_needs_cap_sys_resource() {
    // An ordinary user can raise oom_score_adj and lower it back to the lower bound, but not below
    // it; a value root writes becomes the new lower bound, inherited after su
    let f = "/proc/self/oom_score_adj";
    let script = format!(
        "su -s /bin/sh nobody -c 'echo 500 > {f}; cat {f}; echo 0 > {f}; \
         echo -1 > {f} || echo denied; cat {f}'; \
         echo -500 > {f}; su -s /bin/sh nobody -c 'cat {f}; echo -400 > {f}; \
         echo -500 > {f} && echo ok; echo -501 > {f} || echo denied'"
    );
    for (tier, out) in run(&["/bin/sh", "-c", &script]) {
        assert_eq!(
            shell(tier, &out),
            "500\ndenied\n0\n-500\nok\ndenied\n",
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("Permission denied"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn oom_killer_honours_oom_score_adj() {
    // Over the memory limit, first the process with oom_score_adj 1000 is killed (even though it
    // takes almost no memory), and if it is still over after that one exits, the awk that really
    // eats memory is killed. The rules for picking processes do not depend on the tier, so run only
    // under Tier 1: in a Tier 0 debug build, building a 128 MB string takes over ten seconds
    let Some(store) = store() else {
        return;
    };
    let script = "(echo 1000 > /proc/self/oom_score_adj && exec sleep 30) & sleep 0.2; \
                  awk 'BEGIN { s = \"x\"; for (i = 0; i < 27; i++) s = s s; print length(s) }'; \
                  echo awk $?; wait $!; echo sleep $?";
    let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
        .args([
            "--tier",
            "1",
            "--memory-limit",
            "128M",
            "--env",
            PATH,
            "--rootfs",
        ])
        .arg(store)
        .args(["--", "/bin/sh", "-c", script])
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "awk 137\nsleep 137\n", "{}", stderr(&out));
    let err = stderr(&out);
    let (sleep, awk) = (err.find("(sleep) rss"), err.find("(awk) rss"));
    assert!(sleep.is_some() && sleep < awk, "{err}");
}

#[test]
fn cgroup_memory_files_follow_the_oom_killer() {
    // /sys/fs/cgroup/memory.max is the limit --memory-limit gives (max with none); after the OOM
    // killer kills a process memory.events records it, and memory.peak has measured a usage over
    // the limit. Run only under Tier 1, for the same reason
    let Some(store) = store() else {
        return;
    };
    let run = |limit: &str, script: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
            .args([
                "--tier",
                "1",
                "--memory-limit",
                limit,
                "--env",
                PATH,
                "--rootfs",
            ])
            .arg(store)
            .args(["--", "/bin/sh", "-c", script])
            .output()
            .unwrap();
        stdout(&out).to_owned()
    };
    let none = run("none", "cat /sys/fs/cgroup/memory.max");
    assert_eq!(none, "max\n");
    let script = "cd /sys/fs/cgroup; cat memory.max; grep oom_kill memory.events; \
                  awk 'BEGIN { s = \"x\"; for (i = 0; i < 27; i++) s = s s; print length(s) }'; \
                  echo awk $?; grep -E '^(max|oom|oom_kill) ' memory.events | sed 's/ [1-9][0-9]*$/ N/'; \
                  [ $(cat memory.peak) -gt 134217728 ] && echo peak over the limit; \
                  dmesg | grep -oE 'task=awk,pid=[0-9]+|Killed process [0-9]+ \\(awk\\)' | sed 's/[0-9][0-9]*/N/'";
    assert_eq!(
        run("128M", script),
        "134217728\noom_kill 0\nawk 137\nmax N\noom N\noom_kill N\npeak over the limit\n\
         task=awk,pid=N\nKilled process N (awk)\n"
    );
}

#[test]
fn oom_kills_are_reported_to_the_app() {
    // A process the OOM killer kills leaves a report for the app (take_kills): the console's
    // number, which cgroup's memory.max (no cgroup for the app's limit), the usage and the limit.
    // Run only under Tier 1, for the same reason. The awk in the cgroup keeps running for a while
    // after building a 64 MB string: the OOM killer measures usage periodically, and would not
    // notice a process that exits as soon as it has used it all
    use tuxport_kernel::{ExitStatus, KillReason, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    let limit = 96 << 20;
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Dbt,
        rootfs: Some(store.to_path_buf()),
        memory_limit: Some(limit),
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: false,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let id = machine.open_console(24, 80).unwrap();
    let script = "cd /sys/fs/cgroup; echo +memory > cgroup.subtree_control; mkdir -p oomt; \
                  echo 16M > oomt/memory.max; \
                  sh -c 'echo $$ > oomt/cgroup.procs; \
                      exec awk \"BEGIN { s = \\\"x\\\"; for (i = 0; i < 26; i++) s = s s; \
                                         for (j = 0; j < 50000000; j++) n++ }\"'; \
                  echo cgroup $?; \
                  awk 'BEGIN { s = \"x\"; for (i = 0; i < 27; i++) s = s s; print length(s) }'; \
                  echo awk $?";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    let out = read_console(&machine, id);
    assert!(
        out.contains("cgroup 137\n") && out.ends_with("awk 137\n"),
        "{out}"
    );
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    let kills = machine.take_kills();
    let [first, awk] = kills.as_slice() else {
        panic!("{kills:?}");
    };
    assert_eq!(
        (first.comm.as_str(), first.console),
        ("awk", Some(id)),
        "{first:?}"
    );
    assert!(
        matches!(&first.reason, KillReason::OutOfMemory { cgroup: Some(c), used, limit }
            if c == "/oomt" && *limit == 16 << 20 && used > limit),
        "{first:?}"
    );
    assert_eq!(
        (awk.comm.as_str(), awk.console),
        ("awk", Some(id)),
        "{awk:?}"
    );
    assert!(
        matches!(&awk.reason, KillReason::OutOfMemory { cgroup: None, used, limit: l }
            if *l == limit && used > l),
        "{awk:?}"
    );
    // A process not on a console (here one detached from its controlling terminal) has no number
    let script = "setsid sh -c 'exec awk \"BEGIN { s = \\\"x\\\"; for (i = 0; i < 27; i++) s = s s }\"' \
                  </dev/null >/dev/null 2>&1; echo $?";
    let id = machine.open_console(24, 80).unwrap();
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    assert_eq!(read_console(&machine, id), "137\n");
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    let kills = machine.take_kills();
    assert!(
        matches!(kills.as_slice(), [k] if k.comm == "awk" && k.console.is_none()),
        "{kills:?}"
    );
    // memory.oom.group: the sleep in the same group is killed along with it, each with a report,
    // and the picked awk has only one (Linux counts the picked one again when it kills the whole
    // group). The sh running the script is not in the group; the subshell joins it, then execs
    // into awk
    let script = "cd /sys/fs/cgroup; mkdir g; echo 16M > g/memory.max; echo 1 > g/memory.oom.group; \
                  sh -c 'echo $$ > g/cgroup.procs; sleep 30 & \
                      exec awk \"BEGIN { s = \\\"x\\\"; for (i = 0; i < 26; i++) s = s s; \
                                         for (j = 0; j < 50000000; j++) n++ }\"'; \
                  echo group $?";
    let id = machine.open_console(24, 80).unwrap();
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    let out = read_console(&machine, id);
    assert!(out.ends_with("group 137\n"), "{out}");
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    let mut comms: Vec<_> = machine
        .take_kills()
        .into_iter()
        .map(|k| (k.comm, k.console))
        .collect();
    comms.sort();
    assert_eq!(
        comms,
        [("awk".to_owned(), Some(id)), ("sleep".to_owned(), Some(id))]
    );
}

#[test]
fn code_cache_pages_are_counted_once() {
    // Tier 1's code cache is mapped twice for W^X (one for writing, one for execution), and the
    // host's RSS counts pages per mapping: a page touched in both views counts twice, so the
    // translations count double in the app's memory usage (what the memory limit and the LMK look
    // at). So the timer thread tears down the written pages of the writing view once a second,
    // and once more after all threads are waiting. A series of small busybox programs gets the
    // same program's instance kept in the pool, translations keep piling up, all occupying the
    // execution view; once the shell stops in read, the writing view soon occupies no pages
    let Some(store) = store() else {
        return;
    };
    let script = "for c in 'ls -lR /etc' 'find /usr -name *.so*' 'du -s /usr' ps df date \
                  'sort -r /etc/passwd' 'sed s/a/b/g /etc/passwd' 'awk -F: {print} /etc/passwd' \
                  'grep -r root /etc' 'tar c /etc' 'gzip -c /bin/busybox' 'sha256sum /bin/busybox' \
                  'md5sum /bin/busybox' 'od -x /etc/passwd' 'hexdump -C /etc/hostname' \
                  'cut -d: -f1 /etc/passwd' 'tr a-z A-Z' 'uniq /etc/passwd' 'base64 /etc/passwd' \
                  'stat /etc' 'top -bn1' 'netstat -an' 'diff /etc/passwd /etc/group' \
                  'bzip2 -c /etc/passwd' 'xargs echo'; do $c </etc/passwd; done >/dev/null 2>&1; \
                  echo ready; read x";
    let mut child = command(store, "1", &["/bin/sh", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "ready");
    // The code cache is a 128 MB shared mapping: (permissions, Rss KB) of each view
    let caches = || {
        let smaps = std::fs::read_to_string(format!("/proc/{}/smaps", child.id())).unwrap();
        let (mut caches, mut view) = (Vec::new(), None);
        for line in smaps.lines() {
            let mut f = line.split_whitespace();
            match (f.next(), f.next()) {
                (Some(range), Some(perms)) if range.contains('-') && perms.len() == 4 => {
                    let (lo, hi) = range.split_once('-').unwrap();
                    let size =
                        u64::from_str_radix(hi, 16).unwrap() - u64::from_str_radix(lo, 16).unwrap();
                    view = (size == 128 << 20 && perms.ends_with('s')).then(|| perms.to_owned());
                }
                (Some("Rss:"), Some(kb)) => {
                    if let Some(perms) = view.take() {
                        caches.push((perms, kb.parse::<u64>().unwrap()));
                    }
                }
                _ => {}
            }
        }
        caches
    };
    let rss = |caches: &[(String, u64)], perms: &str| -> Vec<u64> {
        caches
            .iter()
            .filter(|(p, _)| p == perms)
            .map(|&(_, kb)| kb)
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = caches();
    while rss(&seen, "rw-s").iter().sum::<u64>() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        seen = caches();
    }
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(
        rss(&seen, "r-xs")
            .iter()
            .max()
            .is_some_and(|&kb| kb >= 1024),
        "{seen:?}"
    );
    assert_eq!(rss(&seen, "rw-s").iter().sum::<u64>(), 0, "{seen:?}");
}

#[test]
fn processes_of_one_program_share_translations() {
    // Processes of one program that are mapped alike share Tier 1 translations: the shell and the
    // 6 sleeps it starts (all busybox, sleep run directly in the forked process) take only one
    // code cache, where each used to have its own. A file's ctime is trusted only once it is more
    // than two seconds old (this store was just imported), so wait a while before exec'ing a shell
    let Some(store) = store() else {
        return;
    };
    let script = "sleep 3; exec sh -c 'for i in 1 2 3 4 5 6; do sleep 30 & done; echo ready; \
                  read x; kill $(jobs -p)'";
    let mut child = command(store, "1", &["/bin/sh", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "ready");
    // Wait for these sleeps to all start running (having translated code)
    std::thread::sleep(Duration::from_millis(500));
    let smaps = std::fs::read_to_string(format!("/proc/{}/smaps", child.id())).unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let caches = smaps
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (range, perms) = (f.next()?, f.next()?);
            let (lo, hi) = range.split_once('-')?;
            let size = u64::from_str_radix(hi, 16).ok()? - u64::from_str_radix(lo, 16).ok()?;
            (size == 128 << 20 && perms == "r-xs").then_some(())
        })
        .count();
    // The shared one, plus the one kept in the pool (the shell before exec)
    assert!((1..=2).contains(&caches), "{caches} code caches");
}

/// Ports below 1024 (docs/rounds/156-low-ports.md, 208-low-ports-direct.md): when the host allows
/// binding the original port it is bound (running as root, or in a container), otherwise another
/// one is used; when always mapping, the host first takes 10000 + port, and the guest's port moves
/// to 20000 + port. Machine reports this listen on a non-loopback address, the host can connect
/// through the reported port, and when the port moved the guest's kernel log records where it
/// moved to. A different port each time, so the TIME_WAIT left by one connection does not affect
/// the next.
#[test]
fn low_ports_bind_the_real_port_or_elsewhere() {
    use tuxport_kernel::{ExitStatus, ListenEvent, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    for (tier, port, always_map) in [
        (Tier::Interpreter, 79u16, true),
        (Tier::Dbt, 78, true),
        (Tier::Interpreter, 77, false),
        (Tier::Dbt, 76, false),
    ] {
        // busybox's nc binds the IPv6 wildcard address, so take the same kind here
        let busy = always_map.then(|| std::net::TcpListener::bind(("::", 10000 + port)));
        if let Some(Err(_)) = busy {
            // Only this case: the others do not need that port
            eprintln!(
                "skipped {tier:?} port {port}: cannot bind [::]:{} on the host",
                10000 + port
            );
            continue;
        }
        let host_port = if always_map {
            20000 + port
        } else if std::net::TcpListener::bind(("::", port)).is_ok() {
            port
        } else {
            10000 + port
        };
        let machine = Machine::start(&MachineOptions {
            tier,
            rootfs: Some(store.to_path_buf()),
            memory_limit: None,
            low_port_offset: None,
            always_map_low_ports: always_map,
            exception_trace: false,
        })
        .unwrap();
        let env = [PATH.as_bytes().to_vec()];
        let id = machine.open_console(24, 80).unwrap();
        let script = format!("echo hello | nc -l -p {port}; dmesg | grep 'port {port}/' || true");
        let argv = [b"sh".to_vec(), b"-c".to_vec(), script.into_bytes()];
        let pid = machine
            .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let event = loop {
            if let Some(e) = machine.take_listens().into_iter().next() {
                break e;
            }
            assert!(Instant::now() < deadline, "{tier:?}: no listen event");
            std::thread::sleep(Duration::from_millis(20));
        };
        let want = ListenEvent {
            comm: "nc".into(),
            port,
            host_port,
        };
        assert_eq!(event, want, "{tier:?}");
        // nc does not close the connection after reading stdin; this side closes it once it has
        // read, and only then does nc exit
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", host_port)).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut got = [0u8; 6];
        conn.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello\n", "{tier:?}");
        drop(conn);
        let out = read_console(&machine, id);
        let log = format!("tuxport: port {port}/tcp is {host_port} on the host");
        // Nothing is logged when the original port can be bound
        assert_eq!(out.contains(&log), host_port != port, "{tier:?}: {out}");
        assert_eq!(machine.wait(pid), ExitStatus::Exited(0), "{tier:?}");
        assert!(machine.take_listens().is_empty(), "{tier:?}");
    }
}

#[test]
fn one_machine_runs_several_terminal_sessions() {
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    for tier in [Tier::Interpreter, Tier::Dbt] {
        let machine = Machine::start(&MachineOptions {
            tier,
            rootfs: Some(store.to_path_buf()),
            memory_limit: None,
            low_port_offset: None,
            always_map_low_ports: false,
            exception_trace: false,
        })
        .unwrap();
        let env = [PATH.as_bytes().to_vec()];
        let sh = |script: &str| {
            let id = machine.open_console(24, 80).unwrap();
            let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
            let pid = machine
                .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
                .unwrap();
            (id, pid)
        };
        // Two sessions run at once, each with its own controlling terminal
        let (a, pa) = sh("tty; stty size; echo one");
        let (b, pb) = sh("read line; echo got $line; exit 3");
        machine.write_console(b, b"input\n").unwrap();
        machine.resize_console(a, 40, 100).unwrap();
        let (out_a, out_b) = (read_console(&machine, a), read_console(&machine, b));
        assert!(out_a.starts_with("/dev/pts/"), "{tier:?}: {out_a}");
        assert!(out_a.ends_with("one\n"), "{tier:?}: {out_a}");
        assert_eq!(out_b, "input\ngot input\n", "{tier:?}");
        assert_eq!(machine.wait(pa), ExitStatus::Exited(0));
        assert_eq!(machine.wait(pb), ExitStatus::Exited(3));
        // Closing a console is hanging up the terminal: an interactive shell receives SIGHUP and
        // exits
        let id = machine.open_console(24, 80).unwrap();
        let argv = [b"sh".to_vec(), b"-i".to_vec()];
        let pid = machine
            .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
            .unwrap();
        machine.write_console(id, b"echo ready\n").unwrap();
        machine.close_console(id).unwrap();
        assert!(
            matches!(machine.wait(pid), ExitStatus::Signaled { signal, .. } if signal.number() == 1),
            "{tier:?}"
        );
        assert_eq!(
            machine.read_console(id, &mut [0; 8]),
            Err(tuxport_kernel::errno::Errno::ENOENT)
        );
    }
}

/// The few settings Alpine's sysctl service writes at boot (`/usr/lib/sysctl.d/00-alpine.conf`),
/// and the rules for writing them: `/proc/sys` is read-only in a container, so a differential
/// test cannot compare writes (round 185).
#[test]
fn sysctls_written_at_alpine_boot() {
    let script = "
        sysctl -w kernel.panic=120 fs.protected_hardlinks=1 fs.protected_symlinks=0 \
            kernel.unprivileged_bpf_disabled=1 >/dev/null && echo set
        cat /proc/sys/kernel/panic /proc/sys/fs/protected_symlinks \
            /proc/sys/kernel/unprivileged_bpf_disabled
        echo 0 2>/dev/null > /proc/sys/kernel/unprivileged_bpf_disabled || echo locked
        echo 2 2>/dev/null > /proc/sys/fs/protected_symlinks || echo range
        su -s /bin/sh nobody -c 'echo 0 > /proc/sys/kernel/panic' 2>/dev/null || echo nobody
        link=/tmp/sysctl-link-$$
        ln -s /etc/hostname $link && chown -h nobody $link
        cat $link >/dev/null && echo followed
        echo 1 > /proc/sys/fs/protected_symlinks
        cat $link 2>&1 >/dev/null | grep -c 'Permission denied'
        rm $link";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_ok(tier, &out);
        assert_eq!(
            stdout(&out),
            "set\n120\n0\n1\nlocked\nrange\nnobody\nfollowed\n1\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

/// The built-in init boots and shuts down by inittab (round 186): the order, output written to
/// /var/log/boot.log, environment variables the same as busybox init's, skipping uninstalled
/// programs and respawn; after boot the guest's poweroff and `kill -TERM 1` are requests to power
/// off and to reboot, and without boot they do nothing. Done in one test case: they all change
/// /etc/inittab in the store.
#[test]
fn init_runs_the_inittab() {
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Power, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    let inittab = "\
::sysinit:/bin/sh -c 'echo sysinit $PATH $HOME >> /tmp/inittab-order'
::sysinit:/bin/echo printed to the log
::sysinit:/sbin/not-installed sysinit
::wait:/bin/sh -c 'echo wait >> /tmp/inittab-order'
::once:/bin/sh -c 'sleep 0.3; echo once >> /tmp/inittab-order'
tty1::respawn:/sbin/getty 38400 tty1
::respawn:/bin/sh -c 'echo respawn >> /tmp/inittab-order'
::shutdown:/bin/sh -c 'echo shutdown >> /tmp/inittab-order'
";
    let setup = format!("cat > /etc/inittab <<'EOF'\n{inittab}EOF\nrm -f /tmp/inittab-order");
    let order = |tier: &str| {
        let out = command(store, tier, &["/bin/cat", "/tmp/inittab-order"])
            .output()
            .unwrap();
        stdout(&out).to_owned()
    };
    for tier in TIERS {
        let out = command(store, tier, &["/bin/sh", "-c", &setup])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let init = |script: &str| {
            Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
                .args(["--tier", tier, "--init", "--env", PATH, "--rootfs"])
                .arg(store)
                .args(["--", "/bin/sh", "-c", script])
                .output()
                .unwrap()
        };
        // The program starts after sysinit and wait; after it ends, shut down
        let out = init("cat /tmp/inittab-order /var/log/boot.log; sleep 1.5");
        assert_ok(tier, &out);
        let booted = "sysinit /sbin:/usr/sbin:/bin:/usr/bin /\nwait\n";
        assert_eq!(
            stdout(&out),
            format!("{booted}printed to the log\n"),
            "tier {tier}"
        );
        assert_eq!(
            order(tier),
            format!("{booted}once\nshutdown\n"),
            "tier {tier}"
        );
        // poweroff: the shutdown commands run as usual, and the program is ended by SIGTERM
        let start = Instant::now();
        let out = init("rm /tmp/inittab-order; poweroff; sleep 5; echo not reached");
        assert_eq!(
            out.status.code(),
            Some(128 + 15),
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(start.elapsed() < Duration::from_secs(4), "tier {tier}");
        assert!(order(tier).contains("shutdown\n"), "tier {tier}");
        // Without --init, poweroff does nothing
        let out = command(store, tier, &["/bin/sh", "-c", "poweroff; echo still here"])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "still here\n", "tier {tier}");
    }
    // The Machine interface: the app takes requests with take_power and decides by processes
    // whether to stay in the background
    for tier in [Tier::Interpreter, Tier::Dbt] {
        let machine = Machine::start(&MachineOptions {
            tier,
            rootfs: Some(store.to_path_buf()),
            memory_limit: None,
            low_port_offset: None,
            always_map_low_ports: false,
            exception_trace: false,
        })
        .unwrap();
        let env = [PATH.as_bytes().to_vec()];
        let sh = |script: &str| {
            let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
            let pid = machine.spawn(b"/bin/sh", &argv, &env, Stdio::Host).unwrap();
            machine.wait(pid)
        };
        assert_eq!(sh("kill -TERM 1"), ExitStatus::Exited(0));
        assert_eq!(machine.take_power(), None, "{tier:?}: not booted");
        machine.boot();
        assert!(machine.processes() >= 1, "{tier:?}: the once entry");
        assert_eq!(
            sh("rm /tmp/inittab-order; kill -TERM 1"),
            ExitStatus::Exited(0)
        );
        assert_eq!(machine.take_power(), Some(Power::Reboot), "{tier:?}");
        assert_eq!(machine.take_power(), None, "{tier:?}");
        machine.shutdown(Power::Reboot, Duration::from_secs(10));
        assert_eq!(machine.processes(), 0, "{tier:?}");
    }
    // Not a single command ran (none of the programs is installed): the previous boot's output is
    // cleared all the same (round 195; the app looks in it for services that failed to start)
    let script = "cp /etc/inittab /tmp/inittab.saved; echo ::sysinit:/sbin/not-installed > /etc/inittab; \
                  echo ' * ERROR: stale failed to start' > /var/log/boot.log";
    assert_ok(
        "1",
        &command(store, "1", &["/bin/sh", "-c", script])
            .output()
            .unwrap(),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
        .args(["--tier", "1", "--init", "--env", PATH, "--rootfs"])
        .arg(store)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "wc -c < /var/log/boot.log; mv /tmp/inittab.saved /etc/inittab",
        ])
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "0\n", "{}", stderr(&out));
    let out = command(
        store,
        "1",
        &["/bin/sh", "-c", "cat /tmp/inittab-order; rm /etc/inittab"],
    )
    .output()
    .unwrap();
    assert!(stdout(&out).ends_with("shutdown\n"), "{}", stdout(&out));
}

#[test]
fn files_app_changes_reach_inotify_watchers() {
    // Changes made by the Files app (the host opening the store separately,
    // tuxport_kernel::browse_changing) are handed to the running kernel, and programs watching in
    // the guest receive events all the same: create, open for writing (truncate), write on close,
    // rename, delete. busybox's inotifyd prints events as "letters\tdirectory\tname", the letters
    // in event-bit order being acew0rmynd: c is IN_MODIFY, r IN_OPEN, w IN_CLOSE_WRITE, m and y
    // moved out and moved in
    use tuxport_kernel::vfs::browse;
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    use std::io::Write;
    let Some(store) = store() else {
        return;
    };
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Dbt,
        rootfs: Some(store.to_path_buf()),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: false,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let id = machine.open_console(24, 200).unwrap();
    let script = "rm -rf /root/fw; mkdir /root/fw; timeout 4 inotifyd - /root/fw & \
                  sleep 1; echo ready; wait";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine
        .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
        .unwrap();
    let host = {
        let store = store.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let top = b"/root".as_slice();
            let s = store.as_path();
            let ok = |r: Result<(), tuxport_kernel::errno::Errno>| r.unwrap();
            ok(tuxport_kernel::browse_changing(s, |s, c| {
                browse::create(s, top, b"/root/fw", b"a.txt", false, (0, 0), c).map(drop)
            }));
            ok(tuxport_kernel::browse_changing(s, |s, c| {
                browse::open_write(s, top, b"/root/fw/a.txt", true, false, c)?
                    .write_all(b"edited\n")
                    .map_err(|_| tuxport_kernel::errno::Errno::EIO)
            }));
            ok(tuxport_kernel::browse_changing(s, |s, c| {
                browse::written(s, top, b"/root/fw/a.txt", c)
            }));
            ok(tuxport_kernel::browse_changing(s, |s, c| {
                browse::rename(s, top, b"/root/fw/a.txt", b"b.txt", c)
            }));
            ok(tuxport_kernel::browse_changing(s, |s, c| {
                browse::remove(s, top, b"/root/fw/b.txt", c)
            }));
        })
    };
    let out = read_console(&machine, id);
    host.join().unwrap();
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    let events = out.split_once("ready\n").map(|(_, e)| e).unwrap_or(&out);
    assert_eq!(
        events,
        "n\t/root/fw\ta.txt\nc\t/root/fw\ta.txt\nr\t/root/fw\ta.txt\n\
         c\t/root/fw\ta.txt\nw\t/root/fw\ta.txt\nm\t/root/fw\ta.txt\ny\t/root/fw\tb.txt\n\
         d\t/root/fw\tb.txt\n",
        "{out}"
    );
    // Conversely, directories whose entries the guest changed are handed to the app (the Files app
    // refreshes them): recording starts only at the first take
    assert!(machine.take_changed_dirs().is_empty());
    let script = "mkdir /root/fw/d && touch /root/fw/d/f && echo x > /root/fw/d/f";
    let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
    let pid = machine.spawn(b"/bin/sh", &argv, &env, Stdio::Host).unwrap();
    assert_eq!(machine.wait(pid), ExitStatus::Exited(0));
    let mut dirs = machine.take_changed_dirs();
    dirs.sort();
    assert_eq!(dirs, [b"/root/fw".to_vec(), b"/root/fw/d".to_vec()]);
    assert!(machine.take_changed_dirs().is_empty());
}

#[test]
fn files_app_writes_stay_out_of_clones() {
    // The Files app writes a file's host object itself (tuxport_kernel::browse_changing hands out
    // the fd): a clone the guest makes while the app has the file open copies it rather than
    // sharing its object, so the app's later writes stay out of the clone; and when the app opens
    // a file that shares its object for writing, the guest's descriptions of the file move to the
    // file's new object with it and read what the app wrote (tests/guest/hostclone.c)
    use tuxport_kernel::vfs::browse;
    use tuxport_kernel::{ExitStatus, Machine, MachineOptions, Stdio as Io, Tier};
    use std::os::unix::fs::FileExt;
    let (Some(store), Some(program)) = (store(), build_static("hostclone")) else {
        return;
    };
    let script = "rm -rf /root/hc && mkdir /root/hc && cat > /root/hc/hostclone && \
                  chmod +x /root/hc/hostclone && echo AAAA > /root/hc/src";
    let out = command(store, "1", &["/bin/sh", "-c", script])
        .stdin(std::fs::File::open(&program).unwrap())
        .output()
        .unwrap();
    assert_ok("1", &out);
    let machine = Machine::start(&MachineOptions {
        tier: Tier::Dbt,
        rootfs: Some(store.to_path_buf()),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
        exception_trace: false,
    })
    .unwrap();
    let env = [PATH.as_bytes().to_vec()];
    let run = |script: &str| {
        let id = machine.open_console(24, 200).unwrap();
        let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
        let pid = machine
            .spawn(b"/bin/sh", &argv, &env, Io::Console(id))
            .unwrap();
        let out = read_console(&machine, id).replace('\r', "");
        assert_eq!(machine.wait(pid), ExitStatus::Exited(0), "{out}");
        out
    };
    let top = b"/root".as_slice();
    let errno = |_| tuxport_kernel::errno::Errno::EIO;
    // The app opens src for writing and keeps it open while the guest clones it
    let mut writer = None;
    tuxport_kernel::browse_changing(store, |s, c| {
        writer = Some(browse::open_write(
            s,
            top,
            b"/root/hc/src",
            false,
            false,
            c,
        )?);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        run("/root/hc/hostclone clone /root/hc/src /root/hc/dst"),
        "cloned\n"
    );
    writer.take().unwrap().write_all_at(b"BBBB\n", 0).unwrap();
    tuxport_kernel::browse_changing(store, |s, c| browse::written(s, top, b"/root/hc/src", c))
        .unwrap();
    assert_eq!(run("cat /root/hc/src /root/hc/dst"), "BBBB\nAAAA\n");
    // A clone made after the app closed the file shares it again; the app then writes the clone
    // while a guest program holds it open
    assert_eq!(
        run("/root/hc/hostclone clone /root/hc/src /root/hc/dst2"),
        "cloned\n"
    );
    let host = {
        let store = store.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            tuxport_kernel::browse_changing(&store, |s, c| {
                let f = browse::open_write(s, top, b"/root/hc/dst2", false, false, c)?;
                f.write_all_at(b"CCCC\n", 0).map_err(errno)
            })
            .unwrap();
            tuxport_kernel::browse_changing(&store, |s, c| {
                browse::written(s, top, b"/root/hc/dst2", c)
            })
            .unwrap();
        })
    };
    let out = run("/root/hc/hostclone hold /root/hc/dst2 3");
    host.join().unwrap();
    assert_eq!(out, "holding\nread CCCC\n");
    assert_eq!(run("cat /root/hc/src; rm -rf /root/hc"), "BBBB\n");
}

/// Runs once under each tier, returning (tier, output); returns an empty list without a rootfs.
fn run(args: &[&str]) -> Vec<(&'static str, Output)> {
    let Some(store) = store() else {
        return Vec::new();
    };
    TIERS
        .iter()
        .map(|&tier| (tier, command(store, tier, args).output().unwrap()))
        .collect()
}

/// Imports the rootfs once; all test cases share the same store.
fn store() -> Option<&'static Path> {
    static STORE: OnceLock<Option<PathBuf>> = OnceLock::new();
    let store = STORE
        .get_or_init(|| {
            let Some(tarball) = find_tarball() else {
                assert!(
                    std::env::var_os("TUXPORT_REQUIRE_ROOTFS").is_none_or(|v| v != "1"),
                    "rootfs tarball not found; run scripts/fetch-rootfs"
                );
                return None;
            };
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rootfs-store");
            let _ = std::fs::remove_dir_all(&dir);
            let out = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
                .arg("--rootfs")
                .arg(&dir)
                .arg("--import")
                .arg(&tarball)
                .output()
                .unwrap();
            assert!(out.status.success(), "import failed: {}", stderr(&out));
            Some(dir)
        })
        .as_deref();
    // Every test that skips says so, not only the first to ask
    if store.is_none() {
        eprintln!("skipping rootfs tests: run scripts/fetch-rootfs to enable them");
    }
    store
}

fn find_tarball() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TUXPORT_ROOTFS_TARBALL") {
        return Some(PathBuf::from(p));
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rootfs");
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with("alpine-minirootfs-") && name.ends_with("-aarch64.tar.gz")
        })
        .collect();
    // The newest by version (3.24 after 3.3), not by name
    found.sort_by_key(|p| version_key(&p.file_name().unwrap().to_string_lossy()));
    found.pop()
}

#[test]
fn tarballs_are_chosen_by_version() {
    let mut names = [
        "alpine-minirootfs-3.3.0-aarch64.tar.gz",
        "alpine-minirootfs-3.24.3-aarch64.tar.gz",
        "alpine-minirootfs-3.9.1-aarch64.tar.gz",
    ];
    names.sort_by_key(|n| version_key(n));
    assert_eq!(names[2], "alpine-minirootfs-3.24.3-aarch64.tar.gz");
}

/// The numbers in a file name, compared in order (`alpine-minirootfs-3.24.3-aarch64.tar.gz` gives
/// 3, 24, 3, 64).
fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// Builds tests/guest/<name>.c statically with musl-gcc; returns None without musl-gcc.
fn build_static(name: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.c"));
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rootfs-{name}"));
    let status = Command::new("musl-gcc")
        .args([
            "-static", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", "-o",
        ])
        .arg(&out)
        .arg(&src)
        .status();
    let Ok(status) = status else {
        assert!(
            std::env::var_os("TUXPORT_REQUIRE_MUSL").is_none_or(|v| v != "1"),
            "musl-gcc not found; install musl-tools"
        );
        eprintln!("skipping {name}: musl-gcc not found");
        return None;
    };
    assert!(status.success(), "compiling {} failed", src.display());
    Some(out)
}

fn command(store: &Path, tier: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tuxport-run"));
    cmd.args(["--tier", tier, "--env", PATH, "--rootfs"])
        .arg(store)
        .arg("--")
        .args(args);
    cmd
}

/// Reads the console until the session ends (the way the Android terminal tabs use Machine).
fn read_console(machine: &tuxport_kernel::Machine, id: u32) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match machine.read_console(id, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8(out).unwrap().replace("\r\n", "\n")
}

/// Checks that a script run in `sh -c` exits with 0, and returns its stdout.
fn shell(tier: &str, out: &Output) -> String {
    assert_ok(tier, out);
    stdout(out).to_owned()
}

fn assert_ok(tier: &str, out: &Output) {
    assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(out));
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}
