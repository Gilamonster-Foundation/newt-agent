//! Fixed macOS spelling compatibility, never filesystem-derived authority.
//! Snapshot authentication once, before dispatch. Request matching only rewrites
//! these immutable OS aliases; it never inspects a request or stored grant.
use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

const ALIASES: [(&str, &str); 3] = [
    ("/tmp", "/private/tmp"),
    ("/var", "/private/var"),
    ("/etc", "/private/etc"),
];
static TABLE: OnceLock<Table> = OnceLock::new();

struct Table([bool; 3]);
impl Table {
    fn verify(mut trusted: impl FnMut(&str, &str) -> bool) -> Self {
        Self(ALIASES.map(|(alias, target)| trusted(alias, target)))
    }

    fn rewrite<'a>(&self, path: &'a str) -> Cow<'a, str> {
        for (enabled, (alias, target)) in self.0.iter().zip(ALIASES) {
            if *enabled
                && (path == alias
                    || path
                        .strip_prefix(alias)
                        .is_some_and(|tail| tail.starts_with('/')))
            {
                return Cow::Owned(format!("{target}{}", &path[alias.len()..]));
            }
        }
        Cow::Borrowed(path)
    }
}

fn trusted_alias(alias: &str, target: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    // Neither the alias nor the destination's parent may be replaceable by a
    // non-root writer. Do not canonicalize either name during authentication.
    let parents_immutable = ["/", "/private"].iter().all(|parent| {
        std::fs::symlink_metadata(parent)
            .is_ok_and(|m| m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0)
    });
    parents_immutable
        && std::fs::symlink_metadata(alias).is_ok_and(|m| {
            trusted_link(
                m.uid(),
                m.is_symlink(),
                std::fs::read_link(alias).ok().as_deref(),
                Path::new(target),
            )
        })
}

fn trusted_link(uid: u32, symlink: bool, target: Option<&Path>, expected: &Path) -> bool {
    // macOS ships relative link text ("private/tmp") under /. Also accept
    // the exact absolute spelling; no other lexical or filesystem resolution.
    uid == 0 && symlink && (target == Some(expected) || target == expected.strip_prefix("/").ok())
}

pub(crate) fn initialize() {
    TABLE.get_or_init(|| Table::verify(trusted_alias));
}

pub(crate) fn rewrite(path: &str) -> Cow<'_, str> {
    // Embedders that do not use the process entry point still authenticate
    // once. Unavailable or unexpected aliases remain disabled for this process.
    initialize();
    TABLE.get().expect("initialized alias table").rewrite(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_alias_rewrite_is_lexical_and_component_bounded() {
        let mut probes = 0;
        let table = Table::verify(|_, _| {
            probes += 1;
            true
        });
        for (alias, target) in ALIASES {
            for suffix in ["", "/missing/leaf", "/x/../y", "//unicode-λ"] {
                assert_eq!(
                    table.rewrite(&format!("{alias}{suffix}")),
                    format!("{target}{suffix}")
                );
            }
            assert_eq!(table.rewrite(target), target);
            let sibling = format!("{alias}-sibling/file");
            assert_eq!(table.rewrite(&sibling), sibling);
        }
        for unchanged in [
            "relative/file",
            "./tmp/file",
            "/custom/link/file",
            "/outside",
            "",
        ] {
            assert_eq!(table.rewrite(unchanged), unchanged);
        }
        assert_eq!(probes, 3, "rewrites cannot probe the filesystem again");
    }

    #[test]
    fn invalid_or_unavailable_system_alias_is_disabled() {
        let expected = Path::new("/private/tmp");
        for (uid, link, target) in [
            (1, true, Some(expected)),
            (0, false, Some(expected)),
            (0, true, None),
            (0, true, Some(Path::new("/outside"))),
        ] {
            assert!(!trusted_link(uid, link, target, expected));
        }
        assert!(trusted_link(0, true, Some(expected), expected));
        assert!(trusted_link(
            0,
            true,
            Some(Path::new("private/tmp")),
            expected
        ));
        let table = Table::verify(|alias, _| alias == "/etc");
        assert_eq!(table.rewrite("/tmp/file"), "/tmp/file");
        assert_eq!(table.rewrite("/var/file"), "/var/file");
        assert_eq!(table.rewrite("/etc/file"), "/private/etc/file");
        // Exercise the production snapshot on either Unix host; it has no
        // effect on Linux permission code, which never imports this module.
        initialize();
        assert_eq!(rewrite("/custom/file"), "/custom/file");
    }
}
