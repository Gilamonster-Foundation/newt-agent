"""Find final reports using the renderer's reply and completed-turn boundaries."""

import re

# newt-tui/src/lib.rs::print_metrics emits TurnMetrics::display_line
# (newt-core/src/metrics.rs). Cursor redraws can place it after prompt text.
FOOTER = re.compile(
    r"(?<![\w.])\d+(?:\.\d+)?(?:ms|s) · "
    r"(?:[\d,]+ in / [\d,]+ out|\(tokens unavailable\))[^\n]*"
)
# display.rs::REPLY_MARKER/newt_line; mod.rs::render_tool_round_narration.
REPLY = re.compile(r"(?m)^[ \t]*(?:newt )?[▸▹][ \t]+")
ANNOTATION = re.compile(r"(?m)^[ \t]*(?:⚠ claim check|▒ |⚙ |\[session |\[Find the |❯ )")


def extract_summary(text: str) -> str:
    """Prefer the last assistant block of the last completed renderer turn."""
    footers = list(FOOTER.finditer(text))
    if footers:
        start = footers[-2].end() if len(footers) > 1 else 0
        text = text[start : footers[-1].start()]
        replies = list(REPLY.finditer(text))
        if replies:
            text = text[replies[-1].end() :]
            return ANNOTATION.split(text, maxsplit=1)[0].strip()
    # mod.rs::cap_exit_progress_block and humanize_cap_exit_progress render
    # hard line breaks between this label and the captured plan/state.
    handoffs = list(re.finditer(r"(?m)^[ \t]*Captured working state:[ \t]*$", text))
    if handoffs and re.search(r"(?m)^[ \t]*Plan:[ \t]*$", text[handoffs[-1].end() :]):
        return ANNOTATION.split(text[handoffs[-1].start() :], maxsplit=1)[0].strip()
    starts = list(
        re.finditer(
            r"(?m)^(?:(?:#{1,3} )?Summary[ \t]*$|The refactor is (?:finished|complete)"
            r"|• Refactored the longest|Deliverable:)",
            text,
        )
    )
    if not starts:
        raise ValueError("no recognizable final summary in transcript")
    return ANNOTATION.split(text[starts[-1].start() :], maxsplit=1)[0].strip()
