use super::*;
use newt_core::agentic::{
    MemAddr, MemPayload, MemorySource, SpillProvenance, SpillRecordV1, SpillScope,
};
use newt_core::SpillStore;

#[test]
fn session_spill_retrieval_without_memory_index_or_conversation_store() {
    let spill = newt_core::SessionSpillStore::new([7; 16]);
    let compaction = newt_core::SessionSpillStore::new([7; 16]);
    let body = "the exact detail omitted from the tool output";
    let cid = spill
        .store(SpillRecordV1::new(
            SpillScope::Session([7; 16]),
            SpillProvenance::ToolOutput {
                tool_name: Some("read_file".into()),
            },
            body.into(),
        ))
        .unwrap();

    // Frozen notes and ephemeral sessions both omit the persistent memory
    // sources. Neither may disconnect the reader for a live session spill.
    let source = memory_fetch_source(None, None, &spill, &compaction, None);
    assert_eq!(
        source
            .fetch(&MemAddr::Spill {
                id: cid.to_handle()
            })
            .unwrap(),
        MemPayload::Found(body.into())
    );
    let summary_cid = compaction
        .store(SpillRecordV1::new(
            SpillScope::Session([7; 16]),
            SpillProvenance::CompactionSpan,
            "the exact detail omitted from the summary".into(),
        ))
        .unwrap();
    assert_eq!(
        source
            .fetch(&MemAddr::Compaction {
                id: summary_cid.to_handle()
            })
            .unwrap(),
        MemPayload::Found("the exact detail omitted from the summary".into())
    );
    for address in [
        MemAddr::Note { id: "3".into() },
        MemAddr::Turn {
            conversation: "past".into(),
            seq: 1,
        },
        MemAddr::Spill {
            id: summary_cid.to_handle(),
        },
    ] {
        assert!(matches!(
            source.fetch(&address).unwrap(),
            MemPayload::NotFound { .. }
        ));
    }
}
