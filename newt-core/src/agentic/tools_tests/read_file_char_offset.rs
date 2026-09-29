use super::*;
use crate::agentic::content_spill::{self, SessionSpillStore};
use crate::agentic::memory_fetch::StoreMemorySource;
use crate::agentic::tools::output_budget;
use crate::agentic::NoMcp;

// #2563 round 2: read_file must never emit a page over the model-facing cap,
// even for a single line longer than the cap — and it must never re-spill a
// spill address it is asked to page through. `char_offset` (the character
// position WITHIN the `offset` line to resume from) is the mechanism, and it
// is learned ONLY from a page's own footer, never from the tool's schema.

/// Parse the `offset=`/`char_offset=` continuation a page's footer names, and
/// split the page into (body, footer). No footer ⇒ the read is complete.
fn split_page(page: &str) -> (&str, Option<usize>, Option<usize>) {
    let (body, footer) = match page.rfind("\n\n[") {
        Some(marker) => (&page[..marker], Some(&page[marker..])),
        None => (page, None),
    };
    let mut next_offset = None;
    let mut next_char_offset = None;
    if let Some(footer) = footer {
        for tok in footer.split_whitespace() {
            if let Some(v) = tok.strip_prefix("offset=") {
                next_offset = v
                    .trim_end_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok();
            } else if let Some(v) = tok.strip_prefix("char_offset=") {
                next_char_offset = v
                    .trim_end_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok();
            }
        }
    }
    (body, next_offset, next_char_offset)
}

#[allow(clippy::too_many_arguments)]
async fn read_file_offloaded(
    ws: &std::path::Path,
    caveats: &Caveats,
    args: serde_json::Value,
    memory_source: Option<&dyn crate::agentic::memory_fetch::MemorySource>,
    spill: &dyn content_spill::SpillStore,
) -> String {
    execute_tool_with_offload(
        "read_file",
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        memory_source,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        true, // tool_offload
        Some(spill),
        None,
    )
    .await
}

fn page_args(path: &str, offset: Option<usize>, char_offset: Option<usize>) -> serde_json::Value {
    let mut args = serde_json::json!({"path": path});
    if let Some(o) = offset {
        args["offset"] = serde_json::json!(o);
    }
    if let Some(c) = char_offset {
        args["char_offset"] = serde_json::json!(c);
    }
    args
}

#[tokio::test]
async fn long_line_pages_under_the_spill_cap_and_never_spills() {
    // Test 1: offload ON, real spill store. A 40k-char line plus short lines
    // after it must be readable to the end via offset/char_offset alone,
    // every page under the spill cap, and NEVER producing a `spill:` handle
    // (round 1's bug: the whole-line page itself exceeded the cap and was
    // spilled, and re-reading the spill hit the same arm and spilled again —
    // an infinite loop, the line never actually readable).
    let ws = tempfile::TempDir::new().unwrap();
    let long_line = "x".repeat(40_000);
    let original = format!("{long_line}\nshort one\nshort two\n");
    std::fs::write(ws.path().join("f.txt"), &original).unwrap();
    let caveats = caveats_rw(ws.path());
    let spill = SessionSpillStore::new([9u8; 16]);
    let cap = content_spill::TOOL_RESULT_SPILL_CAP;

    let mut reconstructed = String::new();
    let mut pages = Vec::new();
    let mut offset = None;
    let mut char_offset = None;
    for _ in 0..500 {
        let args = page_args("f.txt", offset, char_offset);
        let page = read_file_offloaded(ws.path(), &caveats, args, None, &spill).await;
        let (body, next_offset, next_char_offset) = split_page(&page);
        if char_offset.is_none() && !reconstructed.is_empty() {
            reconstructed.push('\n');
        }
        reconstructed.push_str(body);
        pages.push(page.clone());
        match next_offset {
            Some(n) => {
                offset = Some(n);
                char_offset = next_char_offset;
            }
            None => break,
        }
    }

    assert_eq!(
        reconstructed,
        original.trim_end_matches('\n'),
        "reassembled bytes must equal the original file exactly"
    );
    for page in &pages {
        assert!(
            page.len() <= cap,
            "page exceeds the spill cap ({cap} > {}): {} bytes",
            cap,
            page.len()
        );
        assert!(
            !page.contains("spill:"),
            "the long line must never trigger an offload: {page:?}"
        );
    }
    assert!(pages.len() > 1, "the long line must actually be paginated");
}

#[tokio::test]
async fn long_line_pages_under_the_token_budget_with_offload_off() {
    // Test 2: offload OFF. Every page must stay under `max_output_tokens`'s
    // char budget — round 1's bug was unbounded here (a multi-MB single line
    // would go into context whole).
    let ws = tempfile::TempDir::new().unwrap();
    let long_line = "y".repeat(40_000);
    let original = format!("{long_line}\nshort one\nshort two\n");
    std::fs::write(ws.path().join("f.txt"), &original).unwrap();
    let caveats = caveats_rw(ws.path());
    output_budget::set_max_output_tokens(1_000);
    let max_chars = output_budget::cap_estimator().chars_for_tokens(1_000);

    let mut reconstructed = String::new();
    let mut pages = Vec::new();
    let mut offset = None;
    let mut char_offset = None;
    for _ in 0..500 {
        let args = page_args("f.txt", offset, char_offset);
        let page = run_tool("read_file", args, ws.path(), &caveats, None).await;
        let (body, next_offset, next_char_offset) = split_page(&page);
        if char_offset.is_none() && !reconstructed.is_empty() {
            reconstructed.push('\n');
        }
        reconstructed.push_str(body);
        pages.push(page.clone());
        match next_offset {
            Some(n) => {
                offset = Some(n);
                char_offset = next_char_offset;
            }
            None => break,
        }
    }
    output_budget::set_max_output_tokens(output_budget::DEFAULT_MAX_OUTPUT_TOKENS);

    assert_eq!(reconstructed, original.trim_end_matches('\n'));
    for page in &pages {
        assert!(
            page.len() <= max_chars + 300,
            "page exceeds the max_output_tokens char budget (~{max_chars}): {} bytes",
            page.len()
        );
    }
    assert!(pages.len() > 1, "the long line must actually be paginated");
}

#[tokio::test]
async fn read_file_on_a_spill_address_pages_a_long_line_without_re_spilling() {
    // Test 3: `read_file` given a `spill:` address whose payload contains a
    // single over-cap line must page it via the SAME mechanism, with no
    // re-spill at any step (round 1's infinite-loop bug).
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let spill = SessionSpillStore::new([11u8; 16]);
    let long_line = "z".repeat(40_000);
    let payload = format!("{long_line}\nshort one\nshort two\n");
    let staged = spill
        .stage(
            content_spill::SpillProvenance::ToolOutput {
                tool_name: Some("run_command".to_string()),
            },
            payload.clone(),
        )
        .expect("stage");
    let committed = spill.commit_batch(&[staged]).expect("commit");
    let cid = committed[0].cid().to_string();
    let address = format!("spill:{cid}");

    let memory_source = StoreMemorySource::from_stores(None, None).with_spill_store(&spill);
    let cap = content_spill::TOOL_RESULT_SPILL_CAP;

    let mut reconstructed = String::new();
    let mut pages = Vec::new();
    let mut offset = None;
    let mut char_offset = None;
    for _ in 0..500 {
        let args = page_args(&address, offset, char_offset);
        let page =
            read_file_offloaded(ws.path(), &caveats, args, Some(&memory_source), &spill).await;
        let (body, next_offset, next_char_offset) = split_page(&page);
        if char_offset.is_none() && !reconstructed.is_empty() {
            reconstructed.push('\n');
        }
        reconstructed.push_str(body);
        pages.push(page.clone());
        match next_offset {
            Some(n) => {
                offset = Some(n);
                char_offset = next_char_offset;
            }
            None => break,
        }
    }

    assert_eq!(reconstructed, payload.trim_end_matches('\n'));
    for page in &pages {
        assert!(
            page.len() <= cap,
            "page exceeds the spill cap: {} bytes",
            page.len()
        );
        assert!(
            !page.contains("spill:"),
            "re-reading a spill must never produce a NEW spill handle: {page:?}"
        );
    }
    assert!(pages.len() > 1, "the long line must actually be paginated");
}

/// Round 2 review off-by-one: when the char cap cuts EXACTLY on the newline
/// ending the `start` line, `whole_through` must be `start` (the whole line
/// WAS shown), not `start - 1` — otherwise the footer sends the model back to
/// re-read the same line from char 0, and a line whose length is an exact
/// multiple of the effective cap loops forever. Bounded to 500 follow-the-
/// footer iterations so a regression fails the test instead of hanging.
async fn exact_multiple_of_cap_line_terminates(k: usize) {
    let ws = tempfile::TempDir::new().unwrap();
    // The exact char cap `paginate_unspillable` resolves to on the offload
    // path (mirrors its own computation).
    let cap_tokens =
        output_budget::cap_estimator().tokens_for_chars(content_spill::TOOL_RESULT_SPILL_CAP - 512);
    let max_chars = output_budget::cap_estimator().chars_for_tokens(cap_tokens);
    let long_line = "q".repeat(k * max_chars);
    let original = format!("{long_line}\nshort tail\n");
    std::fs::write(ws.path().join("f.txt"), &original).unwrap();
    let caveats = caveats_rw(ws.path());
    let spill = SessionSpillStore::new([13u8; 16]);

    let mut reconstructed = String::new();
    let mut offset = None;
    let mut char_offset = None;
    let mut iterations = 0;
    loop {
        iterations += 1;
        assert!(
            iterations <= 500,
            "k={k}: did not terminate within 500 follow-the-footer iterations \
             (infinite resume loop — the off-by-one regression)"
        );
        let args = page_args("f.txt", offset, char_offset);
        let page = read_file_offloaded(ws.path(), &caveats, args, None, &spill).await;
        let (body, next_offset, next_char_offset) = split_page(&page);
        if char_offset.is_none() && !reconstructed.is_empty() {
            reconstructed.push('\n');
        }
        reconstructed.push_str(body);
        match next_offset {
            Some(n) => {
                offset = Some(n);
                char_offset = next_char_offset;
            }
            None => break,
        }
    }

    assert_eq!(
        reconstructed,
        original.trim_end_matches('\n'),
        "k={k}: reassembled bytes must equal the original file exactly, with no \
         duplicated line from the resume loop"
    );
}

#[tokio::test]
async fn exact_multiple_of_cap_line_terminates_k1() {
    exact_multiple_of_cap_line_terminates(1).await;
}

#[tokio::test]
async fn exact_multiple_of_cap_line_terminates_k2() {
    exact_multiple_of_cap_line_terminates(2).await;
}

#[test]
fn char_offset_is_not_in_the_read_file_tool_schema() {
    // char_offset is learned ONLY from a page's own footer text — it must
    // never appear in the model-facing tool definition, or a model could
    // reach for it speculatively on a first call (per #2558: a new
    // model-facing parameter must earn its place).
    let defs = crate::agentic::tools::catalog::tool_definitions();
    let read_file = defs
        .as_array()
        .expect("tool_definitions returns an array")
        .iter()
        .find(|d| d["function"]["name"] == "read_file")
        .expect("read_file is a defined tool");
    let rendered = read_file.to_string();
    assert!(
        !rendered.contains("char_offset"),
        "char_offset leaked into read_file's schema: {rendered}"
    );
}
