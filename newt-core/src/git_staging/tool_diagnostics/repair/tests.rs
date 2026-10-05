use super::*;
use std::cell::RefCell;
use std::rc::Rc;

type Object = Rc<RefCell<Snapshot>>;
type Parent = Rc<RefCell<Object>>;
struct FakeFs {
    named_parent: RefCell<Parent>,
}
fn object(inode: u64) -> Object {
    Rc::new(RefCell::new(Snapshot {
        identity: ObjectIdentity {
            object: RootIdentity { device: 1, inode },
            kind: ObjectKind::Directory,
            owner: 1000,
            group: 1000,
        },
        mode: 0o40775,
    }))
}
impl FileSystem for FakeFs {
    type Parent = Parent;
    type Object = Object;
    fn named_metadata(&self, _: &Path) -> io::Result<Snapshot> {
        self.metadata(&self.named_parent.borrow().borrow())
    }
    fn hold_parent(&self, _: &Path) -> io::Result<Parent> {
        Ok(self.named_parent.borrow().clone())
    }
    fn open(&self, parent: &Parent, _: &Path) -> io::Result<Object> {
        Ok(parent.borrow().clone())
    }
    fn metadata(&self, object: &Object) -> io::Result<Snapshot> {
        Ok(*object.borrow())
    }
    fn set_mode(&self, object: &Object, mode: u32) -> io::Result<()> {
        object.borrow_mut().mode = mode;
        Ok(())
    }
}
fn fixture() -> (FakeFs, Object, TrustHint) {
    let original = object(10);
    let fs = FakeFs {
        named_parent: RefCell::new(Rc::new(RefCell::new(original.clone()))),
    };
    let hint = TrustHint {
        path: "/shared/bin/tool-dir".into(),
        mode: 0o40775,
        chmod_arg: "g-w",
    };
    (fs, original, hint)
}

/// #2739 round 2: replacing an ancestor must not redirect an approved chmod.
#[test]
fn parent_swap_never_changes_the_unrelated_object() {
    let (fs, original, hint) = fixture();
    let repair = BoundRepair::capture(&hint, &fs).unwrap();
    let unrelated = object(20);
    *fs.named_parent.borrow_mut() = Rc::new(RefCell::new(unrelated.clone()));
    repair.apply(&hint, &fs).unwrap();
    assert_eq!(unrelated.borrow().mode & 0o777, 0o775);
    assert_eq!(original.borrow().mode & 0o777, 0o755);
}

/// #2739 round 2: a different leaf beneath the held parent is refused.
#[test]
fn leaf_replacement_refuses_without_changing_either_object() {
    let (fs, original, hint) = fixture();
    let repair = BoundRepair::capture(&hint, &fs).unwrap();
    let unrelated = object(20);
    *fs.named_parent.borrow().borrow_mut() = unrelated.clone();
    let error = repair.apply(&hint, &fs).unwrap_err();
    assert!(error.to_string().contains("re-run `newt doctor`"));
    assert_eq!(unrelated.borrow().mode & 0o777, 0o775);
    assert_eq!(original.borrow().mode & 0o777, 0o775);
}

/// #2739 round 2: identity validation must include device, type and ownership.
#[test]
fn changed_identity_fields_refuse_before_chmod() {
    for field in 0..4 {
        let (fs, original, hint) = fixture();
        let repair = BoundRepair::capture(&hint, &fs).unwrap();
        match field {
            0 => original.borrow_mut().identity.object.device += 1,
            1 => original.borrow_mut().identity.kind = ObjectKind::File,
            2 => original.borrow_mut().identity.owner += 1,
            _ => original.borrow_mut().identity.group += 1,
        }
        assert!(repair.apply(&hint, &fs).is_err());
        assert_eq!(original.borrow().mode & 0o777, 0o775);
    }
}

/// #2739 round 2: a matching object can still be repaired, preserving other bits.
#[test]
fn successful_repair_preserves_other_permissions() {
    let (fs, original, hint) = fixture();
    let repair = BoundRepair::capture(&hint, &fs).unwrap();
    repair.apply(&hint, &fs).unwrap();
    assert_eq!(original.borrow().mode, 0o40755);
}

/// Grounds the injected parent/leaf swap model in actual descriptor-relative
/// opens and fchmod. Own temporary directories only; outside the unit tier.
#[test]
#[ignore = "real filesystem permission repair; run explicitly in the native lane"]
fn native_parent_symlink_and_leaf_replacement_preserve_unrelated_modes() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let moved = temp.path().join("moved");
    let unrelated = temp.path().join("unrelated");
    std::fs::create_dir_all(parent.join("tool")).unwrap();
    std::fs::create_dir_all(unrelated.join("tool")).unwrap();
    let make_hint = |path: PathBuf| {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o775)).unwrap();
        TrustHint {
            mode: path.metadata().unwrap().mode(),
            path,
            chmod_arg: "g-w",
        }
    };
    let mode = |path: &Path| path.metadata().unwrap().mode() & 0o777;
    let hint = make_hint(parent.join("tool"));
    let other = make_hint(unrelated.join("tool"));
    let bound = BoundRepair::capture(&hint, &Host).unwrap();
    std::fs::rename(&parent, &moved).unwrap();
    std::os::unix::fs::symlink(&unrelated, &parent).unwrap();
    bound.apply(&hint, &Host).unwrap();
    assert_eq!(mode(&other.path), 0o775);
    assert_eq!(mode(&moved.join("tool")), 0o755);

    let leaf = moved.join("second");
    std::fs::create_dir(&leaf).unwrap();
    let hint = make_hint(leaf.clone());
    let bound = BoundRepair::capture(&hint, &Host).unwrap();
    let saved = moved.join("saved");
    std::fs::rename(&leaf, &saved).unwrap();
    std::fs::create_dir(&leaf).unwrap();
    let _replacement = make_hint(leaf.clone());
    assert!(bound.apply(&hint, &Host).is_err());
    assert_eq!(mode(&saved), 0o775);
    assert_eq!(mode(&leaf), 0o775);
}
