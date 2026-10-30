//! Fault-injection tests of the reply journal: a failing directory sync, a
//! failing fsync, the order of syncs against the first emission. They need the
//! crate-private seams (`open_with`, `open_with_append`, `sync_plan`), which
//! is why they live here and not in `tests/`: nothing fault-injecting is part
//! of the public API.

use super::*;
use crate::{CitedFile, Evidence, TaskKind};
use std::sync::{Arc, Mutex};

fn evidence() -> Evidence {
    Evidence {
        flock: "f".into(),
        event: "e".into(),
        index: 1,
        kind: TaskKind::Brief,
        from: "shep".into(),
        to: "h1".into(),
        lane: "l".into(),
        head: None,
        files: Vec::<CitedFile>::new(),
    }
}

/// A caller whose answer is no clear answer, recording each call.
fn unclear(
    log: &Arc<Mutex<Vec<String>>>,
) -> FnCaller<impl FnMut(&str, Value) -> Result<ToolReply, Lost>> {
    let log = Arc::clone(log);
    FnCaller(move |_: &str, _| {
        log.lock().unwrap().push("emit".into());
        Ok(ToolReply::Text("???".into()))
    })
}

fn recorder(log: &Arc<Mutex<Vec<String>>>) -> SyncDir {
    let log = Arc::clone(log);
    Box::new(move |p: &Path| {
        log.lock().unwrap().push(p.display().to_string());
        Ok(())
    })
}

#[test]
fn the_journal_directory_is_synced_before_any_emission_and_a_failing_sync_stops_the_open() {
    let tmp = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(vec![]));
    let mut rp =
        Replies::open_with(&tmp.path().join("j"), "h1", unclear(&log), recorder(&log)).unwrap();
    rp.send(&Reply::ack(&evidence())).unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.last().map(String::as_str), Some("emit"), "{seen:?}");
    assert!(seen.len() > 1 && seen[..seen.len() - 1].iter().all(|x| x != "emit"));

    let failing = Replies::open_with(
        &tmp.path().join("other"),
        "h1",
        unclear(&log),
        Box::new(|_: &Path| Err(std::io::Error::other("disk"))),
    );
    assert!(failing.is_err(), "an unsynced journal must not open");
}

/// Existence is not consulted: every open syncs the journal directory and each
/// ancestor up to `/`, child first, before any emission.
#[test]
fn every_open_syncs_the_whole_chain_before_any_emission() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    // "another opener" already made the first level
    std::fs::create_dir_all(base.join("a")).unwrap();
    let dir = base.join("a").join("b");
    let log = Arc::new(Mutex::new(vec![]));
    let mut rp = Replies::open_with(&dir, "h1", unclear(&log), recorder(&log)).unwrap();
    rp.send(&Reply::ack(&evidence())).unwrap();
    let want: Vec<String> = dir
        .ancestors()
        .map(|p| p.display().to_string())
        .chain(["emit".to_string()])
        .collect();
    assert_eq!(*log.lock().unwrap(), want, "child first, up to the root");
}

/// A sync that fails stops the open (nothing is emitted); the retry syncs the
/// whole chain again, though every directory now exists.
#[test]
fn a_failed_sync_fails_the_open_and_the_retry_syncs_everything_again() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("x").join("y");
    let first = Arc::new(Mutex::new(true));
    let f = Arc::clone(&first);
    let log = Arc::new(Mutex::new(vec![]));
    let failed = Replies::open_with(
        &dir,
        "h1",
        unclear(&log),
        Box::new(move |_: &Path| {
            if std::mem::replace(&mut *f.lock().unwrap(), false) {
                Err(std::io::Error::other("disk"))
            } else {
                Ok(())
            }
        }),
    );
    assert!(failed.is_err());
    assert!(log.lock().unwrap().is_empty(), "nothing was emitted");
    let rp = Replies::open_with(&dir, "h1", unclear(&log), recorder(&log));
    assert!(rp.is_ok(), "the failed open left the journal locked");
    let want: Vec<String> = dir.ancestors().map(|p| p.display().to_string()).collect();
    assert_eq!(*log.lock().unwrap(), want);
}

/// A one-component relative directory has the empty path as its parent, which
/// cannot be synced, so the plan never names it. (Pure, so no test changes the
/// process's directory; `open` adds the resolved chain.)
#[test]
fn a_relative_dir_never_syncs_the_empty_path() {
    assert_eq!(
        sync_plan(Path::new("replies")),
        [Path::new("replies").to_path_buf()]
    );
    assert_eq!(
        sync_plan(Path::new("a/b")),
        [Path::new("a/b").to_path_buf(), Path::new("a").to_path_buf()]
    );
}

/// A symlinked ancestor: the lexical chain names the alias, but the entries
/// that must be durable are in the physical parents, so the resolved chain is
/// synced too, before any emission.
#[test]
fn a_symlinked_ancestor_has_its_physical_chain_synced_before_any_emission() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap().join("base");
    let physical = base.join("physical");
    let parent = physical.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    symlink(&parent, base.join("alias")).unwrap();
    let dir = base.join("alias").join("journal");

    let log = Arc::new(Mutex::new(vec![]));
    let mut rp = Replies::open_with(&dir, "h1", unclear(&log), recorder(&log)).unwrap();
    rp.send(&Reply::ack(&evidence())).unwrap();
    let seen = log.lock().unwrap().clone();
    let at = |p: &Path| seen.iter().position(|x| *x == p.display().to_string());
    let emit = seen.iter().position(|x| x == "emit").unwrap();
    for p in [
        parent.join("journal"),
        parent.clone(),
        physical.clone(),
        base.clone(),
    ] {
        assert!(
            at(&p).is_some_and(|i| i < emit),
            "{} not synced before emission: {seen:?}",
            p.display()
        );
    }
    assert!(at(&dir).is_some(), "the lexical chain is kept: {seen:?}");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        seen.len(),
        "a directory synced twice: {seen:?}"
    );
}

/// A failed append poisons the journal. The head is not advanced, a retry on
/// the same object writes and emits nothing, and a reopen (verify + tail
/// recovery) is the only way back, with the chain intact.
#[test]
fn a_failed_append_poisons_the_journal_until_it_is_reopened() {
    use std::io::Write as _;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("j");
    let log = Arc::new(Mutex::new(vec![]));
    let one_shot = Arc::new(Mutex::new(true));
    let fault = Arc::clone(&one_shot);
    let mut rp = Replies::open_with_append(
        &dir,
        "h1",
        unclear(&log),
        Box::new(|_: &Path| Ok(())),
        // the line reaches the file, then the fsync fails: the file and the
        // in-memory head now disagree unless the journal refuses to go on
        Box::new(move |f: &mut std::fs::File, bytes: &[u8]| {
            f.write_all(bytes)?;
            if std::mem::replace(&mut *fault.lock().unwrap(), false) {
                return Err(std::io::Error::other("fsync"));
            }
            f.sync_all()
        }),
    )
    .unwrap();
    let ev = evidence();
    assert!(
        rp.send(&Reply::ack(&ev)).is_err(),
        "the failed append is an error"
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "nothing left before the intent"
    );
    let before = std::fs::read(dir.join("replies.jsonl")).unwrap();
    // the fault is spent, yet the same object refuses to go on
    let again = rp.send(&Reply::result(&ev, true).unwrap());
    assert!(
        again.err().is_some_and(|e| e.contains("reopen")),
        "poisoned"
    );
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(dir.join("replies.jsonl")).unwrap(), before);
    drop(rp);

    // reopening verifies the file (the intent line did land) and works again
    let mut rp = Replies::open(&dir, "h1", unclear(&log)).unwrap();
    rp.send(&Reply::result(&ev, true).unwrap()).unwrap();
    drop(rp);
    let text = std::fs::read_to_string(dir.join("replies.jsonl")).unwrap();
    let geneses = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<Value>(l).unwrap()["node"]["parents"]
                .as_array()
                .is_some_and(Vec::is_empty)
        })
        .count();
    assert_eq!(geneses, 1, "a second genesis forked the chain");
    Replies::open(&dir, "h1", unclear(&log)).unwrap(); // the whole chain verifies
}
