#[test]
fn terminal_fd_available_uses_platform_null_device() {
    assert!(
        super::terminal_fd_available(),
        "terminal probe should open the platform null device on a healthy process"
    );
}
