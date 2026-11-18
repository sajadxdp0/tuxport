# Rust: cargo new, building (release, debug), tests, threads, child processes; dependencies pulled
# from crates.io
cd /tmp; rm -rf hello; cargo new -q hello && cd hello || exit 1
cat > src/main.rs <<'RS'
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;
fn main() {
    let t = Instant::now();
    let total = Arc::new(Mutex::new(0u64));
    let (tx, rx) = mpsc::channel();
    let hs: Vec<_> = (0..4).map(|i| { let total = total.clone(); let tx = tx.clone(); std::thread::spawn(move || {
        let s: u64 = (0..2_000_000u64).map(|x| x * x % 7).sum(); *total.lock().unwrap() += s; tx.send(i).unwrap(); }) }).collect();
    for h in hs { h.join().unwrap(); }
    drop(tx);
    let mut ids: Vec<i32> = rx.iter().collect(); ids.sort();
    let out = std::process::Command::new("sh").args(["-c", "echo child"]).output().unwrap();
    std::fs::write("/tmp/hello.txt", "data").unwrap();
    println!("total={} ids={:?} child={} file={} ok={}", total.lock().unwrap(), ids, String::from_utf8_lossy(&out.stdout).trim(), std::fs::read_to_string("/tmp/hello.txt").unwrap(), t.elapsed().as_secs() < 60);
}
#[cfg(test)] mod tests { #[test] fn adds() { assert_eq!(2 + 2, 4); } #[test] #[should_panic] fn panics() { panic!("boom"); } }
RS
s=$(date +%s); cargo build -q --release; echo "build release: $? ($(( $(date +%s) - s ))s)"; ./target/release/hello
cargo test -q 2>&1 | grep -E 'test result|FAILED|panicked' | head -3
cd /tmp; rm -rf dep; cargo new -q dep && cd dep && cargo add -q regex@1 2>&1 | tail -2
cat > src/main.rs <<'RS'
fn main() { let re = regex::Regex::new(r"(\d+)-(\d+)").unwrap(); let c = re.captures("x 12-34 y").unwrap(); println!("regex {} {}", &c[1], &c[2]); }
RS
s=$(date +%s); cargo build -q -j 4 2>&1 | tail -3; echo "build with dependency: $? ($(( $(date +%s) - s ))s)"; ./target/debug/dep
