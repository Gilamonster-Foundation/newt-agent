//! PATH lookup for the carried `which` command (#2795), without spawning a shell.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

fn lookup(
    name: &OsStr,
    directories: &[PathBuf],
    extensions: &[OsString],
    windows: bool,
    executable: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let name_path = Path::new(name);
    let explicit = name_path.components().count() > 1 || name_path.is_absolute();
    let bases = if explicit {
        vec![name_path.to_path_buf()]
    } else {
        directories.iter().map(|dir| dir.join(name)).collect()
    };
    let mut matches = Vec::new();
    for base in bases {
        let candidates = if windows {
            let recognized = base.extension().is_some_and(|ext| {
                extensions.iter().any(|suffix| {
                    suffix
                        .to_string_lossy()
                        .trim_start_matches('.')
                        .eq_ignore_ascii_case(&ext.to_string_lossy())
                })
            });
            if recognized {
                vec![base]
            } else {
                extensions
                    .iter()
                    .map(|suffix| {
                        let mut candidate = base.as_os_str().to_os_string();
                        candidate.push(suffix);
                        PathBuf::from(candidate)
                    })
                    .collect()
            }
        } else {
            vec![base]
        };
        for candidate in candidates {
            if executable(&candidate) && !matches.contains(&candidate) {
                matches.push(candidate);
            }
        }
    }
    matches
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn run(args: &[OsString]) -> i32 {
    let mut all = false;
    let mut options = true;
    let mut names = Vec::new();
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg == "-a" {
            all = true;
        } else if options && arg.to_string_lossy().starts_with('-') {
            eprintln!("which: supported: [-a] [--] command ...");
            return 2;
        } else {
            names.push(arg);
        }
    }
    if names.is_empty() {
        eprintln!("which: supported: [-a] [--] command ...");
        return 2;
    }
    let directories: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let extensions: Vec<_> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|ext| ext.starts_with('.') && ext.len() > 1)
        .map(OsString::from)
        .collect();
    let mut status = 0;
    for name in names {
        let matches = lookup(
            name,
            &directories,
            &extensions,
            cfg!(windows),
            is_executable,
        );
        if matches.is_empty() {
            status = 1;
        }
        for path in matches.iter().take(if all { usize::MAX } else { 1 }) {
            println!("{}", path.display());
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }
    fn extensions() -> Vec<OsString> {
        [".EXE", ".CMD"].iter().map(OsString::from).collect()
    }
    /// #2795: resolve cargo using PATH order and PATHEXT, retaining all matches.
    #[test]
    fn path_and_pathext_order() {
        let found = paths(&["first/cargo.CMD", "second/cargo.EXE"]);
        assert_eq!(
            lookup(
                OsStr::new("cargo"),
                &paths(&["first", "second"]),
                &extensions(),
                true,
                |p| found.contains(&p.to_path_buf())
            ),
            found
        );
    }
    /// #2795: explicit extensions and paths must not be suffixed or searched again.
    #[test]
    fn explicit_path_and_extension() {
        let found = paths(&["bin/cargo.EXE"]);
        assert_eq!(
            lookup(
                OsStr::new("bin/cargo.EXE"),
                &paths(&["wrong"]),
                &extensions(),
                true,
                |p| found.contains(&p.to_path_buf())
            ),
            found
        );
    }
    /// #2795: Unix uses executable bits, not Windows suffix expansion.
    #[test]
    fn unix_and_missing() {
        let found = paths(&["second/cargo"]);
        assert_eq!(
            lookup(
                OsStr::new("cargo"),
                &paths(&["first", "second"]),
                &extensions(),
                false,
                |p| found.contains(&p.to_path_buf())
            ),
            found
        );
        assert!(lookup(
            OsStr::new("missing"),
            &paths(&["first"]),
            &extensions(),
            true,
            |_| false
        )
        .is_empty());
    }
}
