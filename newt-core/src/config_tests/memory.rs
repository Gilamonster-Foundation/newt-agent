use super::*;

// Memory retention and disclosure configuration.

#[test]
fn memory_note_nudge_interval_defaults_and_parses() {
    // Default: 10 — via Default and when `[memory]` omits the key.
    assert_eq!(MemoryConfig::default().note_nudge_interval, 10);
    let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
    assert_eq!(cfg.note_nudge_interval, 10);
    // 0 = nudge off.
    let cfg: MemoryConfig = toml::from_str("note_nudge_interval = 0").unwrap();
    assert_eq!(cfg.note_nudge_interval, 0);
}

#[test]
fn memory_extract_notes_on_close_defaults_off_and_parses() {
    // Default OFF (Step 19.4, #248): the close-time extraction pass is
    // optional and costs a completion — nobody pays for it unasked.
    assert!(!MemoryConfig::default().extract_notes_on_close);
    let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
    assert!(!cfg.extract_notes_on_close);
    // `[memory] extract_notes_on_close = true` is the opt-in.
    let cfg: MemoryConfig = toml::from_str("extract_notes_on_close = true").unwrap();
    assert!(cfg.extract_notes_on_close);
}

#[test]
fn memory_disclosure_defaults_to_frozen_and_parses_index() {
    // INERT BY DEFAULT (#319): the disclosure facet defaults to Frozen —
    // today's behavior, the memory_fetch tool unwired — and only `index`
    // opts in to progressive disclosure.
    assert_eq!(MemoryConfig::default().disclosure, MemoryDisclosure::Frozen);
    let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
    assert_eq!(cfg.disclosure, MemoryDisclosure::Frozen);
    let cfg: MemoryConfig = toml::from_str("disclosure = \"index\"").unwrap();
    assert_eq!(cfg.disclosure, MemoryDisclosure::Index);
    let cfg: MemoryConfig = toml::from_str("disclosure = \"frozen\"").unwrap();
    assert_eq!(cfg.disclosure, MemoryDisclosure::Frozen);
}
