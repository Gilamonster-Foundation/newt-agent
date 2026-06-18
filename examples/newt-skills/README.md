# newt-skills Example

Demonstrates skill loading:

```rust
use newt_skills::Skill;
let skill = Skill::load("example_skill");
assert_eq!(skill.name(), "example_skill");
```