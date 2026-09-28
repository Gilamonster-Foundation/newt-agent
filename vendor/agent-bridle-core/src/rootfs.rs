//! Minimal-rootfs **builder** (ADR 0013 D2 / agent-bridle#107) — the foundation
//! of the Tier-2 program-identity close.
//!
//! ADR 0013's keystone (D1): confine program *identity* by controlling **what
//! exists** in the process's filesystem view, not by allow-listing reads (a
//! readable ELF is a runnable ELF, so `ld.so <readable>` trampolines past
//! Landlock's `Execute`). This module computes the *plan* for a read-only root
//! tree that **physically contains only** the granted program files, their shared
//! library closure, the dynamic loader, the curated runtime data, and the granted
//! `fs_read`/`fs_write` roots — and nothing else executable. With no un-granted
//! ELF present, `find -exec curl`, a `system("curl")`, a shebang to an un-granted
//! interpreter, and an `ld.so` trampoline all fail because the target is absent.
//!
//! This is the **builder only** (the plan + a copy-materializer for tests). Wiring
//! it into a runnable jail (`unshare` + `pivot_root`, read-only bind-mounts, the
//! privileged broker) is #109/#108; booting it as a micro-VM guest is #111. The
//! production materialization is read-only **bind-mounts** (so files are shared,
//! not copied); [`materialize_copy`] is the test/diagnostic path.
//!
//! Linux-only (it shells out to `ldd` to resolve the loader's view of the closure
//! and reads `/proc`); inert until a Tier-1.5/Tier-2 backend consumes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Caveats, NormalizationPolicy, RootfsPolicy, Scope};

// The curated runtime data paths a permitted program reads (locale, timezone, CA
// bundles, resolver/loader config, `/dev` + `/proc/self` essentials) are supplied
// by `RootfsPolicy::data_paths` (config.rs) — never executables (so they do not
// reopen the loader trampoline). The shared libraries are added *specifically*
// from the per-binary `ldd` closure, NOT by binding `/usr/lib` wholesale, so only
// the `.so`s the granted binaries actually need are present.

/// The directories a bare program name is resolved against (mirrors the loader's
/// search, `$PATH` then the configured `fallback`). Only used to find the granted
/// binaries' real paths for the plan.
fn search_dirs(fallback: &[String]) -> Vec<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        let dirs: Vec<PathBuf> = path
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }
    fallback.iter().map(PathBuf::from).collect()
}

/// Resolve a granted `exec` entry (a bare name or a path) to an absolute, existing
/// program file — canonicalized so the plan anchors the real inode.
fn resolve_program(entry: &str, search_fallback: &[String]) -> Option<PathBuf> {
    let candidate = if entry.contains('/') {
        let p = PathBuf::from(entry);
        p.is_file().then_some(p)
    } else {
        search_dirs(search_fallback)
            .into_iter()
            .map(|d| d.join(entry))
            .find(|c| c.is_file())
    }?;
    candidate.canonicalize().ok()
}

/// The shared-library closure of `bin` as the loader sees it: parse `ldd`'s output
/// for the resolved `=> /abs/path` libraries and the trailing loader line. `ldd`
/// uses `LD_TRACE_LOADED_OBJECTS` (it does not execute the target for a normal
/// dynamic binary), and the granted binaries are trusted system tools. A static
/// binary (no dynamic deps) yields an empty closure — not an error.
fn ldd_closure(bin: &Path) -> Vec<PathBuf> {
    let out = match Command::new("ldd").arg(bin).output() {
        Ok(o) if o.status.success() => o.stdout,
        // "not a dynamic executable" / static / ldd error ⇒ no closure to add.
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out);
    let mut libs = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        // "libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x...)"
        if let Some(rhs) = line.split(" => ").nth(1) {
            let path = rhs.split(" (").next().unwrap_or("").trim();
            if path.starts_with('/') {
                // Keep the SONAME path ldd reports (e.g. `libz.so.1`), NOT its
                // canonical target (`libz.so.1.3`): the dynamic loader opens the
                // soname, so the jail must expose the `.so` at that exact path or
                // the granted program fails to load (agent-bridle#113 — a
                // deny-of-function the runtime canary surfaced). A bind-mount of the
                // soname (a symlink on most distros) exposes the real file's content
                // at that name; `libc.so.6` worked before only because it is a real
                // file, not a symlink.
                let pb = PathBuf::from(path);
                if pb.exists() {
                    libs.insert(pb);
                }
            }
        } else if line.starts_with('/') {
            // The loader itself: "/lib64/ld-linux-x86-64.so.2 (0x...)".
            let path = line.split(" (").next().unwrap_or("").trim();
            if let Ok(c) = PathBuf::from(path).canonicalize() {
                libs.insert(c);
            }
        }
        // "linux-vdso.so.1 (0x...)" has no path ⇒ skipped (kernel-provided).
    }
    libs.into_iter().collect()
}

/// One path the minimal rootfs exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsEntry {
    /// The host path to expose at the same location inside the rootfs.
    pub src: PathBuf,
    /// `true` ⇒ exposed read-write (an `fs_write` root); else read-only.
    pub writable: bool,
    /// `true` ⇒ a directory mount-point (bind-mounted in production; an empty
    /// dir in [`materialize_copy`]); `false` ⇒ a single file (copied/bound).
    pub is_dir: bool,
}

/// The plan for a minimal rootfs: exactly the paths to expose, nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootfsPlan {
    /// The exposed paths (deduplicated, sorted).
    pub entries: Vec<RootfsEntry>,
}

/// Widen the static closure for known runtime-dynamic loaders (ADR 0013 D7 /
/// agent-bridle#113). `dlopen` / `ctypes` loads and glibc NSS modules are not in
/// the `ldd` closure, so a granted toolchain can fail to load a `.so` it needs at
/// runtime (a **deny-of-function**, not a safety hole). This adds their typical
/// **library/data** paths — `.so` files and stdlib data dirs, **never** `/usr/bin`
/// executables — so the D1 identity invariant (no un-granted *program* is
/// reachable) still holds: the fallback widens libraries, not the executable set.
fn add_runtime_closure_fallback(
    programs: &BTreeSet<String>,
    files: &mut BTreeSet<PathBuf>,
    ro_dirs: &mut BTreeSet<PathBuf>,
    nss_fallback: bool,
    python_fallback: bool,
) {
    // glibc `dlopen`s `libnss_*.so.N` at runtime (getpwnam, gethostbyname, …).
    // They live in libc's directory but are never in the static closure. Add them
    // from the same dir(s) as the resolved libc (canonicalized ⇒ the real `.so.N`,
    // the soname glibc actually opens; the `.so` dev symlinks are not needed).
    // Toggle: disabling only makes the rootfs *more* minimal (I7, #146).
    if nss_fallback {
        let libc_dirs: BTreeSet<PathBuf> = files
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("libc.so"))
            })
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        for dir in &libc_dirs {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("libnss_") && name.contains(".so") {
                        if let Ok(c) = entry.path().canonicalize() {
                            files.insert(c);
                        }
                    }
                }
            }
        }
    }

    // Python `dlopen`s C-extensions from its stdlib (`lib-dynload/*.so`) and reads
    // the pure-python stdlib — none of it in the static closure, and the
    // interpreter will not even start without it. When a `python*` is granted, add
    // the versioned stdlib dirs (data + `.so`; a bind-mount includes `lib-dynload`
    // and preserves internal symlinks). No `/usr/bin` is added.
    let wants_python = python_fallback
        && programs.iter().any(|p| {
            Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p)
                .starts_with("python")
        });
    if wants_python {
        for base in ["/usr/lib", "/usr/local/lib", "/usr/lib64"] {
            if let Ok(rd) = std::fs::read_dir(base) {
                for entry in rd.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("python3")
                        && entry.path().is_dir()
                    {
                        ro_dirs.insert(entry.path());
                    }
                }
            }
        }
    }
}

/// Build the minimal-rootfs plan for `effective` (ADR 0013 D2). Requires `exec` to
/// be **confined** (`Only`) — a minimal rootfs is meaningless when any program may
/// run, so an ambient `exec` is rejected (the caller falls back to the Tier-1
/// boundary). The plan contains: the resolved granted program files + each one's
/// `ldd` shared-library closure (incl. the loader) + the configured data paths +
/// the granted `fs_read` (ro) / `fs_write` (rw) roots — and nothing else.
pub fn build_rootfs_plan(
    effective: &Caveats,
    rootfs: &RootfsPolicy,
    norm: &NormalizationPolicy,
) -> Result<RootfsPlan, String> {
    let programs = match &effective.exec {
        Scope::All => {
            return Err("minimal rootfs requires a confined exec scope (exec: Only)".to_string())
        }
        Scope::Only(set) => set,
    };
    let search_fallback = &rootfs.search_dirs;

    // (src, writable, is_dir) accumulated then deduped.
    let mut files: BTreeSet<PathBuf> = BTreeSet::new(); // ro files (binaries + .so + loader + data files)
    let mut ro_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut rw_dirs: BTreeSet<PathBuf> = BTreeSet::new();

    for prog in programs {
        let bin = resolve_program(prog, search_fallback)
            .ok_or_else(|| format!("granted program not found: {prog}"))?;
        // Toggle: the static `ldd` closure. Disabling only removes `.so`s from the
        // plan (more minimal) — a granted dynamic program then fails to load, so
        // this is a capability/degradation knob, never a confinement relaxation.
        if norm.ldd_closure {
            for so in ldd_closure(&bin) {
                files.insert(so);
            }
        }
        files.insert(bin);
    }

    // D7 runtime-closure fallback (agent-bridle#113): `dlopen`/`ctypes`/NSS loads
    // are undecidable to enumerate statically, so widen the closure with known
    // dynamic **library/data** paths — never un-granted executables, so the D1
    // identity invariant still holds.
    add_runtime_closure_fallback(
        programs,
        &mut files,
        &mut ro_dirs,
        norm.nss_closure_fallback,
        norm.python_closure_fallback,
    );

    for d in rootfs.data_paths.resolve() {
        let p = PathBuf::from(&d);
        match p.metadata() {
            Ok(m) if m.is_dir() => {
                ro_dirs.insert(p);
            }
            Ok(_) => {
                files.insert(p);
            }
            Err(_) => {} // absent ⇒ skipped (harmless)
        }
    }

    // Granted fs roots: read roots ro, write roots rw (write wins on overlap).
    if let Scope::Only(rd) = &effective.fs_read {
        for p in rd {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                ro_dirs.insert(pb);
            } else if pb.exists() {
                files.insert(pb);
            }
        }
    }
    if let Scope::Only(wr) = &effective.fs_write {
        for p in wr {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                ro_dirs.remove(&pb);
                rw_dirs.insert(pb);
            } else if pb.exists() {
                files.insert(pb);
            }
        }
    }

    let mut entries: Vec<RootfsEntry> = Vec::new();
    entries.extend(files.into_iter().map(|src| RootfsEntry {
        src,
        writable: false,
        is_dir: false,
    }));
    entries.extend(ro_dirs.into_iter().map(|src| RootfsEntry {
        src,
        writable: false,
        is_dir: true,
    }));
    entries.extend(rw_dirs.into_iter().map(|src| RootfsEntry {
        src,
        writable: true,
        is_dir: true,
    }));
    entries.sort_by(|a, b| a.src.cmp(&b.src));
    Ok(RootfsPlan { entries })
}

/// Materialize a plan by **copying** file entries (and creating empty mount-point
/// dirs for directory entries) under `dest`, preserving each `src`'s absolute path
/// (so `/usr/bin/cat` lands at `dest/usr/bin/cat`). This is the **test /
/// diagnostic** path — production exposes the same plan via read-only bind-mounts
/// (no copy) through the broker (#108). Directory entries are left as empty
/// mount-points (their contents are bound at run time), so this does NOT recurse
/// into large data trees like `/usr/share`.
pub fn materialize_copy(plan: &RootfsPlan, dest: &Path) -> std::io::Result<()> {
    for e in &plan.entries {
        let rel = e.src.strip_prefix("/").unwrap_or(&e.src);
        let target = dest.join(rel);
        if e.is_dir {
            std::fs::create_dir_all(&target)?; // empty mount-point
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // A file entry may be a special file (/dev/null) — skip copy if it is
            // not a regular file, but record the mount-point's parent above.
            if e.src.is_file() {
                std::fs::copy(&e.src, &target)?;
            }
        }
    }
    Ok(())
}

/// A content-addressed cache of materialized minimal rootfs trees (#112 / ADR
/// 0013 D7). Building a rootfs (resolving the `ldd` closure + assembling the
/// tree) is the expensive step; keying it by the *(granted-binaries + resolved
/// closure)* identity lets repeated runs of the same toolchain reuse the build.
///
/// The cache stores [`materialize_copy`] trees keyed by [`RootfsCache::key`];
/// production keys the read-only bind-mount jail by the *same* key.
pub struct RootfsCache {
    root: PathBuf,
}

impl RootfsCache {
    /// A cache rooted at `root` (created on first store).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The content key for `plan`: a BLAKE3 [`crate::ContentId`] (hex) over each
    /// file entry's `(path, ro/rw, len, mtime)` and each directory mount-point's
    /// `(path, ro/rw)`. A changed granted binary or `.so` (different len/mtime) ⇒
    /// a different key ⇒ a rebuild; a changed exec scope ⇒ different paths ⇒ a
    /// different key. Directory mount-points key by path only — their contents are
    /// bind-mounted at run time, not part of the built tree.
    #[must_use]
    pub fn key(plan: &RootfsPlan) -> String {
        let mut buf = String::new();
        for e in &plan.entries {
            buf.push_str(&e.src.to_string_lossy());
            buf.push('\u{0}');
            buf.push_str(if e.writable { "rw" } else { "ro" });
            buf.push('\u{0}');
            buf.push_str(if e.is_dir { "d" } else { "f" });
            if !e.is_dir {
                if let Ok(m) = e.src.metadata() {
                    buf.push_str(&format!("\u{0}{}", m.len()));
                    if let Ok(mtime) = m.modified() {
                        if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                            buf.push_str(&format!("\u{0}{}", d.as_nanos()));
                        }
                    }
                }
            }
            buf.push('\n');
        }
        crate::ContentId::of_bytes(buf.as_bytes())
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The cached rootfs directory for `plan`, materializing it (copy) exactly
    /// once. Returns `(dir, hit)` — `hit == true` ⇒ a complete prior build was
    /// reused. A partial (interrupted) build is detected by the absence of the
    /// completion marker and rebuilt from scratch.
    pub fn get_or_materialize(&self, plan: &RootfsPlan) -> std::io::Result<(PathBuf, bool)> {
        let dir = self.root.join(Self::key(plan));
        let marker = dir.join(".bridle-rootfs-complete");
        if marker.is_file() {
            return Ok((dir, true));
        }
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?; // clear a partial/stale build
        }
        materialize_copy(plan, &dir)?;
        std::fs::create_dir_all(&dir)?; // ensure the root exists even for an empty plan
        std::fs::write(&marker, b"")?;
        Ok((dir, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "agent-bridle-rootfs-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ambient_exec_is_rejected() {
        // exec: All ⇒ no minimal rootfs (any program could run).
        let err = build_rootfs_plan(
            &Caveats::top(),
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .unwrap_err();
        assert!(err.contains("confined exec scope"), "{err}");
    }

    /// ADR 0013 D1 invariant: the built tree contains the granted program (and its
    /// library closure), and **no un-granted program** — identity by what *exists*.
    #[test]
    fn rootfs_contains_granted_program_and_not_ungranted_tools() {
        let work = unique_dir("work");
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            fs_read: Scope::only([work.to_string_lossy().into_owned()]),
            fs_write: Scope::only([work.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let plan = build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .expect("plan");

        // The granted binary and at least one library (its closure) are planned.
        let has_cat = plan
            .entries
            .iter()
            .any(|e| e.src.file_name().map(|n| n == "cat").unwrap_or(false) && !e.is_dir);
        let has_lib = plan
            .entries
            .iter()
            .any(|e| e.src.to_string_lossy().contains("/libc.so"));
        assert!(
            has_cat,
            "granted `cat` must be in the plan: {:?}",
            plan.entries
        );
        assert!(
            has_lib,
            "cat's libc closure must be in the plan: {:?}",
            plan.entries
        );

        // The writable work dir is rw; the loader is present.
        assert!(plan.entries.iter().any(|e| e.writable
            && e.is_dir
            && e.src == work.canonicalize().unwrap_or(work.clone())));
        assert!(plan
            .entries
            .iter()
            .any(|e| e.src.to_string_lossy().contains("ld-")));

        // No un-granted program/interpreter anywhere in the plan (the D1 invariant).
        for tool in [
            "/curl", "/sh", "/bash", "/python3", "/perl", "/head", "/wget", "/nc",
        ] {
            assert!(
                !plan.entries.iter().any(|e| {
                    let s = e.src.to_string_lossy();
                    s.ends_with(tool) || s.contains(&format!("/bin{tool}"))
                }),
                "un-granted tool `{tool}` must NOT be in the minimal rootfs: {:?}",
                plan.entries
            );
        }

        // Materialize (copy) and re-check on the real tree: cat present, sh absent.
        let dest = unique_dir("root");
        materialize_copy(&plan, &dest).expect("materialize");
        let cat_present = dest.join("usr/bin/cat").exists()
            || dest.join("bin/cat").exists()
            || dest.join("usr/local/bin/cat").exists();
        assert!(cat_present, "materialized tree must contain cat");
        for tool in [
            "usr/bin/sh",
            "bin/sh",
            "usr/bin/curl",
            "bin/bash",
            "usr/bin/head",
        ] {
            assert!(
                !dest.join(tool).exists(),
                "materialized minimal rootfs must NOT contain un-granted `{tool}`"
            );
        }

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&dest);
    }

    /// #144 (I5): the curated data-path list is config-driven — a `replace`d empty
    /// `data_paths` drops the built-in DATA_PATHS (e.g. `/usr/share`) from the
    /// plan, proving the builder reads the policy and not the const. Would fail on
    /// the old const path (which always injected `/usr/share`).
    #[test]
    fn rootfs_data_paths_are_config_driven() {
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            ..Caveats::top()
        };
        let has_usr_share =
            |p: &RootfsPlan| p.entries.iter().any(|e| e.src == Path::new("/usr/share"));

        // Default policy: /usr/share (a DATA_PATHS dir) is planned.
        let default_plan = build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .expect("plan");
        assert!(
            has_usr_share(&default_plan),
            "default plan must include the built-in /usr/share data dir"
        );

        // Empty, `replace`d data_paths ⇒ /usr/share is gone (the policy drives it).
        let stripped = crate::RootfsPolicy {
            data_paths: crate::PathList {
                base: vec![],
                extra: vec![],
                replace: true,
            },
            ..crate::RootfsPolicy::default()
        };
        let stripped_plan =
            build_rootfs_plan(&cav, &stripped, &crate::NormalizationPolicy::default())
                .expect("plan");
        assert!(
            !has_usr_share(&stripped_plan),
            "a replace'd empty data_paths must drop /usr/share from the plan"
        );
    }

    /// #146 (I7): the `ldd` static-closure normalization is a toggle — disabling
    /// it drops the `.so` closure from the plan (a capability/degradation knob,
    /// never a confinement relaxation: fewer files present is strictly tighter).
    /// Would fail on the old always-on path.
    #[test]
    fn rootfs_ldd_closure_is_toggleable() {
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            ..Caveats::top()
        };
        let has_lib = |p: &RootfsPlan| {
            p.entries
                .iter()
                .any(|e| e.src.to_string_lossy().contains("/libc.so"))
        };

        // Default (on): cat's libc closure is planned.
        let on = build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .expect("plan");
        assert!(has_lib(&on), "default plan must include cat's libc closure");

        // Off: the `.so` closure is gone (the toggle drives it).
        let off = crate::NormalizationPolicy {
            ldd_closure: false,
            ..crate::NormalizationPolicy::default()
        };
        let plan = build_rootfs_plan(&cav, &crate::RootfsPolicy::default(), &off).expect("plan");
        assert!(
            !has_lib(&plan),
            "disabling ldd_closure must drop the .so closure from the plan"
        );
    }

    /// #112: the cache key is stable for the same grant and varies with the
    /// granted-program set (the D7 content-key).
    #[test]
    fn cache_key_is_stable_and_varies_with_grant() {
        let work = unique_dir("ck");
        let mk = |prog: &str| Caveats {
            exec: Scope::only([prog.to_string()]),
            fs_read: Scope::only([work.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let k_cat = RootfsCache::key(
            &build_rootfs_plan(
                &mk("cat"),
                &crate::RootfsPolicy::default(),
                &crate::NormalizationPolicy::default(),
            )
            .unwrap(),
        );
        let k_cat2 = RootfsCache::key(
            &build_rootfs_plan(
                &mk("cat"),
                &crate::RootfsPolicy::default(),
                &crate::NormalizationPolicy::default(),
            )
            .unwrap(),
        );
        let k_grep = RootfsCache::key(
            &build_rootfs_plan(
                &mk("grep"),
                &crate::RootfsPolicy::default(),
                &crate::NormalizationPolicy::default(),
            )
            .unwrap(),
        );
        assert_eq!(k_cat, k_cat2, "same grant ⇒ stable key");
        assert_ne!(k_cat, k_grep, "different exec scope ⇒ different key");
        assert_eq!(k_cat.len(), 64, "hex of a 32-byte BLAKE3 content id");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// #112: the cache materializes a plan once, then reports a hit (reuse).
    #[test]
    fn cache_materializes_once_then_hits() {
        let work = unique_dir("cm");
        std::fs::write(work.join("data"), b"x").unwrap();
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            fs_read: Scope::only([work.to_string_lossy().into_owned()]),
            fs_write: Scope::only([work.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let plan = build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .expect("plan");
        let cache_root = unique_dir("cache");
        let cache = RootfsCache::new(&cache_root);

        let (dir1, hit1) = cache.get_or_materialize(&plan).expect("build");
        assert!(!hit1, "first build is a miss");
        assert!(
            dir1.join(".bridle-rootfs-complete").is_file(),
            "completion marker written"
        );
        assert!(
            dir1.join("usr/bin/cat").exists() || dir1.join("bin/cat").exists(),
            "cached tree contains the granted program"
        );

        let (dir2, hit2) = cache.get_or_materialize(&plan).expect("reuse");
        assert!(hit2, "second build is a cache hit");
        assert_eq!(dir1, dir2, "same keyed directory");

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    /// #113 / ADR 0013 D7: granting `python3` widens the closure with the python
    /// stdlib dir(s) (so startup imports and `dlopen`ed C-extensions resolve) — but
    /// adds **no un-granted executable** (`/usr/bin/*`), preserving the D1 identity
    /// invariant. Skips if python3 is not installed on the host.
    #[test]
    fn python_fallback_adds_stdlib_without_executables() {
        let work = unique_dir("py");
        let cav = Caveats {
            exec: Scope::only(["python3".to_string()]),
            fs_read: Scope::only([work.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let plan = match build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        ) {
            Ok(p) => p,
            Err(_) => return, // python3 not installed ⇒ nothing to prove
        };
        let has_py_stdlib = plan
            .entries
            .iter()
            .any(|e| e.is_dir && !e.writable && e.src.to_string_lossy().contains("/python3"));
        assert!(
            has_py_stdlib,
            "python stdlib dir must be in the plan: {:?}",
            plan.entries
        );
        // D1: the only executable file the fallback may leave in a bin dir is the
        // granted python itself — never another `/usr/bin` program.
        for e in &plan.entries {
            if e.is_dir {
                continue;
            }
            let s = e.src.to_string_lossy();
            if s.starts_with("/usr/bin") || s.starts_with("/bin") || s.contains("/sbin/") {
                let base = e.src.file_name().and_then(|n| n.to_str()).unwrap_or("");
                assert!(
                    base.starts_with("python"),
                    "fallback must not add an un-granted executable: {s}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// #113 / ADR 0013 D7: the NSS modules glibc `dlopen`s at runtime (never in the
    /// static `ldd` closure) are added for a dynamically-linked grant. Asserts only
    /// when the host actually ships them (all mainstream glibc distros do).
    #[test]
    fn nss_modules_added_to_closure() {
        let work = unique_dir("nss");
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            fs_read: Scope::only([work.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let plan = build_rootfs_plan(
            &cav,
            &crate::RootfsPolicy::default(),
            &crate::NormalizationPolicy::default(),
        )
        .expect("plan");
        let nss_in_plan = |p: &RootfsPlan| {
            p.entries.iter().any(|e| {
                e.src
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("libnss_"))
            })
        };
        // Derive libc's directory from the plan; if the host ships NSS modules
        // there, the fallback must have added them.
        if let Some(libc) = plan.entries.iter().find(|e| {
            e.src
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("libc.so"))
        }) {
            if let Some(dir) = libc.src.parent() {
                let host_has_nss = dir
                    .read_dir()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("libnss_"));
                if host_has_nss {
                    assert!(
                        nss_in_plan(&plan),
                        "NSS modules must be added to the closure: {:?}",
                        plan.entries
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&work);
    }
}
