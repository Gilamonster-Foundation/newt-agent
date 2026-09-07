# Personality and named personas

Edit these preferences in the rich-TUI `/psyche` panel, or load a named persona
with `/persona set <name> --keep-context`.

Newt's communication style has five independent preferences. They describe
how the agent should communicate, not psychological measurements or a promise
that every model will follow the guidance exactly.

| Preference | Lower values | Higher values |
|---|---|---|
| Agreeableness | More candid and challenging | More collaborative framing |
| Extraversion | Reserved and concise | More conversational |
| Warmth | Neutral tone | More reassuring tone |
| Approachability | More formal | More accessible and inviting |
| Prosocial behavior | Focus on the immediate task | More attention to helpfulness and affected people |

Values are whole numbers from 0 through 100. `auto` is different from zero:
it releases the override and inherits the selected persona's value, if any.
An omitted value is unspecified, not an implicit midpoint.

Every setting remains civil and honest. Agreeableness is not factual agreement,
and warmth is not flattery. Extraversion does not authorize unsolicited actions.
These controls do not change OCAP grants, tool restrictions, model routing,
cognition, or tenacity. Tenacity governs how the agent approaches completing
work; personality governs its communication style.

## Edit, apply, cancel, save

Use the existing rich-TUI `/psyche` panel:

1. Select a persona, then use the five named rows to preview style changes.
2. Use left/right to change a value, including `auto`; Enter applies the draft.
   Esc cancels without applying it.
3. Use `:w <name>` to save a named persona without applying the draft, or
   `:wq <name>` to save and apply. Existing names require explicit `!` overwrite.
4. Use `/persona list`, then `/persona set <name> --keep-context` to select a
   saved choice while retaining the conversation. Saving under a new name does
   not itself select that name.

Session overrides belong to their tab. Selecting a persona with `--keep-context`
retains those overrides; `auto` allows the new persona to supply that trait.
Starting a new conversation resets the tab's unsaved overrides. Named persona
files survive restarts; unsaved session edits do not.

Saving retains the selected profile's prose and parsed metadata, including its
tools, skills, and caveats. It does not preserve the original TOML comments or
formatting. A failed save leaves the draft open; it must not be reported as an
applied setting. An unavailable profile cannot safely be reconstructed from a
display name alone.

## Named styles

The bundled choices are `steady` (balanced), `direct` (reserved and
candid), and `sociable` (conversational and welcoming). They are style presets,
not capability suites. An operator-owned persona with one of these names must
not be overwritten when defaults are seeded.

Custom files use the existing persona format:

```markdown
+++
[personality]
agreeableness = 60
extraversion = 40
warmth = 65
approachability = 70
prosocial_behavior = 65
+++

Explain your reasoning clearly and ground conclusions in evidence.
```

See the [settings walkthrough](../../demos/README.md#psyche-and-personas) for
the recording's apply/cancel, save/reload, and unchanged-authority checks.
