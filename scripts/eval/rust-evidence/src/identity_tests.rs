//! Operator identity rule: kind/name removal, not signature or body equality.
use super::evidence;

#[test]
fn edited_declarations_of_every_supported_kind_are_extractions() {
    for (old, moved) in [
        (
            "fn moved(x: Old) -> Old { old::work(x) }",
            "pub fn moved(x: new::Type) -> New { new::work(x) }",
        ),
        ("struct Moved { old: u8 }", "pub struct Moved { new: u64 }"),
        ("enum Moved { Old }", "pub enum Moved { New(u32) }"),
        (
            "trait Moved { fn old(); }",
            "pub trait Moved { fn new(x: i64); }",
        ),
        ("impl Moved { fn old() {} }", "impl Moved { fn new() {} }"),
        (
            "impl Trait for Moved { fn old() {} }",
            "impl Trait for Moved { fn new() {} }",
        ),
        ("const MOVED: u8 = 1;", "pub const MOVED: u64 = 2;"),
        ("static MOVED: u8 = 1;", "pub static MOVED: u64 = 2;"),
        ("type Moved = old::Type;", "pub type Moved = new::Type;"),
        (
            "macro_rules! moved { () => { 1 }; }",
            "macro_rules! moved { ($x:expr) => { $x + 2 }; }",
        ),
    ] {
        assert_eq!(
            evidence(old, "mod extracted;", moved, "extracted"),
            Ok(true),
            "{old}"
        );
    }
}

#[test]
fn retained_identity_is_a_copy_even_if_body_changed_or_forwarded() {
    assert_eq!(
        evidence(
            "fn moved() {}",
            "mod extracted; fn moved() { extracted::moved() }",
            "pub fn moved() {}",
            "extracted"
        ),
        Ok(false)
    );
    for retained in [
        "fn moved() { changed() }",
        "fn moved() { extracted::moved() }",
    ] {
        assert_eq!(
            evidence(
                "fn moved() {}",
                &format!("mod extracted; {retained}"),
                "pub fn moved() { changed() }",
                "extracted"
            ),
            Ok(false)
        );
    }
    assert_eq!(
        evidence(
            "macro_rules! moved { () => { 1 }; }",
            "mod extracted; macro_rules! moved { () => { 2 }; }",
            "macro_rules! moved { () => { 3 }; }",
            "extracted"
        ),
        Ok(false)
    );
}

#[test]
fn kind_name_and_impl_trait_are_all_part_of_identity() {
    for (old, moved) in [
        ("fn moved() {}", "fn renamed() {}"),
        ("fn Moved() {}", "struct Moved;"),
        ("struct Moved;", "enum Moved {}"),
        ("impl A { fn old() {} }", "impl B { fn old() {} }"),
        ("impl TraitA for A {}", "impl TraitB for A {}"),
        ("impl A {}", "impl Trait for A {}"),
        ("impl A<u8> {}", "impl A<u16> {}"),
    ] {
        assert_eq!(
            evidence(old, "mod extracted;", moved, "extracted"),
            Ok(false),
            "{old} / {moved}"
        );
    }
}

#[test]
fn retained_different_trait_impl_does_not_block_removed_impl() {
    assert_eq!(
        evidence(
            "impl TraitA for A { fn old() {} } impl TraitB for A {}",
            "mod extracted; impl TraitB for A {}",
            "impl TraitA for A { fn new() {} }",
            "extracted"
        ),
        Ok(true)
    );
}

#[test]
fn edited_same_identity_still_requires_direct_unconditional_wiring() {
    for after in [
        "mod other;",
        "mod outer { mod extracted; }",
        "mod extracted {}",
    ] {
        assert_eq!(
            evidence(
                "fn moved() {}",
                after,
                "pub fn moved() { changed() }",
                "extracted"
            ),
            Ok(false)
        );
    }
    for after in [
        "#[cfg(any())] mod extracted;",
        "#[path = \"other.rs\"] mod extracted;",
    ] {
        assert!(evidence(
            "fn moved() {}",
            after,
            "pub fn moved() { changed() }",
            "extracted"
        )
        .is_err());
    }
    for attr in ["#[cfg(any())]", "#[custom]"] {
        assert!(evidence(
            "fn moved() {}",
            "mod extracted;",
            &format!("{attr} pub fn moved() {{ changed() }}"),
            "extracted"
        )
        .is_err());
        assert_ne!(
            evidence(
                &format!("{attr} fn moved() {{}}"),
                "mod extracted;",
                "pub fn moved() { changed() }",
                "extracted"
            ),
            Ok(true)
        );
    }
}
