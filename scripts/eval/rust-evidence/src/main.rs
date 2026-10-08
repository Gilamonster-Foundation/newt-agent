//! Parsed, unconditional extraction evidence for #2804. No macro expansion.
use quote::ToTokens;
use std::{env, fs, process};
use syn::{Attribute, Item};

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
