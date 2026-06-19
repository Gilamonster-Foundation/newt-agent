//! Pure, unit-testable logic behind `newt dgx pull` (Step 14.5).
//!
//! `ollama pull hf.co/<org>/<repo>:<quant>` works for single-file GGUF repos
//! but **fails on sharded (multi-part) GGUF** with
//! `400: The specified tag is a sharded GGUF. Ollama does not support this yet`
//! (ollama/ollama#5245). Q5+/Q8_0 of large unsloth repos are typically sharded.
//! This module turns an arg into a [`PullPlan`] that automates the documented
//! workaround: download each shard with `curl` and `ollama create` from a
//! `Modelfile` whose `FROM` points at the first shard (ollama/llama.cpp auto-
//! loads sibling shards).
//!
//! It also encodes the **GLM-5.2 lesson**: a model whose on-disk size exceeds
//! the node's RAM is effectively unrunnable (heavy disk paging). [`fit_check`]
//! computes that verdict so the caller can refuse-unless-`--force`.
//!
//! Everything here is pure: arg parsing, quant→file matching, plan construction
//! from already-fetched HF JSON, name sanitization, the fit verdict, remote
//! shell-script generation, and ssh argv construction. The HF API fetch and the
//! SSH/ollama execution are thin wrappers in `dgx.rs`.

/// A parsed model reference: either a plain Ollama name or a HuggingFace repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRef {
    /// A plain Ollama model name, e.g. `qwen2.5-coder:32b`. Pulled directly via
    /// `ollama pull <name>`.
    Ollama { name: String },
    /// A HuggingFace GGUF reference, e.g. `unsloth/Qwen3-Coder-GGUF:Q8_0` or
    /// `hf.co/unsloth/Qwen3-Coder-GGUF:Q8_0`. Goes through the smart path.
    Hf {
        org: String,
        repo: String,
        quant: String,
    },
}

impl ModelRef {
    /// Parse a `pull` argument.
    ///
    /// HF form is recognised by a `<org>/<repo>:<quant>` shape (exactly one `/`
    /// before the `:quant`), optionally prefixed with `hf.co/` or
    /// `huggingface.co/`. Everything else is treated as a plain Ollama name —
    /// including bare names with no slash and names with a registry host like
    /// `registry.example.com/library/foo:tag`.
    pub fn parse(arg: &str) -> Self {
        let arg = arg.trim();
        // Strip an optional HF host prefix; its presence forces the HF path.
        let (body, forced_hf) = strip_hf_prefix(arg);

        if let Some((path, quant)) = body.rsplit_once(':') {
            let segments: Vec<&str> = path.split('/').collect();
            // org/repo:quant — exactly two non-empty path segments.
            if segments.len() == 2 && !segments[0].is_empty() && !segments[1].is_empty() {
                return Self::Hf {
                    org: segments[0].to_string(),
                    repo: segments[1].to_string(),
                    quant: quant.to_string(),
                };
            }
            // hf.co-prefixed but odd shape: still treat as HF if a slash exists.
            if forced_hf && segments.len() >= 2 && !quant.is_empty() {
                let org = segments[0].to_string();
                let repo = segments[1..].join("/");
                return Self::Hf {
                    org,
                    repo,
                    quant: quant.to_string(),
                };
            }
        }
        Self::Ollama {
            name: body.to_string(),
        }
    }
}

/// Strip a leading `hf.co/` or `huggingface.co/` (any case). Returns the body
/// and whether a prefix was present.
fn strip_hf_prefix(arg: &str) -> (&str, bool) {
    let lower = arg.to_ascii_lowercase();
    for prefix in ["hf.co/", "huggingface.co/"] {
        if lower.starts_with(prefix) {
            return (&arg[prefix.len()..], true);
        }
    }
    (arg, false)
}

/// A GGUF sibling file from the HF model API (`siblings[]` with `?blobs=true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufFile {
    /// Path within the repo (may include a subdirectory), e.g.
    /// `Q8_0/Qwen3-Coder-Q8_0-00001-of-00002.gguf`.
    pub path: String,
    /// Size in bytes, when the API reported it.
    pub size: Option<u64>,
}

/// Parse the HF model API JSON (`GET /api/models/<org>/<repo>?blobs=true`) into
/// the `.gguf` siblings. Tolerates either `size` or `lfs.size` for byte counts.
pub fn parse_gguf_siblings(json: &serde_json::Value) -> Vec<GgufFile> {
    let Some(siblings) = json.get("siblings").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    siblings
        .iter()
        .filter_map(|s| {
            let path = s.get("rfilename").and_then(|v| v.as_str())?;
            if !path.to_ascii_lowercase().ends_with(".gguf") {
                return None;
            }
            let size = s.get("size").and_then(|v| v.as_u64()).or_else(|| {
                s.get("lfs")
                    .and_then(|l| l.get("size"))
                    .and_then(|v| v.as_u64())
            });
            Some(GgufFile {
                path: path.to_string(),
                size,
            })
        })
        .collect()
}

/// Does this GGUF file belong to the requested `quant`?
///
/// Matches the quant token case-insensitively, tolerating:
/// - a `<repo>-<QUANT>` style prefix (the quant appears as a `-`/`_`/`.`/`/`
///   delimited token, not a substring of a longer word),
/// - an optional `-NNNNN-of-NNNNN` split suffix,
/// - an optional subdirectory (the quant may appear in the dir or the file),
/// - unsloth `UD-` dynamic-quant prefixes (e.g. `UD-Q4_K_XL`).
///
/// `Q4_K_M` must not match `Q4_K_S`, and `Q8_0` must not match `Q8_0_L`.
pub fn file_matches_quant(path: &str, quant: &str) -> bool {
    let quant_lc = quant.to_ascii_lowercase();
    if quant_lc.is_empty() {
        return false;
    }
    // Tokenise the whole path on common GGUF delimiters and look for the quant
    // as a *whole token*. This rejects substring false-positives (Q4_K_S vs
    // Q4_K_M) because the split suffix / extension are separate tokens.
    let lower = path.to_ascii_lowercase();
    lower
        .split(['-', '/', '.', ' '])
        .any(|tok| token_matches_quant(tok, &quant_lc))
}

/// True when a single delimiter-split token equals the quant, ignoring a
/// leading unsloth `ud_`/`ud` dynamic-quant marker that `-` splitting may have
/// folded into the token (e.g. `ud` token then `q4_k_xl` token — handled by the
/// caller's tokenisation; here we also accept an exact match).
fn token_matches_quant(tok: &str, quant_lc: &str) -> bool {
    tok == quant_lc
}

/// The outcome of resolving an HF reference + its matched files into an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullPlan {
    /// A plain Ollama name: `ollama pull <tag>`.
    OllamaNative { tag: String },
    /// A single-file HF GGUF: `ollama pull hf.co/<org>/<repo>:<quant>`.
    SingleFileHf {
        org: String,
        repo: String,
        quant: String,
    },
    /// A sharded HF GGUF: download shards + `ollama create` from a Modelfile.
    ShardedHf {
        org: String,
        repo: String,
        quant: String,
        /// Matched shard paths within the repo, sorted by the split index.
        parts: Vec<String>,
        /// The generated `Modelfile` body.
        modelfile: String,
        /// The resulting ollama model name (`--name` or sanitized default).
        name: String,
    },
}

/// Build a [`PullPlan`] for an HF reference given the GGUF files matched to the
/// quant. `name_override` is the `--name` flag (already validated by clap).
///
/// Returns an error when no file matches the quant (so the caller can surface a
/// clear "quant not found" message listing what *is* available).
pub fn plan_hf(
    org: &str,
    repo: &str,
    quant: &str,
    matched: &[GgufFile],
    name_override: Option<&str>,
) -> Result<PullPlan, String> {
    if matched.is_empty() {
        return Err(format!(
            "no .gguf file in {org}/{repo} matches quant {quant:?}"
        ));
    }
    if matched.len() == 1 {
        return Ok(PullPlan::SingleFileHf {
            org: org.to_string(),
            repo: repo.to_string(),
            quant: quant.to_string(),
        });
    }
    let mut parts: Vec<String> = matched.iter().map(|f| f.path.clone()).collect();
    parts.sort();
    let first = parts
        .first()
        .map(|p| basename_of(p).to_string())
        .unwrap_or_default();
    let name = match name_override {
        Some(n) => n.to_string(),
        None => default_ollama_name(repo, quant),
    };
    let modelfile = modelfile_body(&first);
    Ok(PullPlan::ShardedHf {
        org: org.to_string(),
        repo: repo.to_string(),
        quant: quant.to_string(),
        parts,
        modelfile,
        name,
    })
}

/// The basename (final path segment) of a repo-relative path.
pub fn basename_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The `Modelfile` body for a sharded pull. ollama/llama.cpp auto-loads the
/// sibling shards given the first one.
pub fn modelfile_body(first_shard_basename: &str) -> String {
    format!("FROM ./{first_shard_basename}\n")
}

/// Sanitize `<repo>-<quant>` into a legal Ollama model name.
///
/// Ollama names are lowercase; `/` is the registry separator so it is replaced
/// with `-`; the `.gguf` extension (if present) is stripped; any character
/// outside `[a-z0-9._-]` collapses to `-`; runs of `-` collapse and leading/
/// trailing `-`/`.` are trimmed.
pub fn default_ollama_name(repo: &str, quant: &str) -> String {
    let raw = format!("{repo}-{quant}");
    sanitize_ollama_name(&raw)
}

/// Sanitize an arbitrary string into a legal Ollama model name (see
/// [`default_ollama_name`]).
pub fn sanitize_ollama_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let lower = lower.strip_suffix(".gguf").unwrap_or(&lower);
    let mut out = String::with_capacity(lower.len());
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    // Collapse runs of '-'.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_dash = false;
    for ch in out.chars() {
        if ch == '-' {
            if !prev_dash {
                collapsed.push(ch);
            }
            prev_dash = true;
        } else {
            collapsed.push(ch);
            prev_dash = false;
        }
    }
    collapsed.trim_matches(|c| c == '-' || c == '.').to_string()
}

// ---------------------------------------------------------------------------
// Fit pre-flight (the GLM-5.2 lesson)
// ---------------------------------------------------------------------------

/// The verdict of comparing total model bytes against node memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitVerdict {
    /// Model fits comfortably in node RAM.
    Fits { model_bytes: u64, mem_bytes: u64 },
    /// Model exceeds node RAM — unrunnable / heavy disk paging.
    Exceeds { model_bytes: u64, mem_bytes: u64 },
    /// Node memory could not be detected; proceed best-effort.
    Undetectable { model_bytes: u64 },
}

impl FitVerdict {
    /// Whether the pull should be refused unless `--force` is passed.
    pub fn should_refuse(&self) -> bool {
        matches!(self, Self::Exceeds { .. })
    }
}

/// Compare the summed model size against detected node memory.
///
/// `mem_bytes = None` means detection failed (best-effort: warn + proceed).
pub fn fit_check(model_bytes: u64, mem_bytes: Option<u64>) -> FitVerdict {
    match mem_bytes {
        None => FitVerdict::Undetectable { model_bytes },
        Some(mem) if model_bytes > mem => FitVerdict::Exceeds {
            model_bytes,
            mem_bytes: mem,
        },
        Some(mem) => FitVerdict::Fits {
            model_bytes,
            mem_bytes: mem,
        },
    }
}

/// Sum the byte sizes of the matched files (skipping any with unknown size).
pub fn total_bytes(files: &[GgufFile]) -> u64 {
    files.iter().filter_map(|f| f.size).sum()
}

/// Parse the single integer printed by `free -b | awk '/Mem:/{print $2}'`.
pub fn parse_free_bytes(stdout: &str) -> Option<u64> {
    stdout.split_whitespace().next()?.parse::<u64>().ok()
}

/// Human-readable GiB rendering for warnings (1 decimal place).
pub fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

// ---------------------------------------------------------------------------
// Remote script + ssh argv (pure)
// ---------------------------------------------------------------------------

/// Single-quote a string for safe embedding in a POSIX shell command.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A sanitized staging-dir component derived from the model name (no slashes /
/// shell metacharacters).
pub fn staging_component(name: &str) -> String {
    sanitize_ollama_name(name)
}

/// Generate the remote shell script that downloads every shard with resumable
/// `curl`, writes the `Modelfile`, and runs `ollama create`.
///
/// `has_token` controls whether an `Authorization: Bearer "$HF_TOKEN"` header is
/// added (the token value is *never* embedded — the remote `$HF_TOKEN` env is
/// referenced so the secret stays on the node).
pub fn sharded_remote_script(
    org: &str,
    repo: &str,
    parts: &[String],
    modelfile: &str,
    name: &str,
    has_token: bool,
) -> String {
    let staging = staging_component(name);
    let dir = format!("$HOME/.cache/newt-dgx-pull/{staging}");
    let mut script = String::new();
    script.push_str("set -eu\n");
    script.push_str(&format!("STAGE={}\n", sh_quote(&dir)));
    script.push_str("mkdir -p \"$STAGE\"\n");
    script.push_str("cd \"$STAGE\"\n");
    let auth = if has_token {
        " -H \"Authorization: Bearer $HF_TOKEN\""
    } else {
        ""
    };
    for part in parts {
        let url = format!("https://huggingface.co/{org}/{repo}/resolve/main/{part}");
        let basename = basename_of(part);
        script.push_str(&format!(
            "curl -L --fail -C -{auth} -o {out} {url}\n",
            out = sh_quote(basename),
            url = sh_quote(&url),
        ));
    }
    // Write the Modelfile via a heredoc so the FROM line is verbatim.
    script.push_str("cat > Modelfile <<'NEWT_MODELFILE_EOF'\n");
    script.push_str(modelfile);
    if !modelfile.ends_with('\n') {
        script.push('\n');
    }
    script.push_str("NEWT_MODELFILE_EOF\n");
    script.push_str(&format!(
        "ollama create {name} -f Modelfile\n",
        name = sh_quote(name)
    ));
    script
}

/// Build the `ssh` argv that runs `remote_command` on the node.
///
/// Honors user/host and an optional port. The command is passed as a single
/// final argument (ssh runs it through the remote shell). No quoting is applied
/// here — `remote_command` is expected to be a complete shell program (e.g. the
/// output of [`sharded_remote_script`] or a `ollama pull <tag>` line).
pub fn ssh_argv(user: &str, host: &str, port: Option<u16>, remote_command: &str) -> Vec<String> {
    let mut argv = vec!["ssh".to_string()];
    if let Some(p) = port {
        argv.push("-p".to_string());
        argv.push(p.to_string());
    }
    argv.push(format!("{user}@{host}"));
    argv.push(remote_command.to_string());
    argv
}

/// The remote command for the single-file HF path.
pub fn single_file_remote_command(org: &str, repo: &str, quant: &str) -> String {
    format!(
        "ollama pull {}",
        sh_quote(&format!("hf.co/{org}/{repo}:{quant}"))
    )
}

/// The remote command for the plain Ollama path.
pub fn ollama_native_remote_command(tag: &str) -> String {
    format!("ollama pull {}", sh_quote(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ModelRef::parse ----------------------------------------------

    #[test]
    fn parse_plain_ollama_name() {
        assert_eq!(
            ModelRef::parse("qwen2.5-coder:32b"),
            ModelRef::Ollama {
                name: "qwen2.5-coder:32b".into()
            }
        );
    }

    #[test]
    fn parse_bare_name_no_tag() {
        assert_eq!(
            ModelRef::parse("llama3.1"),
            ModelRef::Ollama {
                name: "llama3.1".into()
            }
        );
    }

    #[test]
    fn parse_hf_org_repo_quant() {
        assert_eq!(
            ModelRef::parse("unsloth/Qwen3-Coder-GGUF:Q8_0"),
            ModelRef::Hf {
                org: "unsloth".into(),
                repo: "Qwen3-Coder-GGUF".into(),
                quant: "Q8_0".into()
            }
        );
    }

    #[test]
    fn parse_hf_with_hf_co_prefix() {
        assert_eq!(
            ModelRef::parse("hf.co/unsloth/Qwen3-Coder-GGUF:Q5_K_M"),
            ModelRef::Hf {
                org: "unsloth".into(),
                repo: "Qwen3-Coder-GGUF".into(),
                quant: "Q5_K_M".into()
            }
        );
    }

    #[test]
    fn parse_hf_with_huggingface_co_prefix_case_insensitive() {
        assert_eq!(
            ModelRef::parse("HuggingFace.co/Org/Repo:Q4_K_M"),
            ModelRef::Hf {
                org: "Org".into(),
                repo: "Repo".into(),
                quant: "Q4_K_M".into()
            }
        );
    }

    #[test]
    fn parse_registry_host_is_ollama_not_hf() {
        // Three path segments + tag => not the org/repo HF shape.
        assert_eq!(
            ModelRef::parse("registry.example.com/library/foo:latest"),
            ModelRef::Ollama {
                name: "registry.example.com/library/foo:latest".into()
            }
        );
    }

    #[test]
    fn parse_hf_prefixed_with_subdir_repo() {
        // hf.co forces HF even with extra path segments.
        assert_eq!(
            ModelRef::parse("hf.co/org/repo/sub:Q8_0"),
            ModelRef::Hf {
                org: "org".into(),
                repo: "repo/sub".into(),
                quant: "Q8_0".into()
            }
        );
    }

    // --- parse_gguf_siblings ------------------------------------------

    #[test]
    fn parse_siblings_filters_gguf_and_reads_size() {
        let json = serde_json::json!({
            "siblings": [
                {"rfilename": "README.md"},
                {"rfilename": "model-Q8_0-00001-of-00002.gguf", "size": 100u64},
                {"rfilename": "model-Q8_0-00002-of-00002.gguf", "lfs": {"size": 200u64}},
                {"rfilename": "config.json"}
            ]
        });
        let files = parse_gguf_siblings(&json);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].size, Some(100));
        assert_eq!(files[1].size, Some(200));
    }

    #[test]
    fn parse_siblings_missing_key_is_empty() {
        assert!(parse_gguf_siblings(&serde_json::json!({})).is_empty());
        assert!(parse_gguf_siblings(&serde_json::json!({"siblings": "x"})).is_empty());
    }

    // --- file_matches_quant -------------------------------------------

    #[test]
    fn quant_matches_simple() {
        assert!(file_matches_quant("repo-Q8_0.gguf", "Q8_0"));
        assert!(file_matches_quant("Repo-Q8_0.GGUF", "q8_0"));
    }

    #[test]
    fn quant_matches_split_suffix() {
        assert!(file_matches_quant(
            "Qwen3-Coder-Q8_0-00001-of-00002.gguf",
            "Q8_0"
        ));
    }

    #[test]
    fn quant_matches_in_subdir() {
        assert!(file_matches_quant(
            "Q4_K_M/model-00001-of-00003.gguf",
            "Q4_K_M"
        ));
    }

    #[test]
    fn quant_matches_ud_dynamic_prefix_token() {
        // unsloth UD- prefix: the quant token appears whole after the dash split.
        assert!(file_matches_quant(
            "Model-UD-Q4_K_XL-00001-of-00002.gguf",
            "Q4_K_XL"
        ));
    }

    #[test]
    fn quant_does_not_match_sibling_quant() {
        assert!(!file_matches_quant("repo-Q4_K_S.gguf", "Q4_K_M"));
        assert!(!file_matches_quant("repo-Q8_0_L.gguf", "Q8_0"));
        assert!(!file_matches_quant("repo-Q4_K_M.gguf", "Q4_K_S"));
    }

    #[test]
    fn quant_empty_never_matches() {
        assert!(!file_matches_quant("repo-Q8_0.gguf", ""));
    }

    // --- plan_hf -------------------------------------------------------

    fn gguf(path: &str, size: u64) -> GgufFile {
        GgufFile {
            path: path.into(),
            size: Some(size),
        }
    }

    #[test]
    fn plan_single_file() {
        let matched = vec![gguf("repo-Q8_0.gguf", 10)];
        let plan = plan_hf("unsloth", "Repo-GGUF", "Q8_0", &matched, None).unwrap();
        assert_eq!(
            plan,
            PullPlan::SingleFileHf {
                org: "unsloth".into(),
                repo: "Repo-GGUF".into(),
                quant: "Q8_0".into()
            }
        );
    }

    #[test]
    fn plan_sharded_sorts_parts_and_builds_modelfile() {
        let matched = vec![
            gguf("repo-Q8_0-00002-of-00002.gguf", 20),
            gguf("repo-Q8_0-00001-of-00002.gguf", 10),
        ];
        let plan = plan_hf("unsloth", "Repo-GGUF", "Q8_0", &matched, None).unwrap();
        match plan {
            PullPlan::ShardedHf {
                parts,
                modelfile,
                name,
                ..
            } => {
                assert_eq!(parts[0], "repo-Q8_0-00001-of-00002.gguf");
                assert_eq!(parts[1], "repo-Q8_0-00002-of-00002.gguf");
                assert_eq!(modelfile, "FROM ./repo-Q8_0-00001-of-00002.gguf\n");
                assert_eq!(name, "repo-gguf-q8_0");
            }
            other => panic!("expected sharded, got {other:?}"),
        }
    }

    #[test]
    fn plan_sharded_name_override() {
        let matched = vec![
            gguf("a-00001-of-00002.gguf", 1),
            gguf("a-00002-of-00002.gguf", 1),
        ];
        let plan = plan_hf("o", "r", "Q8_0", &matched, Some("my-model")).unwrap();
        match plan {
            PullPlan::ShardedHf { name, .. } => assert_eq!(name, "my-model"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_no_match_errors() {
        let err = plan_hf("o", "r", "Q8_0", &[], None).unwrap_err();
        assert!(err.contains("Q8_0"), "{err}");
    }

    // --- sanitize ------------------------------------------------------

    #[test]
    fn sanitize_lowercases_and_replaces_slashes() {
        assert_eq!(
            default_ollama_name("Org/Repo-GGUF", "Q8_0"),
            "org-repo-gguf-q8_0"
        );
    }

    #[test]
    fn sanitize_strips_gguf_and_illegal_chars() {
        assert_eq!(sanitize_ollama_name("Weird Name!@#.gguf"), "weird-name");
    }

    #[test]
    fn sanitize_collapses_and_trims_dashes() {
        assert_eq!(sanitize_ollama_name("--a///b--"), "a-b");
        assert_eq!(sanitize_ollama_name("..keep.it.."), "keep.it");
    }

    // --- fit_check -----------------------------------------------------

    #[test]
    fn fit_fits() {
        let v = fit_check(10, Some(100));
        assert_eq!(
            v,
            FitVerdict::Fits {
                model_bytes: 10,
                mem_bytes: 100
            }
        );
        assert!(!v.should_refuse());
    }

    #[test]
    fn fit_exceeds_refuses() {
        let v = fit_check(200, Some(100));
        assert!(v.should_refuse());
        assert!(matches!(v, FitVerdict::Exceeds { .. }));
    }

    #[test]
    fn fit_undetectable_does_not_refuse() {
        let v = fit_check(200, None);
        assert!(!v.should_refuse());
        assert_eq!(v, FitVerdict::Undetectable { model_bytes: 200 });
    }

    #[test]
    fn total_bytes_sums_known_sizes() {
        let files = vec![
            gguf("a", 10),
            GgufFile {
                path: "b".into(),
                size: None,
            },
            gguf("c", 5),
        ];
        assert_eq!(total_bytes(&files), 15);
    }

    #[test]
    fn parse_free_bytes_reads_first_int() {
        assert_eq!(parse_free_bytes("134217728000\n"), Some(134217728000));
        assert_eq!(parse_free_bytes(""), None);
        assert_eq!(parse_free_bytes("not-a-number"), None);
    }

    #[test]
    fn gib_rendering() {
        assert!((bytes_to_gib(1024 * 1024 * 1024) - 1.0).abs() < 1e-9);
    }

    // --- remote script + ssh argv -------------------------------------

    #[test]
    fn sharded_script_has_curl_per_shard_and_create() {
        let parts = vec![
            "Q8_0/m-00001-of-00002.gguf".to_string(),
            "Q8_0/m-00002-of-00002.gguf".to_string(),
        ];
        let mf = modelfile_body("m-00001-of-00002.gguf");
        let script = sharded_remote_script("unsloth", "Repo-GGUF", &parts, &mf, "repo-q8_0", true);
        assert!(script.contains("set -eu"));
        assert!(script.contains("mkdir -p"));
        assert_eq!(script.matches("curl -L --fail -C -").count(), 2);
        assert!(script.contains("Authorization: Bearer $HF_TOKEN"));
        assert!(script.contains(
            "https://huggingface.co/unsloth/Repo-GGUF/resolve/main/Q8_0/m-00001-of-00002.gguf"
        ));
        assert!(script.contains("FROM ./m-00001-of-00002.gguf"));
        assert!(script.contains("ollama create 'repo-q8_0' -f Modelfile"));
    }

    #[test]
    fn sharded_script_omits_auth_without_token() {
        let parts = vec![
            "a-00001-of-00002.gguf".into(),
            "a-00002-of-00002.gguf".into(),
        ];
        let mf = modelfile_body("a-00001-of-00002.gguf");
        let script = sharded_remote_script("o", "r", &parts, &mf, "n", false);
        assert!(!script.contains("Authorization"));
    }

    #[test]
    fn ssh_argv_user_host_default_port() {
        let argv = ssh_argv("bob", "dgx.home.lab", None, "echo hi");
        assert_eq!(argv, vec!["ssh", "bob@dgx.home.lab", "echo hi"]);
    }

    #[test]
    fn ssh_argv_custom_port() {
        let argv = ssh_argv("bob", "dgx", Some(2222), "echo hi");
        assert_eq!(argv, vec!["ssh", "-p", "2222", "bob@dgx", "echo hi"]);
    }

    #[test]
    fn single_file_remote_command_render() {
        assert_eq!(
            single_file_remote_command("unsloth", "Repo-GGUF", "Q8_0"),
            "ollama pull 'hf.co/unsloth/Repo-GGUF:Q8_0'"
        );
    }

    #[test]
    fn ollama_native_remote_command_render() {
        assert_eq!(
            ollama_native_remote_command("qwen2.5-coder:32b"),
            "ollama pull 'qwen2.5-coder:32b'"
        );
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }
}
