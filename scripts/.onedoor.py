import pathlib

P = "newt-tui/src/commands/model.rs"
p = pathlib.Path(P)
s = p.read_text()

start = s.index('        "backend" => {')
end = s.index('        "backends" => {', start)
old_arm = s[start:end]
assert '"/backend <openai|ollama> [model]"' in old_arm, "unexpected arm contents"

s = s[:start] + s[end:]

# One arm, both tokens — which is what the slash registry has claimed all
# along (`cmd("backends", &["backend"], …)`). Until now the alias was a
# fiction: `/backend` toggled the coarse wire KIND and `/backends` picked a
# NAMED endpoint, so the same registry row described two different commands.
#
# The kind toggle is not lost, it is relocated to where it belongs: a kind is
# a property OF a named backend, and the panel's edit form has carried a
# `kind` field since #1979. Setting it globally, detached from the endpoint it
# applies to, is how a session ends up pointed at an OpenAI-wire URL in Ollama
# mode. On the headless path `/backends <name>` selects a backend that already
# has the right kind, which is the same act with the endpoint named.
s = s.replace(
    '        "backends" => {\n            let cfg = crate::resolve_runtime_or_default();',
    '        "backend" | "backends" => {\n            let cfg = crate::resolve_runtime_or_default();',
    1,
)

s = s.replace(
    """                // List every configured [[backends]] entry by name, flagging the
                // one the session currently resolves to. `/backend` toggles the
                // coarse openai-vs-ollama *kind*; `/backends` picks a *named*
                // endpoint (dgx1, gpu-runner, openai, …) regardless of wire protocol.""",
    """                // List every configured [[backends]] entry by name, flagging
                // the one the session currently resolves to. `/backend` is an
                // ALIAS of this, as the slash registry has always said — it
                // used to be a separate kind toggle, which is now the panel
                // edit form's `kind` field, attached to the endpoint it
                // describes.""",
    1,
)

p.write_text(s)
print("one arm, both tokens")
