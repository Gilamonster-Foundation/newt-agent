//! Parsed, unconditional extraction evidence for #2804. No macro expansion.
use quote::ToTokens;
use std::{env, fs, process};
use syn::{Attribute, Item};

mod forwarding;

fn allowed(attrs: &[Attribute], derive: bool) -> bool {
    attrs
        .iter()
        .all(|a| a.path().is_ident("doc") || (derive && a.path().is_ident("derive")))
}

fn attributes(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Fn(x) => Some(&x.attrs),
        Item::Struct(x) => Some(&x.attrs),
        Item::Enum(x) => Some(&x.attrs),
        Item::Impl(x) => Some(&x.attrs),
        Item::Trait(x) => Some(&x.attrs),
        Item::Type(x) => Some(&x.attrs),
        Item::Const(x) => Some(&x.attrs),
        Item::Static(x) => Some(&x.attrs),
        _ => None,
    }
}

fn evidence(old: &str, new: &str, module: &str, name: &str) -> Result<bool, String> {
    let parse = |s| syn::parse_file(s).map_err(|e| format!("unsupported Rust syntax: {e}"));
    let before = parse(old)?;
    let after = parse(new)?;
    let candidate = parse(module)?;
    let wired = after.items.iter().find_map(|item| match item {
        Item::Mod(m) if m.ident == name && m.content.is_none() => Some(m),
        _ => None,
    });
    let Some(wired) = wired else {
        return Ok(false);
    };
    if !allowed(&before.attrs, false)
        || !allowed(&after.attrs, false)
        || !allowed(&candidate.attrs, false)
        || !allowed(&wired.attrs, false)
    {
        return Err("unsupported attributes affecting candidate module".into());
    }
    // Compare parsed token trees, not lines that may live inside strings,
    // comments, macro bodies, or unrelated conditional/test modules.
    let tokens = |item: &Item| match item {
        // Extraction commonly widens visibility and changes documentation while
        // leaving a forwarding wrapper behind. Signature + body prove motion;
        // attributes were checked separately before using this evidence.
        Item::Fn(f) => {
            let (sig, body) = (&f.sig, &f.block);
            quote::quote!(#sig #body).to_string()
        }
        _ => item.to_token_stream().to_string(),
    };
    let mut unsupported = false;
    for item in &candidate.items {
        let Some(attrs) = attributes(item) else {
            continue;
        };
        let text = tokens(item);
        if !allowed(attrs, true) {
            unsupported = true;
            continue;
        }
        if before
            .items
            .iter()
            .any(|old| attributes(old).is_some_and(|a| allowed(a, true)) && tokens(old) == text)
            && !after.items.iter().any(|new| tokens(new) == text)
            && forwarding::removed_or_forwarded(item, &after.items, name)
        {
            return Ok(true);
        }
    }
    if unsupported {
        Err("unsupported attributes affecting candidate declarations".into())
    } else {
        Ok(false)
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    let result = (|| {
        if args.len() != 4 {
            return Err("expected BEFORE AFTER MODULE NAME".to_owned());
        }
        let read = |p| fs::read_to_string(p).map_err(|e| format!("source unavailable: {e}"));
        evidence(
            &read(&args[0])?,
            &read(&args[1])?,
            &read(&args[2])?,
            &args[3],
        )
    })();
    match result {
        Ok(matched) => println!("{}", if matched { "MATCH" } else { "NONE" }),
        Err(error) => {
            eprintln!("{error}");
            process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::evidence;

    #[test]
    fn real_shaped_unrelated_attributes_do_not_disable_extraction() {
        let shared = "#[cfg(feature = \"markdown\")] mod markdown; #[derive(Debug)] struct State;";
        assert_eq!(
            evidence(
                &format!("{shared} fn moved() {{}}"),
                &format!("{shared} mod extracted; fn moved() {{ extracted::moved() }}"),
                "/// Changed documentation.\npub fn moved() {} #[cfg(test)] mod tests { #[test] fn smoke() {} }",
                "extracted"
            ),
            Ok(true)
        );
    }

    #[test]
    fn retained_implementation_with_added_semicolon_is_not_forwarding() {
        // #2804 round 5: copying and trivially editing is not extraction.
        let old = r#"fn moved() { println!("hello") }"#;
        assert_eq!(
            evidence(
                old,
                r#"mod extracted; fn moved() { println!("hello"); }"#,
                old,
                "extracted"
            ),
            Ok(false)
        );
    }

    #[test]
    fn genuine_wrapper_passes_each_parameter_through_in_order() {
        let old = "fn moved(x: i32, y: i32) -> i32 { x + y }";
        assert_eq!(
            evidence(
                old,
                "mod extracted; fn moved(x: i32, y: i32) -> i32 { extracted::moved(x, y) }",
                old,
                "extracted"
            ),
            Ok(true)
        );
    }

    #[test]
    fn direct_return_and_unit_statement_wrappers_forward() {
        for (old, wrapper) in [
            (
                "fn moved(x: i32) -> i32 { x + 1 }",
                "fn moved(x: i32) -> i32 { return extracted::moved(x); }",
            ),
            (
                "fn moved(x: i32) { println!(\"{x}\"); }",
                "fn moved(x: i32) { extracted::moved(x); }",
            ),
        ] {
            assert_eq!(
                evidence(old, &format!("mod extracted; {wrapper}"), old, "extracted"),
                Ok(true)
            );
        }
    }

    #[test]
    fn retained_nonfunction_and_conditional_wrapper_do_not_prove_removal() {
        assert_eq!(
            evidence(
                "struct Moved { x: i32 }",
                "mod extracted; struct Moved { x: i64 }",
                "struct Moved { x: i32 }",
                "extracted"
            ),
            Ok(false)
        );
        assert_eq!(
            evidence(
                "fn moved() {}",
                "mod extracted; #[cfg(any())] fn moved() { extracted::moved() }",
                "fn moved() {}",
                "extracted"
            ),
            Ok(false)
        );
    }

    #[test]
    fn changed_arguments_wrong_callee_and_extra_work_are_not_forwarding() {
        // #2804: retained functions must actually delegate the original inputs.
        let old = "fn moved(x: i32, y: i32) -> i32 { x + y }";
        for body in [
            "x + y + 0",
            "other::moved(x, y)",
            "extracted::other(x, y)",
            "extracted::moved(y, x)",
            "extracted::moved(x, 0)",
            "extracted::moved(x)",
            "let x = 0; extracted::moved(x, y)",
            "extracted::moved(x, y); 0",
        ] {
            assert_eq!(
                evidence(
                    old,
                    &format!("mod extracted; fn moved(x: i32, y: i32) -> i32 {{ {body} }}"),
                    old,
                    "extracted"
                ),
                Ok(false),
                "{body}"
            );
        }
    }

    #[test]
    fn inactive_text_and_macro_bodies_are_not_moved_items() {
        for module in [
            "/* outer /* inner */ fn moved() {} */",
            "const TEXT: &str = r#\"quoted \" text\nfn moved() {}\n\"#;",
            "macro_rules! hidden { () => { fn moved() {} }; }",
        ] {
            assert_eq!(
                evidence("fn moved() {}", "mod extracted;", module, "extracted"),
                Ok(false)
            );
        }
    }

    #[test]
    fn conditional_candidates_and_file_attributes_are_unsupported() {
        for (new, module) in [
            ("mod extracted;", "#[cfg(any())] fn moved() {}"),
            ("#[cfg(any())] mod extracted;", "fn moved() {}"),
            ("mod extracted;", "#![cfg(any())] fn moved() {}"),
        ] {
            assert!(evidence("fn moved() {}", new, module, "extracted").is_err());
        }
    }

    #[test]
    fn unchanged_or_unwired_items_are_not_extraction() {
        assert_eq!(
            evidence(
                "fn moved() {}",
                "fn moved() {} mod extracted;",
                "fn moved() {}",
                "extracted"
            ),
            Ok(false)
        );
        assert_eq!(
            evidence("fn moved() {}", "mod other;", "fn moved() {}", "extracted"),
            Ok(false)
        );
        assert!(evidence("not rust", "mod extracted;", "", "extracted").is_err());
    }
}
