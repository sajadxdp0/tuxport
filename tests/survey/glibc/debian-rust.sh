# Debian (glibc): rustup installs the glibc toolchain, and cargo builds a project with dependencies
cd /tmp; s=$(date +%s)
curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal 2>&1 | tail -2; echo "rustup: $? ($(( $(date +%s) - s ))s)"
. "$HOME/.cargo/env"; rustc -vV | grep -E '^(release|host)'
rm -rf rt; cargo new -q rt && cd rt && cargo add -q regex@1 2>&1 | tail -1
cat > src/main.rs <<'RS'
fn main() { let re = regex::Regex::new(r"(\d+)-(\d+)").unwrap(); let c = re.captures("x 12-34 y").unwrap();
    let h: Vec<_> = (0..4).map(|i| std::thread::spawn(move || i * 2)).collect();
    println!("rust regex {} {} threads {:?}", &c[1], &c[2], h.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()); }
RS
s=$(date +%s); cargo build -q -j 4 2>&1 | tail -3; echo "cargo build: $? ($(( $(date +%s) - s ))s)"; ./target/debug/rt
