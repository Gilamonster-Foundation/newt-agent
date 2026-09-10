use agent_harness::store::FrameStore;
use content_addressable::{ContentAddressable, ContentId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Record {
    value: String,
}
impl ContentAddressable for Record {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

#[test]
fn memory_roundtrip_checks_sources_and_nodes() {
    let mut store = FrameStore::memory();
    let source = store.put_source(b"retained bytes").unwrap();
    assert_eq!(store.source(&source).unwrap(), b"retained bytes");
    let record = Record {
        value: "recorded".into(),
    };
    let id = store.put(&record).unwrap();
    assert_eq!(store.get::<Record>(&id).unwrap(), record);
    let absent: ContentId = Record {
        value: "absent".into(),
    }
    .content_id()
    .unwrap();
    assert!(store.get::<Record>(&absent).is_err());
}

/// Grounds the memory backend's retained-byte and verified-read behavior across
/// a process-style close/reopen and actual filesystem substitution.
#[test]
fn disk_restart_retains_sources_and_refuses_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = FrameStore::open(dir.path()).unwrap();
    let source = store.put_source(b"retained bytes").unwrap();
    let record = Record {
        value: "recorded".into(),
    };
    let id = store.put(&record).unwrap();
    drop(store);
    let store = FrameStore::open(dir.path()).unwrap();
    assert_eq!(store.source(&source).unwrap(), b"retained bytes");
    assert_eq!(store.get::<Record>(&id).unwrap(), record);
    std::fs::write(dir.path().join(source.to_string()), b"substituted").unwrap();
    assert!(store.source(&source).is_err());
    std::fs::write(dir.path().join(format!("{id}.cbor")), b"substituted").unwrap();
    assert!(store.get::<Record>(&id).is_err());
}
