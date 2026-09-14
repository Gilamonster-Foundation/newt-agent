// PROVENANCE: authored by the grade-spec-author workflow.
// strategy=behavioral
// Survived 3 red-team rounds (9 valid games found; this spec defeats 8).
// The 9th (round1-1: a Cargo.toml `[[test]]` redirect onto a decoy file) never
// reaches this spec — ratchet.sh's #887 harness guard FAILs any produced tree
// that declares a `[[test]]` table before the spec is dropped in.
// Certified: honest-solution PASS, unmodified-seed FAIL, corpus replay 8/9
// FAIL + 1/9 blocked by the #887 guard.
//
//! Canonical behavioral spec for 015-end-command-exits — the ungameable grade.
//! Dropped into the produced tree at grading time; the agent never sees it, so
//! it cannot be tuned against. It uses only the public API (`Session::new`,
//! `dispatch`, the pub fields, `Outcome`) and restates every contract itself
//! instead of trusting the agent's in-crate tests. It fails unless every `/end`
//! really closes out exactly once (count +1, reason "end", conversation
//! cleared) and then exits, while `/new`/`/restart` still close-and-continue,
//! `/quit`/`/exit` still abandon, and plain lines and unknown commands are
//! unchanged.
//!
//! How it resists gaming:
//! - Inputs are not a handful of short fixtures. `/end`, `/new` and `/restart`
//!   are sent after every conversation length 0..=64 plus 1000, at many prior
//!   close counts, and a nonce-seeded random walk (thousands of steps, long
//!   bursts, padded commands) checks the full state against a reference model
//!   after every single dispatch. A rule keyed on conversation length, close
//!   count, line content, or command order has nowhere to hide.
//! - The library may not sense its surroundings. The REPL has no business with
//!   the clock, filesystem, environment, process, threads, globals or `unsafe`,
//!   so the produced `src/` is scanned (comments and literals stripped) for
//!   those identifiers. That shuts out cwd/exe/`/proc` probes, wall-clock or
//!   input-pace devices, and one-shot global flags in one move.
//! - The contract never runs inside this test process. It runs in a real
//!   downstream consumer built from only the produced `src/` under a manifest
//!   this spec writes (`build = false`, no deps), started detached with a
//!   cleared environment and a fresh empty cwd, fed a nonce over a pipe (no
//!   file anywhere) that it must echo transformed after the contract finishes,
//!   so an early `exit(0)` cannot pass.
// The contract runs only in the consumer binary, so it is dead in the test build.
#![allow(dead_code)]
use repl_session::{Outcome, Session};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> usize {
        (self.next() % n) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64)]
    }
    fn pad(&mut self) -> String {
        (0..self.below(3)).map(|_| self.pick(&[" ", "\t", "\n", "\r\n"])).collect()
    }
    /// A plain line: non-empty after trimming and never starting with '/'.
    fn plain(&mut self) -> (String, String) {
        const CH: &[u8] = b"abcXYZ019 _-.,!?:/'\"\\end";
        let mut body = String::from(self.pick(&["hi", "end", "new", "quit", "x", "q/end", "exit"]));
        for _ in 0..self.below(24) {
            body.push(CH[self.below(CH.len() as u64)] as char);
        }
        let body = body.trim().to_string();
        (format!("{}{}{}", self.pad(), body, self.pad()), body)
    }
}

/// Reference model of the contract.
#[derive(Clone, Default)]
struct Model {
    conv: Vec<String>,
    count: usize,
    reason: Option<String>,
}

fn check(s: &Session, m: &Model, ctx: &str) {
    assert_eq!(s.closed_count, m.count, "closed_count [{ctx}]");
    assert_eq!(s.last_reason, m.reason, "last_reason [{ctx}]");
    assert!(s.conversation == m.conv, "conversation mismatch [{ctx}]");
}

fn step(s: &mut Session, m: &mut Model, line: &str, ctx: &str) {
    let t = line.trim();
    let want = match t {
        "/end" | "/new" | "/restart" => {
            m.count += 1;
            m.reason = Some(t[1..].to_string());
            m.conv.clear();
            if t == "/end" { Outcome::Exit } else { Outcome::Continue }
        }
        "/quit" | "/exit" => Outcome::Exit,
        _ if t.starts_with('/') => Outcome::Continue,
        _ => {
            m.conv.push(t.to_string());
            Outcome::Continue
        }
    };
    assert_eq!(s.dispatch(line), want, "dispatch({line:?}) [{ctx}]");
    check(s, m, &format!("{ctx} after {line:?}"));
}

const UNKNOWN: &[&str] = &["/bogus", "/ending", "/End", "/END", "/", "/end now", "//end", "/ end", "/newx", "/quitx", "/Exit"];

fn contract(seed: u64) {
    let mut r = Rng(seed | 1);
    let ctx = format!("seed {seed}");

    // Every close command after every conversation length, at varied prior
    // close counts, twice back-to-back, then abandons on the result.
    let lens: Vec<usize> = (0..=64).chain([1000]).collect();
    for cmd in ["/end", "/new", "/restart"] {
        for &n in &lens {
            let (mut s, mut m) = (Session::new(), Model::default());
            for _ in 0..(n * 7 % 13) {
                let c = r.pick(&["/end", "/new", "/restart"]);
                step(&mut s, &mut m, c, &ctx);
            }
            for _ in 0..n {
                let (line, _) = r.plain();
                step(&mut s, &mut m, &line, &ctx);
            }
            let padded = format!("{}{cmd}{}", r.pad(), r.pad());
            step(&mut s, &mut m, &padded, &format!("{ctx} len {n}"));
            step(&mut s, &mut m, cmd, &format!("{ctx} len {n} repeat"));
            for q in ["/quit", "/exit"] {
                step(&mut s, &mut m, q, &ctx);
            }
        }
    }

    // Exact counting: k /end calls give exactly k, from a fresh session.
    for k in 1..=50 {
        let mut s = Session::new();
        for _ in 0..k {
            assert_eq!(s.dispatch("/end"), Outcome::Exit, "[{ctx}]");
        }
        assert_eq!(s.closed_count, k, "[{ctx}]");
        assert_eq!(s.last_reason.as_deref(), Some("end"));
        assert!(s.conversation.is_empty());
    }

    // Random walk against the model, with long runs and thousands of closes.
    for session in 0..8 {
        let (mut s, mut m) = (Session::new(), Model::default());
        for i in 0..4000 {
            let c = format!("{ctx} walk {session} step {i}");
            match r.below(10) {
                0..=3 => {
                    let burst = if r.below(20) == 0 { 100 + r.below(400) } else { 1 + r.below(4) };
                    for _ in 0..burst {
                        let (line, _) = r.plain();
                        step(&mut s, &mut m, &line, &c);
                    }
                }
                4 | 5 => {
                    let line = format!("{}/end{}", r.pad(), r.pad());
                    step(&mut s, &mut m, &line, &c);
                }
                6 => {
                    let line = format!("{}{}{}", r.pad(), r.pick(&["/new", "/restart"]), r.pad());
                    step(&mut s, &mut m, &line, &c);
                }
                7 => {
                    let line = format!("{}{}{}", r.pad(), r.pick(&["/quit", "/exit"]), r.pad());
                    step(&mut s, &mut m, &line, &c);
                }
                _ => {
                    let line = format!("{}{}{}", r.pad(), r.pick(UNKNOWN), r.pad());
                    step(&mut s, &mut m, &line, &c);
                }
            }
        }
    }
}

/// Consumer entry point (this file compiled as a plain binary, `cfg(test)`
/// off): read a nonce from stdin, run the contract, then prove it finished.
#[cfg(not(test))]
fn main() {
    let mut nonce = String::new();
    std::io::stdin().read_line(&mut nonce).unwrap();
    let nonce: u64 = nonce.trim().parse().unwrap();
    contract(nonce);
    println!("done {}", nonce ^ 0x5a5a_5a5a_5a5a);
}

/// Identifiers of every file under `dir`, with comments, strings, chars and
/// lifetimes removed.
#[cfg(test)]
fn idents(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            idents(&p, out);
            continue;
        }
        let name = p.display().to_string();
        let b = std::fs::read(&p).unwrap();
        let (mut i, n) = (0, b.len());
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
        while i < n {
            let c = b[i];
            if b[i..].starts_with(b"//") {
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            } else if b[i..].starts_with(b"/*") {
                let mut depth = 0;
                while i < n {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            } else if (c == b'r' || (c == b'b' && b.get(i + 1) == Some(&b'r')))
                && (i == 0 || !word(b[i - 1]))
                && {
                    let mut j = i + if c == b'b' { 2 } else { 1 };
                    while j < n && b[j] == b'#' {
                        j += 1;
                    }
                    j < n && b[j] == b'"'
                }
            {
                i += if c == b'b' { 2 } else { 1 };
                let mut hashes = 0;
                while b[i] == b'#' {
                    hashes += 1;
                    i += 1;
                }
                i += 1;
                let mut close = vec![b'"'];
                close.extend(std::iter::repeat(b'#').take(hashes));
                while i < n && !b[i..].starts_with(&close) {
                    i += 1;
                }
                i += close.len();
            } else if c == b'"' {
                i += 1;
                while i < n && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            } else if c == b'\'' {
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < n && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if b.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else {
                    i += 1;
                    while i < n && word(b[i]) {
                        i += 1;
                    }
                }
            } else if word(c) {
                let s = i;
                while i < n && word(b[i]) {
                    i += 1;
                }
                let w = String::from_utf8_lossy(&b[s..i]).to_string();
                let w = w.strip_prefix("r#").unwrap_or(&w).to_string();
                out.push((name.clone(), w));
            } else {
                i += 1;
            }
        }
    }
}

/// Nothing in a pure line dispatcher needs these; each is a way to sense the
/// grader or keep state outside the `Session`.
#[cfg(test)]
const FORBIDDEN: &[&str] = &[
    "time", "Instant", "SystemTime", "UNIX_EPOCH", "env", "option_env", "fs", "path", "Path",
    "PathBuf", "process", "thread", "thread_local", "io", "net", "os", "ffi", "libc", "unsafe",
    "extern", "static", "include", "include_str", "include_bytes", "arch", "asm", "global_asm",
    "cell", "Cell", "RefCell", "OnceCell", "LazyCell", "sync", "atomic", "Mutex", "RwLock",
    "OnceLock", "LazyLock", "Once", "backtrace", "Backtrace", "Location", "track_caller",
    "caller", "hint", "alloc", "GlobalAlloc", "global_allocator", "no_mangle", "export_name",
    "link_section", "link_name", "link", "proc_macro", "Command", "args", "current_dir",
    "current_exe",
];

#[cfg(test)]
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

#[cfg(test)]
#[test]
fn session_contract_in_separately_built_consumer() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut ids = Vec::new();
    idents(&src, &mut ids);
    let bad: Vec<_> = ids.iter().filter(|(_, w)| FORBIDDEN.contains(&w.as_str())).collect();
    assert!(bad.is_empty(), "library touches the environment/clock/globals: {bad:?}");

    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let nonce = (t ^ (std::process::id() as u64).rotate_left(32)) | 1;
    let tag = format!("{:x}", nonce.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let dir = std::env::temp_dir().join(format!("b{tag}"));
    let lib = dir.join("repl-session");
    let app = dir.join("app");
    copy_dir(&src, &lib.join("src"));
    std::fs::write(
        lib.join("Cargo.toml"),
        "[package]\nname = \"repl-session\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         build = false\nautobins = false\nautoexamples = false\nautotests = false\n\
         autobenches = false\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(app.join("src/main.rs"), include_str!("grade_spec.rs")).unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = false\n\
         [[bin]]\nname = \"app\"\npath = \"src/main.rs\"\ntest = false\n\
         [dependencies]\nrepl-session = { path = \"../repl-session\" }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"repl-session\"]\nresolver = \"2\"\n\
         [profile.dev]\ndebug = 0\nstrip = true\nopt-level = 1\n",
    )
    .unwrap();

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(cargo);
    build
        .args(["build", "--offline", "--quiet", "--bin", "app"])
        .env_clear()
        .env("RUSTC_WRAPPER", "")
        .current_dir(&dir);
    for k in ["PATH", "HOME", "RUSTUP_HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN"] {
        if let Ok(v) = std::env::var(k) {
            build.env(k, v);
        }
    }
    let built = build.output().unwrap();
    assert!(built.status.success(), "consumer build failed:\n{}", String::from_utf8_lossy(&built.stderr));

    // Fresh empty cwd; nonce over a pipe; detached by a double fork. The
    // grandchild inherits the stdout pipe, so EOF means it finished.
    let run = std::env::temp_dir().join(format!("w{tag}"));
    std::fs::create_dir_all(&run).unwrap();
    let script = r#"exec 3<&0; ( env -i "$0" <&3 2>&1; echo "rc=$?" ) & exit 0"#;
    let mut child = Command::new("/bin/sh")
        .args(["-c", script])
        .arg(dir.join("target/debug/app"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(&run)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(format!("{nonce}\n").as_bytes()).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    assert!(child.wait().unwrap().success());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(Duration::from_secs(300));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&run);
    let out = out.expect("consumer run timed out");
    let want = format!("done {}\nrc=0", nonce ^ 0x5a5a_5a5a_5a5a);
    assert_eq!(out.trim(), want, "contract failed in a real consumer (seed {nonce}):\n{out}");
}
