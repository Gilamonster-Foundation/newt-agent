//! #3160: `step_up` is a copy of the part of agent-bridle-core's step-up module that the crew
//! `attest` decision reads, so that decision needs no bridle crate. While bridle is a dependency
//! the two must agree on what the decision and a policy file rest on: the presence ladder (its
//! order, Debug text and wire form), how a requirement is built and judged, which rule a policy
//! picks for a request, and the JSON a policy crosses in. The crew policy and `crew_authz` are
//! also held to what they were when they were written on bridle's types.
//!
//! This file names bridle, so it is gated with the dependency (the `confine` feature) at the end
//! of the series.

use crate::agentic::crew_attest::step_up as ours;
use crate::agentic::crew_attest::{crew_authz, crew_step_up_policy, CrewAuthz};
use agent_bridle as theirs;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fmt::Debug;

/// The ladder on both sides, weakest first.
const PRESENCES: [(ours::Presence, theirs::Presence); 3] = [
    (ours::Presence::None, theirs::Presence::None),
    (ours::Presence::Prompt, theirs::Presence::Prompt),
    (ours::Presence::Passkey, theirs::Presence::Passkey),
];

/// No wildcard arm, so a rung added to the copy stops this compiling.
fn to_theirs(presence: ours::Presence) -> theirs::Presence {
    match presence {
        ours::Presence::None => theirs::Presence::None,
        ours::Presence::Prompt => theirs::Presence::Prompt,
        ours::Presence::Passkey => theirs::Presence::Passkey,
    }
}

/// No wildcard arm, so a rung added by a bridle upgrade stops this compiling.
fn to_ours(presence: theirs::Presence) -> ours::Presence {
    match presence {
        theirs::Presence::None => ours::Presence::None,
        theirs::Presence::Prompt => ours::Presence::Prompt,
        theirs::Presence::Passkey => ours::Presence::Passkey,
    }
}

fn json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn the_presence_ladder_orders_prints_and_serializes_as_bridles_does() {
    let mut orderings = BTreeSet::new();
    // `a` and `b` are two rungs of the copy's ladder; `ta` and `tb` are those rungs of bridle's.
    for ((a, ta), wire) in PRESENCES.into_iter().zip(["none", "prompt", "passkey"]) {
        for (b, tb) in PRESENCES {
            // `crew_authz` asks `established < required`.
            assert_eq!(
                (a < b, a == b, a.cmp(&b)),
                (ta < tb, ta == tb, ta.cmp(&tb)),
                "{a:?} against {b:?}"
            );
            orderings.insert(a.cmp(&b));
        }
        assert_eq!((to_theirs(a), to_ours(ta)), (ta, a));
        // The crew runner prints a presence with `{:?}` in its denial.
        assert_eq!(format!("{a:?}"), format!("{ta:?}"));
        // One wire form, which each side reads from the other.
        assert_eq!((json(&a), json(&ta)), (json(&wire), json(&wire)));
        let read_by_bridle: theirs::Presence = serde_json::from_value(json(&a)).unwrap();
        let read_by_copy: ours::Presence = serde_json::from_value(json(&ta)).unwrap();
        assert_eq!((read_by_bridle, read_by_copy), (ta, a));
    }
    assert_eq!(
        to_theirs(ours::Presence::default()),
        theirs::Presence::default()
    );
    // Anti-vacuous: the nine pairs reach less, equal and greater.
    assert_eq!(orderings.len(), 3);
}

fn same_requirement(a: &ours::AttestRequirement, b: &theirs::AttestRequirement) {
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(json(a), json(b), "{a:?}");
    assert_eq!(a.demands_gesture(), b.demands_gesture(), "{a:?}");
}

#[test]
fn a_requirement_is_built_and_judged_as_bridles_is() {
    same_requirement(
        &ours::AttestRequirement::NONE,
        &theirs::AttestRequirement::NONE,
    );
    let mut judged = Vec::new();
    for (presence, theirs_presence) in PRESENCES {
        let built = ours::AttestRequirement::presence(presence);
        let theirs_built = theirs::AttestRequirement::presence(theirs_presence);
        same_requirement(&built, &theirs_built);
        same_requirement(
            &ours::AttestRequirement {
                presence,
                record: true,
                freshness_generations: 7,
            },
            &theirs::AttestRequirement {
                presence: theirs_presence,
                record: true,
                freshness_generations: 7,
            },
        );
        judged.push(built.demands_gesture());
    }
    // Anti-vacuous: no gesture for `None`, one for each rung above it.
    assert_eq!(judged, [false, true, true]);
}

/// The grid policy: rule `i` is tagged `100 + i`. It mixes selectors from the crew policy, the
/// existing and inherited tests and bridle's push policy with the boundaries around them: a
/// longer selector over a shorter one, `**`, an exact resource, a second `:`, two selectors of
/// one length that match the same request, a bare `*`, and an empty resource, selector and tool.
const SELECTORS: [&str; 15] = [
    "compose_roster",
    "crew",
    "crew:secrets/*",
    "crew:secrets/prod/*",
    "git.push:github.com/org/*",
    "git.push:github.com/org/r**",
    "fs.delete:/tmp/important/a",
    "fs.delete:/tmp/*",
    "a:b:c",
    "crew:ab*",
    "crew:a**",
    "empty:",
    "star:*",
    "",
    ":x",
];

/// The tag of the grid policy's default.
const DEFAULT_TAG: u64 = 999;

/// Every tool a selector names, one the crew policy names and the grid does not (`team`), and
/// one nothing names.
const TOOLS: [&str; 10] = [
    "compose_roster",
    "crew",
    "team",
    "git.push",
    "fs.delete",
    "a",
    "empty",
    "star",
    "",
    "unknown",
];

/// Resources on both sides of every glob above: exact hits, prefixes, near misses, non-ASCII.
const RESOURCES: [&str; 16] = [
    "",
    "x",
    "a",
    "abc",
    "add a flag",
    "secrets/rotate",
    "secrets/prod/key",
    "github.com/org/",
    "github.com/org/repo",
    "github.com/orgx",
    "/tmp/important/a",
    "/tmp/important/b",
    "/tmp/other",
    "b",
    "b:c",
    "é",
];

/// One request on both sides.
fn request(tool: &str, resource: &str) -> (ours::CallRequest, theirs::CallRequest) {
    (
        ours::CallRequest::new(tool, serde_json::Value::Null, resource),
        theirs::CallRequest::new(tool, serde_json::Value::Null, resource),
    )
}

/// One requirement on both sides. `tag` is distinct per rule, so the compared output names the
/// rule that won; presence and `record` vary with it, so the grid carries every value of both.
fn tagged(tag: u64) -> (ours::AttestRequirement, theirs::AttestRequirement) {
    let (presence, theirs_presence) = PRESENCES[tag as usize % 3];
    let record = tag % 2 == 1;
    (
        ours::AttestRequirement {
            presence,
            record,
            freshness_generations: tag,
        },
        theirs::AttestRequirement {
            presence: theirs_presence,
            record,
            freshness_generations: tag,
        },
    )
}

/// The grid policy's rules in the order listed, as (tag, selector).
fn listed() -> Vec<(u64, &'static str)> {
    (100..).zip(SELECTORS).collect()
}

/// One policy on both sides: `rules` in the order given, over `default`.
fn policies(
    rules: &[(u64, &str)],
    default: (ours::AttestRequirement, theirs::AttestRequirement),
) -> (ours::StepUpPolicy, theirs::StepUpPolicy) {
    let (mut ours_rules, mut theirs_rules) = (Vec::new(), Vec::new());
    for &(tag, selector) in rules {
        let (requirement, theirs_requirement) = tagged(tag);
        ours_rules.push(ours::Rule {
            selector: selector.to_string(),
            requirement,
        });
        theirs_rules.push(theirs::Rule {
            selector: selector.to_string(),
            requirement: theirs_requirement,
        });
    }
    (
        ours::StepUpPolicy::new(ours_rules, default.0),
        theirs::StepUpPolicy::new(theirs_rules, default.1),
    )
}

/// The tag of the requirement the copy's policy picks for one request. Bridle's policy must pick
/// the same requirement, compared as JSON.
fn picked(
    ours_policy: &ours::StepUpPolicy,
    theirs_policy: &theirs::StepUpPolicy,
    tool: &str,
    resource: &str,
) -> u64 {
    let (ours_request, theirs_request) = request(tool, resource);
    let requirement = ours_policy.required_for(&ours_request);
    assert_eq!(
        json(&requirement),
        json(&theirs_policy.required_for(&theirs_request)),
        "{tool:?} over {resource:?}"
    );
    requirement.freshness_generations
}

/// Ask both policies about every tool over every resource. Returns the tags that won.
fn agree_over_the_grid(
    ours_policy: &ours::StepUpPolicy,
    theirs_policy: &theirs::StepUpPolicy,
) -> BTreeSet<u64> {
    let mut winners = BTreeSet::new();
    for tool in TOOLS {
        for resource in RESOURCES {
            winners.insert(picked(ours_policy, theirs_policy, tool, resource));
        }
    }
    winners
}

#[test]
fn the_policy_picks_bridles_rule_for_every_selector_and_request() {
    let rules = listed();
    let reversed: Vec<(u64, &str)> = rules.iter().rev().copied().collect();
    let (ours_listed, theirs_listed) = policies(&rules, tagged(DEFAULT_TAG));
    let (ours_reversed, theirs_reversed) = policies(&reversed, tagged(DEFAULT_TAG));

    let winners = agree_over_the_grid(&ours_listed, &theirs_listed);
    agree_over_the_grid(&ours_reversed, &theirs_reversed);
    // Anti-vacuous: each of the fifteen rules wins some request, and so does the default.
    let every_tag: BTreeSet<u64> = (100..=114).chain([DEFAULT_TAG]).collect();
    assert_eq!(winners, every_tag);

    // `crew:ab*` (109) and `crew:a**` (110) are one length and both match `abc`: the one listed
    // first wins, on both sides.
    assert_eq!(picked(&ours_listed, &theirs_listed, "crew", "abc"), 109);
    assert_eq!(picked(&ours_reversed, &theirs_reversed, "crew", "abc"), 110);

    // Each selector alone, over a default of none: it wins some requests and loses the rest.
    for rule in &rules {
        let none = (
            ours::AttestRequirement::NONE,
            theirs::AttestRequirement::NONE,
        );
        let (ours_alone, theirs_alone) = policies(std::slice::from_ref(rule), none);
        let winners = agree_over_the_grid(&ours_alone, &theirs_alone);
        assert_eq!(winners, BTreeSet::from([0, rule.0]), "{rule:?}");
    }
}

/// `crew_step_up_policy` as it was written on bridle's types before the copy.
fn bridles_crew_policy() -> theirs::StepUpPolicy {
    theirs::StepUpPolicy::new(
        vec![
            theirs::Rule {
                selector: "compose_roster".to_string(),
                requirement: theirs::AttestRequirement::NONE,
            },
            theirs::Rule {
                selector: "crew".to_string(),
                requirement: theirs::AttestRequirement::presence(theirs::Presence::Prompt),
            },
            theirs::Rule {
                selector: "team".to_string(),
                requirement: theirs::AttestRequirement::presence(theirs::Presence::Prompt),
            },
        ],
        theirs::AttestRequirement::presence(theirs::Presence::Prompt),
    )
}

/// `crew_authz` as it was written on bridle's types before the copy. `None` is allow; `Some` is
/// the presence it holds for.
fn bridles_crew_authz(
    policy: &theirs::StepUpPolicy,
    op: &str,
    resource: &str,
    established: theirs::Presence,
) -> Option<theirs::Presence> {
    let request = theirs::CallRequest::new(op, serde_json::Value::Null, resource);
    let req = policy.required_for(&request);
    if req.demands_gesture() && established < req.presence {
        Some(req.presence)
    } else {
        None
    }
}

/// The Passkey policy of `a_passkey_required_action_is_not_satisfied_by_prompt`, on both sides.
fn secrets_policies() -> (ours::StepUpPolicy, theirs::StepUpPolicy) {
    (
        ours::StepUpPolicy::new(
            vec![ours::Rule {
                selector: "crew:secrets/*".to_string(),
                requirement: ours::AttestRequirement::presence(ours::Presence::Passkey),
            }],
            ours::AttestRequirement::NONE,
        ),
        theirs::StepUpPolicy::new(
            vec![theirs::Rule {
                selector: "crew:secrets/*".to_string(),
                requirement: theirs::AttestRequirement::presence(theirs::Presence::Passkey),
            }],
            theirs::AttestRequirement::NONE,
        ),
    )
}

#[test]
fn the_crew_policy_and_decision_are_what_they_were_on_bridles_types() {
    let wire = serde_json::to_string(&crew_step_up_policy()).unwrap();
    let read_by_bridle: theirs::StepUpPolicy = serde_json::from_str(&wire).unwrap();
    assert_eq!(read_by_bridle, bridles_crew_policy());

    let mut decisions = BTreeSet::new();
    for (ours_policy, theirs_policy) in [
        (crew_step_up_policy(), bridles_crew_policy()),
        secrets_policies(),
    ] {
        for op in ["compose_roster", "crew", "team", "mystery_op", ""] {
            for resource in ["", "add a flag", "build X", "secrets/rotate"] {
                for (established, theirs_established) in PRESENCES {
                    let decided = match crew_authz(&ours_policy, op, resource, established) {
                        CrewAuthz::Allow => None,
                        CrewAuthz::NeedsAttest(needed) => Some(to_theirs(needed)),
                    };
                    assert_eq!(
                        decided,
                        bridles_crew_authz(&theirs_policy, op, resource, theirs_established),
                        "{op:?} over {resource:?}, established {established:?}"
                    );
                    decisions.insert(decided);
                }
            }
        }
    }
    // Anti-vacuous: the grid reaches allow, a hold for a prompt and a hold for a passkey.
    assert_eq!(
        decisions,
        BTreeSet::from([
            None,
            Some(theirs::Presence::Prompt),
            Some(theirs::Presence::Passkey),
        ])
    );
}

/// The copy's value to JSON, to bridle's type, to JSON, and back: equal at every hop.
fn crosses<A, B>(ours_value: &A, theirs_value: &B)
where
    A: Serialize + DeserializeOwned + PartialEq + Debug,
    B: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let wire = serde_json::to_string(ours_value).unwrap();
    let read_by_bridle: B = serde_json::from_str(&wire).unwrap();
    assert_eq!(&read_by_bridle, theirs_value, "{wire}");
    let wire_back = serde_json::to_string(&read_by_bridle).unwrap();
    assert_eq!(wire_back, wire);
    let read_by_copy: A = serde_json::from_str(&wire_back).unwrap();
    assert_eq!(&read_by_copy, ours_value, "{wire_back}");
}

#[test]
fn a_policy_crosses_between_the_copy_and_bridle_as_json() {
    for (presence, theirs_presence) in PRESENCES {
        crosses(&presence, &theirs_presence);
    }
    let (ours_policy, theirs_policy) = policies(&listed(), tagged(DEFAULT_TAG));
    for (rule, theirs_rule) in ours_policy.rules.iter().zip(&theirs_policy.rules) {
        crosses(&rule.requirement, &theirs_rule.requirement);
        crosses(rule, theirs_rule);
    }
    crosses(&ours_policy, &theirs_policy);
}
