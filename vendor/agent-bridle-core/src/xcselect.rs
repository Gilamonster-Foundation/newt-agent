//! Recognizer for the macOS Xcode developer-tool locator stub (ADR 0011 D6,
//! agent-bridle#449).
//!
//! `/usr/bin/git`, `python3`, `clang`, … are one hard-linked inode (78 links on
//! macOS 15) that links `/usr/lib/libxcselect.dylib` and only re-execs the
//! selected toolchain's real tool — a re-exec Seatbelt's `process-exec*` checks.
//! Its link count therefore does not make it a multicall. Recognition is narrow
//! and fail-closed: anything failing any check stays a multi-link multicall.

// The parser is exercised by unit tests everywhere but called only on macOS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Whether `real` (a canonical path) is the Xcode locator stub. Always `false`
/// off macOS.
#[cfg(target_os = "macos")]
pub(crate) fn is_xcselect_stub(real: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // One open fd for every check, so they all describe the same object.
    let Ok(mut f) = std::fs::File::open(real) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    let read_only = rustix::fs::fstatvfs(&f)
        .is_ok_and(|v| v.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY));
    meta.is_file()
        && stub_shape(real, meta.uid(), meta.mode(), read_only)
        && macho_imports_xcselect(&mut f)
}

/// Always `false` off macOS: there is no locator stub to exempt.
#[cfg(not(target_os = "macos"))]
pub(crate) fn is_xcselect_stub(_real: &Path) -> bool {
    false
}

/// The stub's file facts: directly in `/usr/bin`, root-owned, no group/other
/// write, on a read-only (sealed system) volume.
fn stub_shape(real: &Path, uid: u32, mode: u32, read_only_volume: bool) -> bool {
    real.parent() == Some(Path::new("/usr/bin"))
        && uid == 0
        && mode & 0o022 == 0
        && read_only_volume
}

const XCSELECT: &[u8] = b"/usr/lib/libxcselect.dylib";
const FAT_MAGIC: u32 = 0xcafe_babe;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_EXECUTE: u32 = 2;
/// Bound on the load-command region read per slice.
const MAX_SIZEOFCMDS: u32 = 64 * 1024;
/// `LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`,
/// `LC_LAZY_LOAD_DYLIB`, `LC_LOAD_UPWARD_DYLIB`.
const DYLIB_CMDS: &[u32] = &[0xc, 0x8000_0018, 0x8000_001f, 0x20, 0x8000_0023];

/// Every Mach-O slice of `f` (thin 64-bit, or a 32-bit-offset universal file) is
/// an executable whose load commands import [`XCSELECT`]. Reads only headers and
/// load commands.
fn macho_imports_xcselect(f: &mut (impl Read + Seek)) -> bool {
    let mut b = [0u8; 8];
    if f.read_exact(&mut b).is_err() {
        return false;
    }
    if be(&b, 0) != FAT_MAGIC {
        return slice_imports_xcselect(f, 0);
    }
    let n = be(&b, 4);
    if n == 0 || n > 8 {
        return false;
    }
    let mut arches = vec![0u8; n as usize * 20];
    if f.read_exact(&mut arches).is_err() {
        return false;
    }
    arches
        .chunks(20)
        .all(|a| slice_imports_xcselect(f, u64::from(be(a, 8))))
}

fn slice_imports_xcselect(f: &mut (impl Read + Seek), base: u64) -> bool {
    let mut h = [0u8; 32];
    if f.seek(SeekFrom::Start(base)).is_err() || f.read_exact(&mut h).is_err() {
        return false;
    }
    let (ncmds, size) = (le(&h, 16), le(&h, 20));
    if le(&h, 0) != MH_MAGIC_64 || le(&h, 12) != MH_EXECUTE || size > MAX_SIZEOFCMDS {
        return false;
    }
    let mut cmds = vec![0u8; size as usize];
    if f.read_exact(&mut cmds).is_err() {
        return false;
    }
    let mut off = 0usize;
    for _ in 0..ncmds {
        if off + 8 > cmds.len() {
            return false;
        }
        let (cmd, len) = (le(&cmds, off), le(&cmds, off + 4) as usize);
        let Some(lc) = cmds.get(off..off + len).filter(|_| len >= 8) else {
            return false;
        };
        if DYLIB_CMDS.contains(&cmd) && lc.len() >= 12 {
            let name = lc.get(le(lc, 8) as usize..).unwrap_or_default();
            if name.split(|&c| c == 0).next() == Some(XCSELECT) {
                return true;
            }
        }
        off += len;
    }
    false
}

fn be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A thin 64-bit `MH_EXECUTE` image whose one dylib command names `dylib`.
    fn thin(dylib: &[u8]) -> Vec<u8> {
        let mut lc = Vec::new();
        let name_off = 24u32;
        let len = (24 + dylib.len() + 1).next_multiple_of(8) as u32;
        for w in [0xcu32, len, name_off, 0, 0, 0] {
            lc.extend(w.to_le_bytes());
        }
        lc.extend(dylib);
        lc.resize(len as usize, 0);
        let mut img = Vec::new();
        for w in [MH_MAGIC_64, 0x0100_000c, 0, MH_EXECUTE, 1, len, 0, 0] {
            img.extend(w.to_le_bytes());
        }
        img.extend(lc);
        img
    }

    /// A universal file wrapping `slices`.
    fn fat(slices: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(FAT_MAGIC.to_be_bytes());
        out.extend((slices.len() as u32).to_be_bytes());
        let mut off = 4096u32;
        for s in slices {
            for w in [0u32, 0, off, s.len() as u32, 12] {
                out.extend(w.to_be_bytes());
            }
            off += (s.len() as u32).next_multiple_of(4096);
        }
        for s in slices {
            out.resize(out.len().next_multiple_of(4096), 0);
            out.extend(s);
        }
        out
    }

    fn imports(img: Vec<u8>) -> bool {
        macho_imports_xcselect(&mut Cursor::new(img))
    }

    /// #449: the locator stub is recognized only when every fact holds — the
    /// shape (dir, owner, mode, sealed volume) and a Mach-O import of
    /// libxcselect in every slice. Any miss keeps the multi-link Interceptor.
    #[test]
    fn xcselect_stub_recognizer_is_narrow_and_fail_closed() {
        let stub = thin(XCSELECT);
        assert!(imports(stub.clone()), "thin stub");
        assert!(
            imports(fat(&[stub.clone(), stub.clone()])),
            "universal stub"
        );
        assert!(
            !imports(thin(b"/usr/lib/libSystem.B.dylib")),
            "missing libxcselect"
        );
        assert!(
            !imports(thin(b"/usr/lib/libxcselect.dylib.evil")),
            "prefix only"
        );
        assert!(
            !imports(fat(&[stub.clone(), thin(b"/usr/lib/libSystem.B.dylib")])),
            "one slice without libxcselect"
        );
        assert!(!imports(b"#!/bin/sh\nexit 0\n".to_vec()), "not Mach-O");
        assert!(!imports(stub[..40].to_vec()), "truncated load commands");

        let git = Path::new("/usr/bin/git");
        assert!(stub_shape(git, 0, 0o100_755, true));
        assert!(
            !stub_shape(Path::new("/usr/local/bin/git"), 0, 0o100_755, true),
            "wrong dir"
        );
        assert!(
            !stub_shape(Path::new("/usr/bin/x/git"), 0, 0o100_755, true),
            "nested dir"
        );
        assert!(!stub_shape(git, 501, 0o100_755, true), "non-root owner");
        assert!(!stub_shape(git, 0, 0o100_775, true), "group-writable");
        assert!(!stub_shape(git, 0, 0o100_757, true), "other-writable");
        assert!(!stub_shape(git, 0, 0o100_755, false), "writable volume");
    }

    /// A generic multi-link file is not the stub and stays a multicall grant.
    #[cfg(unix)]
    #[test]
    fn generic_multi_link_file_is_not_the_stub() {
        let dir = std::env::temp_dir().join(format!("agent-bridle-449-xc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("git");
        std::fs::write(&a, thin(XCSELECT)).unwrap();
        std::fs::hard_link(&a, dir.join("python3")).unwrap();
        let real = a.canonicalize().unwrap();
        assert!(!is_xcselect_stub(&real));
        let cav = crate::Caveats {
            exec: crate::Scope::only([real.to_string_lossy().into_owned()]),
            ..crate::Caveats::top()
        };
        assert!(crate::sandbox::exec_grant_is_multicall(&cav));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On macOS the real `/usr/bin/git` and `python3` are the stub.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_locator_stub_is_recognized() {
        for n in ["git", "python3"] {
            let real = Path::new("/usr/bin").join(n).canonicalize().unwrap();
            assert!(is_xcselect_stub(&real), "{real:?}");
        }
    }
}
