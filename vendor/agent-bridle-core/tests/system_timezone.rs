//! Real-resource grounding for `SandboxPolicy`'s timezone read defaults. The
//! configured path-list unit test cannot prove libc follows `/etc/localtime`
//! symlinks or that a shell's children see the same timezone under confinement.
//! These proofs run in the existing Linux/macOS all-features CI lanes.

#![cfg(any(
    all(target_os = "linux", feature = "linux-landlock"),
    all(target_os = "macos", feature = "macos-seatbelt")
))]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use agent_bridle_core::{
    Caveats, ConfinedCommand, Gate, SandboxKind, Scope, Tool, ToolContext, ToolResult,
};

struct Probe;

// macOS /bin/sh is a dispatcher that re-execs the selected shell. Pin the
// actual interpreter so this timezone proof does not depend on that exec shim.
const SHELL: &str = if cfg!(target_os = "macos") {
    "/bin/bash"
} else {
    "/bin/sh"
};

#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "timezone_probe"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

fn supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        assert!(agent_bridle_core::seatbelt_is_supported());
        true
    }
    #[cfg(target_os = "linux")]
    {
        let available = agent_bridle_core::landlock_is_supported();
        assert!(
            available || std::env::var_os("BRIDLE_REQUIRE_LANDLOCK").is_none(),
            "CI requires an actual Landlock timezone proof"
        );
        if !available {
            eprintln!("skipping timezone proof: Landlock unavailable");
        }
        available
    }
}

fn confined(program: &str, args: &[&str], timezone: Option<&str>) -> Output {
    let caveats = Caveats {
        fs_read: Scope::none(),
        fs_write: Scope::none(),
        exec: Scope::only(["/bin/cat".into(), "/bin/date".into(), SHELL.into()]),
        // 0.8 admission refuses `net: none` under default Landlock (io_uring
        // cannot be bounded); network is irrelevant to timezone reads.
        ..Caveats::top()
    };
    let cx = Gate::new(0).authorize(&Probe, &caveats).unwrap();
    let mut command = ConfinedCommand::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(timezone) = timezone {
        command = command.env("TZ", timezone);
    }
    let child = command.spawn(&cx).expect("confined timezone probe");
    #[cfg(target_os = "macos")]
    assert_eq!(child.sandbox_kind, SandboxKind::Seatbelt);
    #[cfg(target_os = "linux")]
    assert_eq!(child.sandbox_kind, SandboxKind::Landlock);
    child.child.wait_with_output().unwrap()
}

#[test]
fn system_timezone_files_are_readable_with_no_user_file_grants() {
    if !supported() {
        return;
    }
    // Checking bytes fails on the old macOS policy even on a UTC CI host, where
    // comparing localtime conversions alone could accidentally pass.
    for path in ["/etc/localtime", "/usr/share/zoneinfo/America/New_York"] {
        let expected = std::fs::read(path).expect("host timezone data must exist");
        assert!(!expected.is_empty());
        let result = confined("/bin/cat", &[path], None);
        assert!(result.status.success(), "system timezone read denied");
        assert_eq!(result.stdout, expected, "child must read actual TZif data");
    }
}

#[test]
fn localtime_and_explicit_timezone_match_parent_through_shell_descendants() {
    if !supported() {
        return;
    }
    for timezone in [None, Some("America/New_York"), Some("UTC-14"), Some("")] {
        // Fixed winter and summer instants exercise DST without a wall-clock
        // race. Preserve unset vs empty TZ; empty means UTC on these runtimes.
        for epoch in ["1705320000", "1721044800"] {
            #[cfg(target_os = "macos")]
            let args = vec!["-r".to_string(), epoch.to_string(), "+%FT%T%z".to_string()];
            #[cfg(target_os = "linux")]
            let args = vec![format!("--date=@{epoch}"), "+%FT%T%z".to_string()];
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut parent = Command::new("/bin/date");
            parent.args(&args).env_clear();
            if let Some(timezone) = timezone {
                parent.env("TZ", timezone);
            }
            let expected = parent.output().unwrap();
            assert!(expected.status.success());
            let direct = confined("/bin/date", &refs, timezone);
            assert!(direct.status.success());
            assert_eq!(
                direct.stdout, expected.stdout,
                "direct child TZ={timezone:?}"
            );

            let script = format!("/bin/date {}; status=$?; exit \"$status\"", args.join(" "));
            let nested = confined(SHELL, &["-c", &script], timezone);
            assert!(
                nested.status.success(),
                "nested date failed: {}",
                String::from_utf8_lossy(&nested.stderr)
            );
            assert_eq!(
                nested.stdout, expected.stdout,
                "shell descendant TZ={timezone:?}"
            );
        }
    }
}

#[test]
fn explicit_timezone_path_does_not_grant_an_unrelated_file() {
    if !supported() {
        return;
    }
    let directory = std::env::temp_dir().join(format!(
        "agent-bridle-timezone-ungranted-{}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("isolated timezone fixture");
    struct Cleanup<'a>(&'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0);
        }
    }
    let _cleanup = Cleanup(&directory);
    let path = directory.join("custom-zone");
    std::fs::copy("/etc/localtime", &path).unwrap();
    let path = path.to_str().unwrap();
    let timezone = format!(":{path}");
    let result = confined("/bin/cat", &[path], Some(&timezone));
    assert!(
        !result.status.success(),
        "TZ must not mint a filesystem grant"
    );
    assert!(
        result.stdout.is_empty(),
        "ungranted timezone file must remain unreadable"
    );
}
