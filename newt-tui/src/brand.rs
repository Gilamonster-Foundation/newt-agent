use std::borrow::Cow;

use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Logo assets
// ---------------------------------------------------------------------------

const LOGO_10: &str = include_str!("../../docs/logos/newt-ansi-10.txt");
pub(crate) const LOGO_20: &str = include_str!("../../docs/logos/newt-ansi-20.txt");
const LOGO_40: &str = include_str!("../../docs/logos/newt-ansi-40.txt");
const LOGO_FULL: &str = include_str!("../../docs/logos/newt-ansi-full.txt");
const LOGO_120: &str = include_str!("../../docs/logos/newt-ansi-120.txt");
const LOGO_160: &str = include_str!("../../docs/logos/newt-ansi-160.txt");

const LOGO_10_COLS: u16 = 10;
const LOGO_20_COLS: u16 = 20;
const LOGO_40_COLS: u16 = 40;
const LOGO_FULL_COLS: u16 = 80;
const LOGO_120_COLS: u16 = 126;
const LOGO_160_COLS: u16 = 166;

pub(crate) const LOGO_PLAIN: &str = include_str!("../../docs/logos/newt-ascii-40.txt");

// ---------------------------------------------------------------------------
// Brand seam
// ---------------------------------------------------------------------------
// The splash/inline art and wordmark are the compiled-in **newt** brand by
// default, but a host that reuses this airframe (the downstream
// `gilamonster-agent`) can override them at runtime — no recompile of this
// crate:
//   - `NEWT_BRAND_LOGO_DIR`    directory holding `<prefix>-<stem>.txt` art
//   - `NEWT_BRAND_LOGO_PREFIX` art filename prefix (default "newt")
//   - `NEWT_BRAND_NAME`        wordmark beside the logo (default "newt")
//   - `NEWT_BRAND_TAGLINE`     one-line tagline (default below)
// A missing/unreadable art file falls back to the compiled-in newt art, so a
// partial override (or a wrong path) degrades gracefully rather than crashing.

const DEFAULT_BRAND_NAME: &str = "newt";
const DEFAULT_BRAND_TAGLINE: &str = "Free, friendly, local agentic coder";

/// Pure core of [`brand_logo`]: resolve a logo from explicit override inputs so
/// it is testable without mutating process-wide env. A missing dir, empty dir,
/// or unreadable file all fall back to the compiled-in `default`.
fn resolve_brand_logo(
    dir: Option<std::ffi::OsString>,
    prefix: Option<String>,
    default: &'static str,
    stem: &str,
) -> Cow<'static, str> {
    let dir = match dir {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => return Cow::Borrowed(default),
    };
    let prefix = prefix
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_BRAND_NAME.to_string());
    match std::fs::read_to_string(dir.join(format!("{prefix}-{stem}.txt"))) {
        Ok(art) => Cow::Owned(art),
        Err(_) => Cow::Borrowed(default),
    }
}

/// Resolve one logo by `stem` (e.g. `"ansi-20"`), preferring a runtime override
/// file under `NEWT_BRAND_LOGO_DIR` and falling back to the compiled-in art.
pub(crate) fn brand_logo(default: &'static str, stem: &str) -> Cow<'static, str> {
    resolve_brand_logo(
        std::env::var_os("NEWT_BRAND_LOGO_DIR"),
        std::env::var("NEWT_BRAND_LOGO_PREFIX").ok(),
        default,
        stem,
    )
}

/// Pure core of the wordmark/tagline resolution: a non-empty override wins,
/// otherwise the compiled-in default.
fn brand_or(value: Option<String>, default: &str) -> String {
    value
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The wordmark printed beside the logo (`NEWT_BRAND_NAME`, default `newt`).
pub(crate) fn brand_name() -> String {
    brand_or(std::env::var("NEWT_BRAND_NAME").ok(), DEFAULT_BRAND_NAME)
}

/// The one-line tagline printed after the wordmark (`NEWT_BRAND_TAGLINE`).
pub(crate) fn brand_tagline() -> String {
    brand_or(
        std::env::var("NEWT_BRAND_TAGLINE").ok(),
        DEFAULT_BRAND_TAGLINE,
    )
}

/// Optional splash line listing the host's mounted plugins/capabilities
/// (`NEWT_BRAND_PLUGINS`, a pre-formatted list the host computes — e.g. the
/// gilamonster pilot fills it from its configured MCP capabilities). `None`
/// when unset/empty, so stock newt shows nothing.
pub(crate) fn brand_plugins() -> Option<String> {
    std::env::var("NEWT_BRAND_PLUGINS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("plugins:  {}", s.trim()))
}

/// Whether a brand logo override is active (the downstream pilot, not stock
/// newt). Gates the blank-band splash layout so the default newt splash — which
/// has no large blank bands to fill — is untouched.
pub(crate) fn brand_active() -> bool {
    std::env::var_os("NEWT_BRAND_LOGO_DIR").is_some_and(|d| !d.is_empty())
}

/// A logo art row is "blank" if no cell carries ink — every truecolor component
/// stays near the dark fill. Half-block art paints `▄` in every cell with the
/// picture in the colors, so a blank row can't be detected by glyphs; we scan
/// the `..;2;r;g;b` SGR triples instead. A bright component means ink.
pub(crate) const VERSION: &str = newt_core::build_info::VERSION_WITH_COMMIT;

pub(crate) const NEWT_ORANGE: Color = Color::Rgb(220, 60, 20);

// ---------------------------------------------------------------------------
// Splash phase
// ---------------------------------------------------------------------------

const STATUS_MIN_COLS: u16 = 44;
const LOGO_160_MIN_TERM_COLS: u16 = 260;

pub(crate) fn logo_for_size(cols: u16, rows: u16) -> (Cow<'static, str>, u16) {
    // Each entry: (art, brand stem, display_cols, display_rows, min_term_cols).
    // A logo is only selected if both width AND height fit the terminal.
    for (art, stem, w, h, min_w) in [
        (
            LOGO_160,
            "ansi-160",
            LOGO_160_COLS,
            81u16,
            LOGO_160_MIN_TERM_COLS,
        ),
        (
            LOGO_120,
            "ansi-120",
            LOGO_120_COLS,
            61u16,
            LOGO_120_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_FULL,
            "ansi-full",
            LOGO_FULL_COLS,
            40u16,
            LOGO_FULL_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_40,
            "ansi-40",
            LOGO_40_COLS,
            20u16,
            LOGO_40_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_20,
            "ansi-20",
            LOGO_20_COLS,
            10u16,
            LOGO_20_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_10,
            "ansi-10",
            LOGO_10_COLS,
            5u16,
            LOGO_10_COLS + STATUS_MIN_COLS + 2,
        ),
    ] {
        if cols >= min_w && rows >= h + 4 {
            return (brand_logo(art, stem), w);
        }
    }
    (brand_logo(LOGO_10, "ansi-10"), LOGO_10_COLS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_assets_are_embedded() {
        assert!(!LOGO_PLAIN.is_empty());
        assert!(LOGO_PLAIN.lines().count() > 5);
        for logo in [LOGO_10, LOGO_20, LOGO_40, LOGO_FULL, LOGO_120, LOGO_160] {
            assert!(!logo.is_empty());
            assert!(logo.lines().count() >= 5);
        }
    }

    #[test]
    fn logo_for_width_picks_correct_size() {
        let (_, w) = logo_for_size(LOGO_160_MIN_TERM_COLS, 999);
        assert_eq!(w, LOGO_160_COLS);

        let (_, w) = logo_for_size(LOGO_160_MIN_TERM_COLS - 1, 999);
        assert_eq!(w, LOGO_120_COLS);

        let (_, w) = logo_for_size(LOGO_120_COLS + STATUS_MIN_COLS + 1, 999);
        assert_eq!(w, LOGO_FULL_COLS);

        let (_, w) = logo_for_size(10, 999);
        assert_eq!(w, LOGO_10_COLS);
    }

    #[test]
    fn brand_logo_falls_back_to_compiled_default() {
        // No override dir (the default newt build) → the compiled-in art.
        let art = resolve_brand_logo(None, None, "NEWT-DEFAULT", "ansi-20");
        assert_eq!(art.as_ref(), "NEWT-DEFAULT");
        assert!(matches!(art, Cow::Borrowed(_)));
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn brand_logo_reads_override_file() {
        // A host (gilamonster) points the dir at its own `<prefix>-<stem>.txt`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gilamonster-ansi-20.txt"), "GILA-ART").unwrap();
        let art = resolve_brand_logo(
            Some(dir.path().as_os_str().to_owned()),
            Some("gilamonster".to_string()),
            "NEWT-DEFAULT",
            "ansi-20",
        );
        assert_eq!(art.as_ref(), "GILA-ART");
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn brand_logo_missing_override_falls_back() {
        // Override dir set but the requested stem is absent → compiled default.
        let dir = tempfile::tempdir().unwrap();
        let art = resolve_brand_logo(
            Some(dir.path().as_os_str().to_owned()),
            Some("gilamonster".to_string()),
            "NEWT-DEFAULT",
            "ansi-99",
        );
        assert_eq!(art.as_ref(), "NEWT-DEFAULT");
    }

    #[test]
    fn brand_text_prefers_nonempty_override() {
        assert_eq!(brand_or(None, "newt"), "newt");
        assert_eq!(brand_or(Some(String::new()), "newt"), "newt");
        assert_eq!(
            brand_or(Some("gilamonster".to_string()), "newt"),
            "gilamonster"
        );
    }

    #[test]
    fn brand_plugins_label_guards_empty() {
        // Mirror brand_plugins()'s pure formatting (env read aside): non-empty
        // gets the "plugins:" label; empty/whitespace yields nothing.
        let fmt = |v: Option<&str>| {
            v.map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!("plugins:  {}", s.trim()))
        };
        assert_eq!(fmt(None), None);
        assert_eq!(fmt(Some("   ")), None);
        assert_eq!(
            fmt(Some("mogul, diagram")),
            Some("plugins:  mogul, diagram".to_string())
        );
    }

    #[test]
    fn logo_widths_are_strictly_ordered() {
        // Verified at compile time — use const assert to satisfy clippy.
        const _: () = {
            assert!(LOGO_10_COLS < LOGO_20_COLS);
            assert!(LOGO_20_COLS < LOGO_40_COLS);
            assert!(LOGO_40_COLS < LOGO_FULL_COLS);
            assert!(LOGO_FULL_COLS < LOGO_120_COLS);
            assert!(LOGO_120_COLS < LOGO_160_COLS);
        };
    }

    #[test]
    fn version_constant_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
