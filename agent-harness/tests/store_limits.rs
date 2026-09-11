use agent_harness::store::FrameStore;
use content_addressable::{MerkleNode, RawContentId};

#[test]
fn source_and_node_writers_obey_the_same_limits_as_readers() {
    assert!(FrameStore::memory_with_max_bytes(0).is_err());
    let mut memory = FrameStore::memory_with_max_bytes(64).unwrap();
    let source = memory.put_source(b"original").unwrap();
    assert_eq!(memory.source(&source).unwrap(), b"original");
    assert!(memory.put_source(&[0; 65]).is_err());
    let node = MerkleNode::new("x".repeat(128), []);
    assert!(memory.put(&node).is_err());
}

/// Grounds the size guard in real oversized files, before CID decoding or reads.
#[test]
fn disk_size_limits_refuse_oversized_sources_nodes_and_occupied_addresses() {
    let directory = tempfile::tempdir().unwrap();
    assert!(FrameStore::open_with_max_bytes(directory.path(), 0).is_err());
    let mut store = FrameStore::open_with_max_bytes(directory.path(), 64).unwrap();
    let source = store.put_source(b"original").unwrap();
    let source_path = directory.path().join(source.to_string());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&source_path)
        .unwrap()
        .set_len(65)
        .unwrap();
    assert!(store
        .source(&source)
        .unwrap_err()
        .to_string()
        .contains("record bytes"));
    assert!(store
        .put_source(b"original")
        .unwrap_err()
        .to_string()
        .contains("record bytes"));
    assert!(store.source_len(&source).is_err());
    assert!(store.put_source(&[0; 65]).is_err());
    assert!(!directory
        .path()
        .join(RawContentId::from_content(&[0; 65]).to_string())
        .exists());
    let node = MerkleNode::new("small".to_owned(), []);
    let id = store.put(&node).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(directory.path().join(format!("{id}.cbor")))
        .unwrap()
        .set_len(65)
        .unwrap();
    assert!(store
        .get::<MerkleNode<String>>(&id)
        .unwrap_err()
        .to_string()
        .contains("record bytes"));
}
