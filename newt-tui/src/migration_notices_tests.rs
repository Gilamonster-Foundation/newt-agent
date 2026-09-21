use super::*;
use newt_core::tty::Level;

/// Equal text is not an attempt identity: two failed reads inside one
/// operation, and a later operation, must all remain observable on Err.
#[test]
fn migration_host_preserves_identical_attempts_and_later_error() {
    let mut delivered = Vec::new();
    for attempts in [2, 1] {
        let result = read_with(
            |report| {
                for _ in 0..attempts {
                    report(Notice::new(Level::Warn, "", "same file: write failed"));
                }
                Err::<(), _>("later decode failure")
            },
            |notice| delivered.push(notice),
        );
        assert_eq!(result, Err("later decode failure"));
    }
    assert_eq!(delivered.len(), 3);
    assert!(delivered
        .iter()
        .all(|n| n.line() == "same file: write failed"));
}
