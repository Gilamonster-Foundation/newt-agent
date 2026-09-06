use super::*;

// Tool output token limits and byte conversion defaults.

// ---- #726: [tools] max_output_tokens ----

#[test]
fn tools_max_output_tokens_defaults_to_10k_when_absent() {
    // No `[tools]` section ⇒ the built-in default budget.
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.tools.is_none());
    assert_eq!(cfg.max_output_tokens(), 10_000);
    assert_eq!(cfg.output_head_tokens(), 1_500);
    assert_eq!(Config::default().max_output_tokens(), 10_000);
    assert_eq!(Config::default().output_head_tokens(), 1_500);
}

#[test]
fn tools_output_cap_chars_per_token_defaults_to_3_and_parses_an_override() {
    // Absent ⇒ the conservative default (3, tighter than the 4 c/t estimate).
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.output_cap_chars_per_token(), 3);
    assert_eq!(Config::default().output_cap_chars_per_token(), 3);
    // A `[tools]` table that omits the key still falls back to 3.
    let cfg: Config = toml::from_str("[tools]\n").unwrap();
    assert_eq!(cfg.output_cap_chars_per_token(), 3);
    // Explicit override (e.g. 2 for very dense workloads) is honored.
    let cfg: Config = toml::from_str("[tools]\noutput_cap_chars_per_token = 2\n").unwrap();
    assert_eq!(cfg.tools.as_ref().unwrap().output_cap_chars_per_token, 2);
    assert_eq!(cfg.output_cap_chars_per_token(), 2);
}

#[test]
fn tools_max_output_tokens_parses_an_override() {
    let cfg: Config = toml::from_str(
        r#"
            [tools]
            max_output_tokens = 4096
            output_head_tokens = 512
        "#,
    )
    .unwrap();
    assert_eq!(cfg.tools.as_ref().unwrap().max_output_tokens, 4096);
    assert_eq!(cfg.tools.as_ref().unwrap().output_head_tokens, 512);
    assert_eq!(cfg.max_output_tokens(), 4096);
    assert_eq!(cfg.output_head_tokens(), 512);
}

#[test]
fn tools_config_default_field_is_the_shared_default() {
    // A `[tools]` table that omits the key falls back to the default fn.
    let cfg: Config = toml::from_str("[tools]\n").unwrap();
    assert_eq!(cfg.max_output_tokens(), 10_000);
    assert_eq!(cfg.output_head_tokens(), 1_500);
}

#[test]
fn tools_max_output_tokens_zero_is_a_valid_no_cap() {
    let cfg: Config = toml::from_str("[tools]\nmax_output_tokens = 0\n").unwrap();
    assert_eq!(cfg.max_output_tokens(), 0);
}
