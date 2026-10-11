# Refactor extraction evidence

Internal, offline helper for the refactor-run grader. Uses the workspace's
existing syn/quote dependencies to compare parsed top-level Rust items.
Arguments are the before, after, and candidate module source paths, followed
by the module name. Prints MATCH or NONE; unsupported candidate attributes or
invalid syntax exit unsuccessfully. It does not expand macros or evaluate cfg.
Only doc/derive attributes are accepted on moved items; only doc attributes
are accepted on file roots and the candidate module declaration. Unrelated
items, including cfg-gated test modules, do not supply extraction evidence.
Identity is the parsed declaration kind plus name: fn, struct, enum, trait,
const, static, type, or macro_rules. Impl identity uses the parsed self-type
plus optional trait, preserving generic arguments and polarity; it does not
resolve aliases or qualified type/trait paths. Visibility, documentation,
function signatures, and bodies may change. The source must no longer contain
that identity, even as a forwarding wrapper. Renames, different kinds, and
copies do not qualify. Macro bodies are never expanded into declarations.
This is structural evidence, not proof that behavior stayed the same.

The Python grader builds this trusted helper once per process, offline, in
its own checkout. No code from the graded checkout executes in this helper.
