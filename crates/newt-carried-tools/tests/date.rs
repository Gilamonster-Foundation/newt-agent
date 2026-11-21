//! #3151: the carried `date` takes GNU `-d`, which macOS's `/bin/date` rejects
//! ("illegal option -- d"), on the `%ci` form git prints.
use std::process::Command;

#[test]
fn date_parses_git_ci_with_gnu_dash_d() {
    let dir = std::env::temp_dir().join(format!("newt-carried-date-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join(format!("date{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_newt-carried-tools"), &tool).unwrap();
    let out = Command::new(&tool)
        .args(["-u", "-d", "2026-11-13 10:00:00 -0500", "+%s"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1794582000");
}
