// Integration tests for terminal_hyperlink — run with:
//   cargo test --package newt-tui --test terminal_hyperlink_integration
//
// These verify the OSC 8 escape sequences render correctly in a real terminal.
// They print raw bytes so you can visually confirm clickable links appear.

use newt_tui::terminal_hyperlink::{render_link, render_link_osc8, render_url, supports_osc8};

#[test]
fn test_os8_visible_in_terminal() {
    // This test prints to stdout — run it in a real terminal (not CI) and
    // verify the URLs are clickable. In CI/piped output they'll show as plain text.
    let url = "https://github.com/Gilamonster-Foundation/newt-agent/issues/771";

    println!("\n=== OSC 8 Hyperlink Test ===");
    println!("Is OSC 8 supported: {}", supports_osc8());
    println!();

    // Test 1: Clickable link with custom text.
    let link = render_link("Click here to view issue #771", url);
    println!("Test 1 (custom text):");
    println!("{link}");
    println!("Raw bytes: {:?}", link.as_bytes());

    // Test 2: URL displayed as the link itself.
    let url_link = render_url(url);
    println!("\nTest 2 (URL as link):");
    println!("{url_link}");
    println!("Raw bytes: {:?}", url_link.as_bytes());

    // Test 3: Multiple links on one line.
    let a = render_link("Issue", "https://github.com/foo/bar/issues/1");
    let b = render_link("PR", "https://github.com/foo/bar/pull/2");
    println!("\nTest 3 (multiple links):");
    println!("{a} | {b}");

    // Test 4: Link with query params.
    let search = render_link(
        "Search on GitHub",
        "https://github.com/search?q=terminal+hyperlink&type=issues",
    );
    println!("\nTest 4 (query params):");
    println!("{search}");

    // Test 5: OSC 8 format structure.
    let osc8 = render_link_osc8("plain text", "https://example.com/path");
    println!("\nTest 5 (structure check):");
    assert!(osc8.starts_with("\x1b]8;;"));
    assert!(osc8.ends_with('\x07'));
    println!("Structure OK: starts with OSC 8 open, ends with ST.");

    // Test 6: Verify the reset sequence.
    let link = render_link_osc8("before", "https://a.com");
    let link2 = render_link_osc8("after", "https://b.com");
    let combined = format!("{link} then {link2}");
    println!("\nTest 6 (reset between links):");
    println!("{combined}");
    // The first link should reset before the second opens.
    assert!(combined.contains("\x1b]8;;\x07"));

    println!("\n=== End of test ===");
}
