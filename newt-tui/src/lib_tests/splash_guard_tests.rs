use super::RestoreOnDrop;
use std::cell::Cell;

/// The ordinary path: the scope ends, the terminal comes back.
#[test]
fn restore_runs_on_normal_scope_exit() {
    let ran = Cell::new(0);
    {
        let _g = RestoreOnDrop {
            restore: || ran.set(ran.get() + 1),
        };
    }
    assert_eq!(ran.get(), 1, "restore must run exactly once on normal exit");
}

/// The path that actually bit: an inner `?` returns early, jumping over
/// every statement below it. `show_splash(..)?` is this shape.
#[test]
fn restore_runs_when_an_inner_question_mark_returns_early() {
    let ran = Cell::new(0);

    fn splash_body(ran: &Cell<u32>) -> std::io::Result<()> {
        let _g = RestoreOnDrop {
            restore: || ran.set(ran.get() + 1),
        };
        // Stands in for a failing `show_splash`.
        Err(std::io::Error::other("splash step failed"))?;
        unreachable!("the ? above returns");
    }

    assert!(splash_body(&ran).is_err());
    assert_eq!(
        ran.get(),
        1,
        "restore must run on the error path — this is the #1411 leak: an I/O \
             error inside the splash used to skip disable_raw_mode + \
             LeaveAlternateScreen and strand the operator in a hidden-cursor, \
             non-echoing terminal"
    );
}

/// A panic inside the splash must also give the terminal back, or the
/// process dies leaving the operator's shell unusable.
#[test]
fn restore_runs_while_unwinding_a_panic() {
    let ran = std::sync::Arc::new(std::sync::Mutex::new(0u32));
    let seen = std::sync::Arc::clone(&ran);

    let result = std::panic::catch_unwind(move || {
        let _g = RestoreOnDrop {
            restore: || *seen.lock().unwrap() += 1,
        };
        panic!("splash panicked");
    });

    assert!(result.is_err(), "the panic propagates");
    assert_eq!(
        *ran.lock().unwrap(),
        1,
        "restore must run during unwind — Drop is the only mechanism that \
             covers this path at all"
    );
}

/// NEGATIVE CONTROL — the defect, preserved as an executable statement.
///
/// The repo's regression rule asks for a test that fails before the fix.
/// Taken literally that is impossible here: the fix *is* the introduction of
/// a type, so any test naming `RestoreOnDrop` cannot compile against the old
/// code. This test closes that gap honestly by modelling the pre-#1411
/// control flow directly — restore as a trailing statement instead of a
/// `Drop` — and asserting it leaks.
///
/// If someone later "simplifies" the guard back into a trailing call, the
/// test above (`restore_runs_when_an_inner_question_mark_returns_early`)
/// starts failing and this one explains why.
#[test]
fn the_pre_fix_shape_skips_restore_on_the_error_path() {
    // Exactly the old block: take the terminal, do fallible work, give it
    // back at the bottom. The `?` jumps over the giving-back.
    fn pre_fix_splash_body(restored: &Cell<u32>) -> std::io::Result<()> {
        // enable_raw_mode()? + EnterAlternateScreen happened here.
        Err(std::io::Error::other("splash step failed"))?;
        // …and this is the `disable_raw_mode()` / `LeaveAlternateScreen`
        // pair at lib.rs:289-290 that control flow never reaches.
        restored.set(restored.get() + 1);
        Ok(())
    }

    let restored = Cell::new(0);
    assert!(pre_fix_splash_body(&restored).is_err());
    assert_eq!(
        restored.get(),
        0,
        "this is the bug #1411 fixes: the terminal was never restored on the \
             error path, so the operator was left in the alternate screen with \
             raw mode on and the cursor hidden"
    );
}

/// Guards nest (the splash sits inside the process, and #1408 C1 will nest
/// more), so restores must run innermost-first and exactly once each.
#[test]
fn nested_guards_restore_in_reverse_order_exactly_once() {
    let order = std::cell::RefCell::new(Vec::new());
    {
        let _outer = RestoreOnDrop {
            restore: || order.borrow_mut().push("outer"),
        };
        {
            let _inner = RestoreOnDrop {
                restore: || order.borrow_mut().push("inner"),
            };
        }
    }
    assert_eq!(*order.borrow(), vec!["inner", "outer"]);
}
