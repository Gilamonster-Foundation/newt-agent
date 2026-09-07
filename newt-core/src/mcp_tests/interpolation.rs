use super::*;

// ---- ${...} interpolation (pure: injected resolver) ----
#[test]
fn interpolate_returns_a_token_free_literal_verbatim() {
    // The resolver must never be called for a token-free literal.
    let out = interpolate_with("Bearer plain /tmp org-12345", |_| {
        panic!("no token should be resolved")
    })
    .unwrap();
    assert_eq!(out, "Bearer plain /tmp org-12345");
}
#[test]
fn interpolate_embeds_a_token_in_surrounding_literal() {
    let out = interpolate_with(
        "Bearer ${cmd:vault kv get -field=token secret/data/github}!",
        |t| {
            assert_eq!(
                t,
                &InterpToken::Cmd("vault kv get -field=token secret/data/github".into())
            );
            Ok("RESOLVED".to_string())
        },
    )
    .unwrap();
    assert_eq!(out, "Bearer RESOLVED!");
}
#[test]
fn interpolate_handles_multiple_tokens_of_every_scheme() {
    let out = interpolate_with("${env:A}/${file:/p}/${X}", |t| {
        Ok(match t {
            InterpToken::Env(v) => format!("E<{v}>"),
            InterpToken::File(p) => format!("F<{p}>"),
            InterpToken::Cmd(c) => format!("C<{c}>"),
        })
    })
    .unwrap();
    // `${X}` (bare, no scheme) resolves as an env var.
    assert_eq!(out, "E<A>/F</p>/E<X>");
}
#[test]
fn classify_token_maps_schemes_and_leaves_unrecognized_verbatim() {
    assert_eq!(
        classify_token("VAR").unwrap(),
        InterpToken::Env("VAR".into())
    );
    assert_eq!(
        classify_token("env:VAR").unwrap(),
        InterpToken::Env("VAR".into())
    );
    assert_eq!(
        classify_token("file:~/.secrets/x").unwrap(),
        InterpToken::File("~/.secrets/x".into())
    );
    assert_eq!(
        classify_token("cmd:vault kv get -field=token secret/x").unwrap(),
        InterpToken::Cmd("vault kv get -field=token secret/x".into())
    );
    // #1301 conservative contract: an unknown scheme is NOT a token (it is
    // passed through verbatim by the caller), never a hard error.
    assert!(classify_token("bogus:thing").is_none());
    // A shell-style default (`${VAR:-default}`) — colon, unknown scheme →
    // verbatim.
    assert!(classify_token("VAR:-https://api.example.com").is_none());
    // A non-identifier bare token (a jq filter `${.field}`) → verbatim.
    assert!(classify_token(".field").is_none());
    assert!(classify_token("1abc").is_none());
    assert!(classify_token("a b").is_none());
    // A valid identifier IS a bare env token.
    assert_eq!(
        classify_token("_MY_TOKEN2").unwrap(),
        InterpToken::Env("_MY_TOKEN2".into())
    );
}
#[test]
fn interpolate_passes_unrecognized_tokens_through_verbatim() {
    // The resolver must NEVER fire for an unrecognized `${…}`; the whole
    // token text is reassembled byte-for-byte (backward-compat, #1301).
    let never = |_: &InterpToken| -> Result<String> { panic!("must not resolve") };
    assert_eq!(
        interpolate_with("${API_BASE:-https://api.example.com}", never).unwrap(),
        "${API_BASE:-https://api.example.com}"
    );
    assert_eq!(interpolate_with("${.field}", never).unwrap(), "${.field}");
    // A recognized token still resolves, with an unrecognized one left as-is.
    let out = interpolate_with("${env:A}-${x:y}", |t| {
        Ok(match t {
            InterpToken::Env(v) => format!("E<{v}>"),
            _ => unreachable!(),
        })
    })
    .unwrap();
    assert_eq!(out, "E<A>-${x:y}");
}
#[test]
fn interpolate_double_dollar_escapes_a_literal_dollar_brace() {
    let never = |_: &InterpToken| -> Result<String> { panic!("must not resolve") };
    // `$${` yields a literal `${` and the following text stays literal.
    assert_eq!(
        interpolate_with("price $${cmd:evil}", never).unwrap(),
        "price ${cmd:evil}"
    );
    assert_eq!(interpolate_with("$${VAR}", never).unwrap(), "${VAR}");
    // A lone `$` before other text is untouched; a real token after it still
    // resolves.
    let out = interpolate_with("$5 then ${env:A}", |t| match t {
        InterpToken::Env(v) => Ok(format!("<{v}>")),
        _ => unreachable!(),
    })
    .unwrap();
    assert_eq!(out, "$5 then <A>");
}
#[test]
fn interpolate_missing_reference_fails_loudly_not_empty() {
    // A token that the resolver can't satisfy propagates as an error — a
    // missing env var must fail the spawn, never become a silent empty.
    let err = interpolate_with("${env:MISSING}", |_| {
        Err(NewtError::Config("environment variable not set".into()))
    })
    .unwrap_err();
    assert!(format!("{err}").contains("not set"));
}
#[test]
fn interpolate_unterminated_token_errors_without_leaking_the_value() {
    // FIX 6 (#1301): the unterminated-`${` error must reference NO value —
    // a stray `${` after literal secret material must not leak it.
    let err = interpolate_with("sk-live-DEADBEEF ${", |_| Ok("x".into())).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("unterminated"), "{msg}");
    assert!(
        !msg.contains("sk-live-DEADBEEF"),
        "the raw value leaked into the error: {msg}"
    );
}
