use super::Config;

/// Query-param keys (case-insensitive) whose values are treated as secrets when
/// redacting an MCP `url` for an audit dump ([`Config::to_redacted_toml`], #1301).
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "access_token",
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "passphrase",
    "key",
    "credential",
    "credentials",
    "signature",
    "sig",
    "x_amz_signature",
    "x_goog_signature",
    "shared_access_signature",
];

/// CLI flags (case-insensitive) whose value is a secret when redacting MCP
/// `args` — both the `--flag=VALUE` and `--flag VALUE` forms (#1301).
const SENSITIVE_ARG_FLAGS: &[&str] = &[
    "-b",
    "-u",
    "--auth",
    "--authorization",
    "--cookie",
    "--oauth2-bearer",
    "--proxy-user",
    "--user",
    "--token",
    "--access-token",
    "--refresh-token",
    "--api-key",
    "--client-secret",
    "--password",
    "--passphrase",
    "--secret",
    "--key",
    "--credential",
    "--credentials",
    "--signature",
    "--sig",
];

/// Decode URL percent escapes for secret-key classification. Invalid escapes
/// stay literal: malformed input must not panic, and valid encoded spellings
/// such as `client%5Fsecret` must not bypass redaction.
fn percent_decode_for_classification(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Normalize common credential-key spellings before classification. Query
/// keys are percent-decoded first and separators collapse to underscores, so
/// `client-secret`, `client_secret`, and `client%5Fsecret` share one policy.
fn normalized_credential_key(value: &str) -> String {
    let decoded = percent_decode_for_classification(value);
    let mut normalized = String::with_capacity(decoded.len());
    let mut separator = false;
    for ch in decoded.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            separator = false;
        } else if !normalized.is_empty() && !separator {
            normalized.push('_');
            separator = true;
        }
    }
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn is_sensitive_credential_key(value: &str) -> bool {
    let normalized = normalized_credential_key(value);
    SENSITIVE_QUERY_KEYS.contains(&normalized.as_str())
        || normalized.ends_with("_token")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_password")
        || normalized.ends_with("_credential")
        || normalized.ends_with("_signature")
}

/// Credential-bearing HTTP field names accepted by common command-line HTTP
/// clients. Vendor headers conventionally add `X-` to the same credential key,
/// so classify the suffix as well as the complete name.
fn is_sensitive_header_name(value: &str) -> bool {
    let normalized = normalized_credential_key(value);
    normalized == "authorization"
        || normalized.ends_with("_authorization")
        || is_sensitive_credential_key(&normalized)
        || normalized
            .strip_prefix("x_")
            .is_some_and(is_sensitive_credential_key)
}

/// Redact credentials embedded in a URL for an audit dump: the userinfo
/// (`user:pass@`) and any query-param value whose key is sensitive
/// ([`SENSITIVE_QUERY_KEYS`]). Non-secret structure (scheme, host, path,
/// fragment, non-sensitive params) is preserved. Pure string surgery — no `url`
/// crate dependency.
pub(super) fn redact_url_secrets(url: &str) -> String {
    // Peel off `#fragment` then `?query`, redact each part, reassemble.
    let (main, fragment) = match url.split_once('#') {
        Some((m, f)) => (m, Some(f)),
        None => (url, None),
    };
    let (authority_and_path, query) = match main.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (main, None),
    };
    let mut out = redact_url_userinfo(authority_and_path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&redact_url_query(q));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Redact `user:pass@` userinfo from the authority of a `scheme://…` string
/// (the `?query`/`#fragment` already stripped). An `@` only counts inside the
/// authority (before the first `/`), so a path/param `@` is never mistaken for
/// userinfo.
fn redact_url_userinfo(s: &str) -> String {
    let Some((scheme, rest)) = s.split_once("://") else {
        return s.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("{}@{host}", Config::REDACTED),
        None => authority.to_string(),
    };
    let mut out = format!("{scheme}://{authority}");
    if let Some(p) = path {
        out.push('/');
        out.push_str(p);
    }
    out
}

/// Redact the values of sensitive query params, keeping keys + non-sensitive
/// params intact.
fn redact_url_query(query: &str) -> String {
    query
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((k, _)) if is_sensitive_credential_key(k) => {
                format!("{k}={}", Config::REDACTED)
            }
            _ => param.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether `flag` is a sensitive CLI flag whose value must be redacted.
fn is_sensitive_arg_flag(flag: &str) -> bool {
    if !flag.starts_with('-') {
        return false;
    }
    let flag = flag.trim_start_matches('-');
    let normalized = normalized_credential_key(flag);
    SENSITIVE_ARG_FLAGS
        .iter()
        .any(|candidate| normalized_credential_key(candidate.trim_start_matches('-')) == normalized)
        || is_sensitive_credential_key(&normalized)
}

/// Redact the values of sensitive flags in an args vector, handling both
/// `--flag=VALUE` (redact the tail) and `--flag VALUE` (redact the next arg).
/// Over-redaction is safe for an audit dump; under-redaction is not.
pub(super) fn redact_arg_secrets(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    let mut redact_header_next = false;
    for arg in args {
        if redact_next {
            out.push(Config::REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if redact_header_next {
            if arg
                .split_once(':')
                .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
            {
                out.push(Config::REDACTED.to_string());
            } else {
                out.push(arg.clone());
            }
            redact_header_next = false;
            continue;
        }
        match arg.split_once('=') {
            Some((flag, _)) if is_sensitive_arg_flag(flag) => {
                out.push(format!("{flag}={}", Config::REDACTED));
            }
            Some((flag, value)) if matches!(flag, "-H" | "--header") => {
                if value
                    .split_once(':')
                    .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
                {
                    out.push(format!("{flag}={}", Config::REDACTED));
                } else {
                    out.push(arg.clone());
                }
            }
            _ if arg.strip_prefix("-H").is_some_and(|value| {
                !value.is_empty()
                    && value
                        .split_once(':')
                        .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
            }) =>
            {
                out.push(format!("-H{}", Config::REDACTED));
            }
            _ if ["-b", "-u"]
                .iter()
                .find_map(|flag| arg.strip_prefix(flag).map(|value| (*flag, value)))
                .is_some_and(|(_, value)| !value.is_empty()) =>
            {
                let flag = &arg[..2];
                out.push(format!("{flag}{}", Config::REDACTED));
            }
            _ if is_sensitive_arg_flag(arg) => {
                // `--flag VALUE`: keep the flag, redact the following value.
                out.push(arg.clone());
                redact_next = true;
            }
            _ if matches!(arg.as_str(), "-H" | "--header") => {
                out.push(arg.clone());
                redact_header_next = true;
            }
            _ => out.push(arg.clone()),
        }
    }
    out
}
