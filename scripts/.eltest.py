import pathlib
import re

P = "newt-tui/src/lib_tests/core.rs"
p = pathlib.Path(P)
s = p.read_text()

# Existing assertions all describe a NON-cockpit session, so they gain `false`.
# Single-line calls only; verified below that none span lines.
pat = re.compile(r"(spill_eligibility_for|live_spill_capable_for)\(([^()]*?)\)")


def add_arg(m):
    name, args = m.group(1), m.group(2)
    if args.rstrip().endswith("false") and args.count(",") >= 5:
        return m.group(0)  # already six
    return f"{name}({args}, false)"


s, n = pat.subn(add_arg, s)
print("call sites updated:", n)

# Extend the precedence contract with the new, deliberately-last rung.
old = '''    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("xterm-256color"), false),
        SpillEligibility::Available
    );'''
new = '''    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("xterm-256color"), false),
        SpillEligibility::Available
    );
    // The cockpit is LAST on purpose. It is the one refusal where nothing is
    // wrong — platform, build and terminal are all fine and the operator is
    // simply on the surface that paints its own frames — so every fixable
    // cause outranks it and the operator is never sent to fix `TERM` for it.
    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("xterm-256color"), true),
        SpillEligibility::TerminalOwnedByCockpit
    );
    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("dumb"), true),
        SpillEligibility::TermDumb,
        "a dumb terminal is the more fundamental refusal and still wins"
    );
    assert_eq!(
        spill_eligibility_for(false, true, true, true, Some("xterm-256color"), true),
        SpillEligibility::UnsupportedPlatform,
        "and so does a platform the viewport could never run on"
    );

    // The cockpit refusal must name the surface AND the way out, or it reads
    // as a broken terminal. This is the line `/spill` printed as
    // "live interaction available" while the cockpit was drawing every frame.
    let cockpit = SpillEligibility::TerminalOwnedByCockpit.explain();
    assert!(cockpit.contains("cockpit"), "{cockpit}");
    assert!(
        cockpit.contains("/spill open"),
        "an unavailability the operator cannot act on is barely better than \\
         a bare `unavailable`: {cockpit}"
    );'''

assert old in s, "the Available assertion moved"
s = s.replace(old, new, 1)
p.write_text(s)
print("precedence contract extended")
