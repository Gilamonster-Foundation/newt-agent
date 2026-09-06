use super::*;

/// Write one backend as a per-file drop-in `<config dir>/backends/<name>.toml`
/// (#1140, epic #1126) — the shape the backend assembly's drop-in merge reads
/// back. The
/// canonical writer for `newt init` / `newt setup`: one endpoint, one file,
/// provenance-stamped by the caller. Returns the written path.
pub fn write_backend_dropin(
    config_path: &std::path::Path,
    backend: &BackendConfig,
) -> std::result::Result<std::path::PathBuf, String> {
    let config_destination = crate::atomic_fs::ResolvedPath::resolve(config_path)
        .map_err(|error| format!("resolve config destination: {error:#}"))?;
    let _lock = crate::atomic_fs::acquire_lock(&config_destination.lock_path())
        .map_err(|error| format!("lock {}: {error:#}", config_path.display()))?;
    write_backend_dropin_unlocked(config_path, backend)
}

fn write_backend_dropin_unlocked(
    config_path: &std::path::Path,
    backend: &BackendConfig,
) -> std::result::Result<std::path::PathBuf, String> {
    if backend.name.trim().is_empty() {
        return Err("backend drop-in needs a name (it becomes the filename)".into());
    }
    let dir = config_path.with_file_name("backends");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", backend.name));
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path)
        .map_err(|e| format!("resolve {}: {e:#}", path.display()))?;
    // Every canonical operator write is TAGGED `operator_v1` —
    // UNCONDITIONALLY, injected at the file boundary by the ONE shared
    // renderer ([`render_operator_backend_dropin`]). `BackendConfig`
    // carries no `record` field at all, so there is no in-memory tag to
    // launder through this channel; probe persistence has its own API
    // ([`persist_probe_observation`]).
    let body = render_operator_backend_dropin(backend)?;
    destination
        .atomic_write(body.as_bytes())
        .map_err(|e| format!("write {}: {e:#}", path.display()))?;
    Ok(path)
}

/// Who owns a backend drop-in FILE — the public ownership view for setup /
/// panel surfaces ("may I edit this file?", "is this a probe cache?"),
/// without exposing the raw on-disk tag vocabulary. Ownership is about the
/// FILE: [`crate::BackendConfig`] deliberately carries no tag, so this is
/// decided from raw text ([`classify_backend_dropin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropinOwnership {
    /// Operator-owned: explicitly tagged as operator configuration, or
    /// untagged (hand-authored files and every legacy operator writer).
    /// Panels may edit it; the probe writeback never touches it.
    Operator,
    /// Machine-owned probe record: explicitly tagged, or the unambiguous
    /// legacy probe cache. The runtime rewrites it wholesale; delete (or
    /// [`claim_backend_dropin_as_operator`]) to take it over.
    Probe,
}

/// Classify a backend drop-in file's raw text — the SAME ownership decision
/// the loader and [`persist_probe_observation`] make, exposed for panel /
/// setup surfaces. Ownership only: a probe-owned file that later fails the
/// strict probe schema is still probe-owned (and will be skipped, not
/// reinterpreted, by the loader).
///
/// # Errors
/// Malformed TOML, and the legacy ambiguity (the exact old newt-adopt probe
/// marker beside binding/operator evidence) with both remediations.
pub fn classify_backend_dropin(text: &str) -> std::result::Result<DropinOwnership, String> {
    match disk_record_tag(text)? {
        Some(RecordTag::ProbeV1) => Ok(DropinOwnership::Probe),
        Some(RecordTag::OperatorV1) => Ok(DropinOwnership::Operator),
        None => {
            let backend = toml::from_str::<BackendConfig>(text).map_err(|e| e.to_string())?;
            match classify_untagged_dropin(&backend, text)? {
                DropinOwner::Operator => Ok(DropinOwnership::Operator),
                DropinOwner::Probe => Ok(DropinOwnership::Probe),
            }
        }
    }
}

/// The canonical operator drop-in body: the ownership stamp as the first
/// top-level key (always valid TOML), then the backend's serialization,
/// byte-identical to serializing `backend` alone. The ONE producer of
/// operator-record bytes — [`write_backend_dropin`] writes exactly this,
/// and a panel/setup surface that builds file bodies itself must use it
/// rather than hand-roll the stamp.
///
/// # Errors
/// Serialization failure, as a human-readable string.
pub fn render_operator_backend_dropin(
    backend: &BackendConfig,
) -> std::result::Result<String, String> {
    let serialized = toml::to_string(backend).map_err(|e| format!("serialize backend: {e}"))?;
    Ok(format!("record = \"operator_v1\"\n{serialized}"))
}

/// Claim a drop-in file as OPERATOR configuration — retag a probe record
/// (or tag an untagged file) **preserving comments, key order, and every
/// key newt does not model**, unlike a serde round-trip. The panel's "keep
/// this probed result as my configuration" edit; idempotent on a file that
/// is already operator-tagged.
///
/// # Errors
/// Text that is not valid TOML.
pub fn claim_backend_dropin_as_operator(text: &str) -> std::result::Result<String, String> {
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("backend drop-in is not valid TOML: {e}"))?;
    let root = doc.as_table_mut();
    match root.get_mut("record") {
        // Retag IN PLACE, keeping the existing value's decor — the trailing
        // comment on `record = "probe_v1"  # ownership note` is the
        // operator's annotation, and a blunt replacement would drop it.
        Some(item) if item.is_value() => {
            let value = item.as_value_mut().expect("checked is_value");
            let decor = value.decor().clone();
            *value = toml_edit::Value::from("operator_v1");
            *value.decor_mut() = decor;
        }
        // A `[record]` table or `[[record]]` array is NOT an ownership tag
        // — refuse rather than overwrite someone's data with a stamp.
        Some(_) => {
            return Err(
                "this drop-in has a `[record]` table/array where the ownership tag \
                 would go — refusing to overwrite it; rename or remove that table \
                 first, then claim the file"
                    .to_string(),
            );
        }
        // Absent: stamp a fresh top-level key.
        None => {
            root.insert("record", toml_edit::value("operator_v1"));
        }
    }
    Ok(doc.to_string())
}

/// What a session probe/adoption OBSERVED — the ONLY thing the runtime may
/// persist about a backend. Typed so an unpersistable fact is
/// unrepresentable: only an [`ProbedServing::Instance`] carries a model
/// (one artifact = backend truth); a multiplexer's per-session pick and an
/// unestablished axis have no model field to persist at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeObservation {
    /// The configured backend the observation is about (the drop-in filename).
    pub name: String,
    /// The endpoint the probe actually spoke to — the association key.
    pub endpoint: String,
    /// The detected wire protocol, when the probe established one.
    pub kind: Option<BackendKind>,
    /// The detected OpenAI HTTP surface, when probed.
    pub api: Option<OpenAiApi>,
    /// The observed serving principal.
    pub serving: ProbedServing,
}

/// The serving principal a probe observed — the typed gate on model
/// persistence (see [`ProbeObservation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbedServing {
    /// A single-artifact server; its model IS backend truth and may persist.
    Instance { model: Option<String> },
    /// A multi-model server; the adopted model is a per-session pick and has
    /// no field here to persist through.
    Multiplexer,
    /// Serving was not established — nothing about the axis persists.
    Unknown,
}

impl ProbeObservation {
    /// The `(serving, model)` axis pair this observation's typed principal
    /// flattens to — the ONLY conversion, so "model iff Instance" holds by
    /// construction everywhere the observation is applied or serialized.
    #[must_use]
    pub fn serving_axis(&self) -> (Option<Serving>, Option<String>) {
        match &self.serving {
            ProbedServing::Instance { model } => (Some(Serving::Instance), model.clone()),
            ProbedServing::Multiplexer => (Some(Serving::Multiplexer), None),
            ProbedServing::Unknown => (None, None),
        }
    }
}

/// The `record = "probe_v1"` machine record an observation serializes as —
/// only observed fields, never card/capability/auth/tiers/managed/host.
/// Pure; [`persist_probe_observation`] owns the IO.
pub(super) fn probe_machine_record(observation: &ProbeObservation) -> ProbeRecordV1 {
    let (serving, model) = observation.serving_axis();
    ProbeRecordV1 {
        name: Some(observation.name.clone()),
        endpoint: observation.endpoint.clone(),
        kind: observation.kind,
        api: observation.api,
        serving,
        model,
        tiers: Vec::new(),
        record: Some(RecordTag::ProbeV1),
        provenance: Some(ProbeProvenanceV1 {
            source: Some(format!(
                "newt adopt v{} (probe_v1 overlay; delete this file to reset)",
                crate::build_info::VERSION_WITH_COMMIT
            )),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: serving.map(|_| true),
        }),
    }
}

/// Who an UNTAGGED drop-in belongs to (files written before [`RecordTag`]
/// existed).
#[derive(Debug)]
pub(super) enum DropinOwner {
    Operator,
    Probe,
}

/// The fully anchored EXACT marker the old runtime writeback stamped:
/// `newt adopt v{version} (probed; delete this file to reset)` — prefix and
/// suffix both anchored, nonempty version between. A near-prefix, a
/// near-suffix, or any custom source is NOT this marker.
fn is_legacy_adopt_probe_marker(source: &str) -> bool {
    source
        .strip_prefix("newt adopt v")
        .and_then(|rest| rest.strip_suffix(" (probed; delete this file to reset)"))
        .is_some_and(|version| !version.is_empty())
}

/// Classify an untagged backend drop-in. **Untagged is Operator by
/// default** — the hand-authored file, the old `newt setup v{…}` /
/// `newt init v{…}` / provider-preset (`newt setup v{…} (preset {name})`)
/// writers, and every custom or probe-stamped source alike: a generic
/// `provenance.probed` timestamp proves nothing (operator writers stamped
/// one too) and is never branched on.
///
/// The ONE exception is the fully anchored exact historical newt-adopt
/// probe marker ([`is_legacy_adopt_probe_marker`]). A file carrying exactly
/// that marker is judged on its RAW key shape (`text`, through the strict
/// [`ProbeRecordV1`] whitelist — the permissive [`BackendConfig`] parse
/// silently DROPS unknown evidence and must not decide this):
///
/// * the strict MODEL-LESS probe shape (endpoint/kind/api/serving only,
///   empty `tiers`, no unknown keys top-level or under `[provenance]`) →
///   the legacy probe cache, [`DropinOwner::Probe`] — overlaid under
///   today's probe rules and migrated on next writeback;
/// * ANYTHING else beside the marker — a `model` (whatever the serving
///   axis), a `card`, auth/tiers/managed/…, or any UNKNOWN key (evidence
///   the old writer never produced) — is genuinely ambiguous: hard-error
///   with both remediations rather than guess.
pub(super) fn classify_untagged_dropin(
    b: &BackendConfig,
    text: &str,
) -> std::result::Result<DropinOwner, String> {
    let source = b
        .provenance
        .as_ref()
        .and_then(|p| p.source.as_deref())
        .unwrap_or("");
    if !is_legacy_adopt_probe_marker(source) {
        return Ok(DropinOwner::Operator);
    }
    let strict = toml::from_str::<ProbeRecordV1>(text);
    if let Ok(record) = &strict {
        if record.model.is_none() && record.tiers.is_empty() {
            return Ok(DropinOwner::Probe);
        }
    }
    let carried = match (b.model.is_some(), b.card.is_some(), strict.is_err()) {
        (true, true, _) => "a model and a card",
        (true, false, _) => "a model",
        (false, true, _) => "a card",
        (false, false, true) => "keys outside the old probe cache's raw shape",
        (false, false, false) => "operator fields beside the probe marker",
    };
    Err(format!(
        "this backend drop-in carries the old newt-adopt probe marker but also \
         {carried} — written by an older newt, its declarations cannot be \
         attributed: as an operator record (A) they replace the configured \
         backend wholesale; as a probe overlay (B) they are per-session residue \
         that must be discarded. Refusing to guess — delete the file to \
         re-probe, or add `record = \"operator_v1\"` to claim it as \
         configuration."
    ))
}

/// A probe record may carry ONLY what a probe can observe: `endpoint` (the
/// association key, nonempty), `kind`, `api`, `serving`, and `model` iff
/// `serving = "instance"`. Enforced on load AND around every write, so a
/// hand-edited or corrupted `probe_v1` file cannot smuggle operator fields
/// through the machine-owned channel.
fn validate_probe_record(r: &ProbeRecordV1) -> std::result::Result<(), String> {
    if r.endpoint.trim().is_empty() {
        return Err("probe record has no endpoint (the association key)".to_string());
    }
    if r.model.is_some() && r.serving != Some(Serving::Instance) {
        return Err(
            "probe record carries a model without serving = \"instance\" — only an \
             instance's model is backend truth"
                .to_string(),
        );
    }
    // Operator-owned keys are UNREPRESENTABLE in [`ProbeRecordV1`] (denied
    // at parse). The one legacy leftover the schema tolerates on read is an
    // empty `tiers = []`; a NONEMPTY one is operator configuration.
    if !r.tiers.is_empty() {
        return Err(
            "probe record carries operator-owned field `tiers` — a probe overlay may \
             hold only endpoint/kind/api/serving (plus an instance's model)"
                .to_string(),
        );
    }
    Ok(())
}

/// The PRIVATE raw header of a backend drop-in file — the only place the
/// `record` ownership key is read. [`BackendConfig`] deliberately does NOT
/// carry the tag: ownership is a property of the FILE, decided at the disk
/// boundary, and a tag smuggled through the in-memory config type was how a
/// probe record could try to launder itself through the operator writer.
#[derive(Deserialize)]
struct DiskRecordHeader {
    #[serde(default)]
    record: Option<RecordTag>,
}

/// The `record` tag of a drop-in's raw text, if any. Unknown sibling keys
/// are ignored — this reads the header, nothing else.
pub(super) fn disk_record_tag(text: &str) -> std::result::Result<Option<RecordTag>, String> {
    toml::from_str::<DiskRecordHeader>(text)
        .map(|h| h.record)
        .map_err(|e| e.to_string())
}

/// The strict machine-record schema for a probe drop-in — a
/// `deny_unknown_fields` mirror of the probe-legal subset of
/// [`BackendConfig`]. [`BackendConfig`] itself tolerates unknown TOML keys
/// (forward compatibility for operator files), which means parsing a probe
/// record through it silently DROPS whatever a hand-edit smuggled in —
/// [`validate_probe_record`] can only reject what survives the parse. Probe
/// records are machine-owned, so they get the opposite contract: an unknown
/// key is a hard parse error, an operator-owned key doubly so (it is
/// unknown HERE by construction).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProbeRecordV1 {
    /// The filename stem is authoritative, but the body may repeat it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default)]
    endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<OpenAiApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) serving: Option<Serving>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    /// Accepted on READ only: the old writeback serialized its
    /// `BackendConfig` patch verbatim, so genuine legacy probe caches carry
    /// a literal `tiers = []`. [`validate_probe_record`] still rejects a
    /// NONEMPTY value; the writer never emits the key again.
    #[serde(default, skip_serializing)]
    tiers: Vec<Tier>,
    /// Absent on a legacy (pre-[`RecordTag`]) probe cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) record: Option<RecordTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<ProbeProvenanceV1>,
}

/// Strict mirror of [`BackendProvenance`] for probe records — the parent is
/// permissive (operator files get forward compatibility), so reusing it
/// here would let unknown NESTED keys deserialize away and the strictness
/// of [`ProbeRecordV1`] would stop one level deep.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeProvenanceV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    derived_serving: Option<bool>,
}

/// Parse the raw text of a probe-owned drop-in through the strict
/// [`ProbeRecordV1`] schema. Callers still run [`validate_probe_record`] on
/// the result (endpoint nonempty, model iff instance) — this layer's job is
/// the key set, which the permissive [`BackendConfig`] parse cannot police.
pub(super) fn parse_probe_record(text: &str) -> std::result::Result<ProbeRecordV1, String> {
    let r: ProbeRecordV1 =
        toml::from_str(text).map_err(|e| format!("not a valid probe record: {e}"))?;
    validate_probe_record(&r).map(|()| r)
}

impl ProbeRecordV1 {
    /// The typed observation a validated record attests — `name` supplied by
    /// the caller (the filename stem is authoritative for drop-ins).
    pub(super) fn to_observation(&self, name: &str) -> ProbeObservation {
        ProbeObservation {
            name: name.to_string(),
            endpoint: self.endpoint.clone(),
            kind: self.kind,
            api: self.api,
            serving: match (self.serving, &self.model) {
                (Some(Serving::Instance), model) => ProbedServing::Instance {
                    model: model.clone(),
                },
                (Some(Serving::Multiplexer), _) => ProbedServing::Multiplexer,
                (None, _) => ProbedServing::Unknown,
            },
        }
    }
}

/// The visible outcome of a probe writeback — persistence is explicitly
/// owned, so "did not write" states are typed, never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeWriteback {
    /// The probe_v1 record was created or updated at this path.
    Written(std::path::PathBuf),
    /// The same-name path is operator-owned (`operator_v1` or untagged) —
    /// its bytes and comments were left untouched.
    SkippedOperatorOwned(std::path::PathBuf),
    /// No user config dir, or an unnamed backend — nothing to persist to.
    NotWritten,
}

/// Persist a probe observation as `~/.newt/backends/<name>.toml` (or under
/// `$NEWT_CONFIG_DIR`) — never into the main `config.toml`. Reset = delete
/// that one file.
///
/// Creates or updates ONLY probe-owned files. An existing same-name file
/// that is operator-owned (tagged `operator_v1`, or untagged and classified
/// operator — same classifier as the loader) is returned as
/// [`ProbeWriteback::SkippedOperatorOwned`] byte-for-byte untouched — the
/// runtime never rewrites operator configuration. An unambiguous LEGACY
/// probe cache (untagged, exact old adopt marker, probe-shaped) is treated
/// as the prior probe record and MIGRATES to tagged `probe_v1` on this
/// write; the genuinely ambiguous legacy file hard-errors with both
/// remediations, exactly as on load. An update re-serializes the probe
/// schema, carrying forward the prior probe file's `kind`/`api` only when
/// its endpoint equals this observation's — `serving`/`model` are NEVER
/// carried, so an Instance-observed model is REMOVED the moment a later
/// observation sees a multiplexer (or nothing).
///
/// # Errors
/// Lock/read/parse/serialize/write failures — and the legacy ambiguity —
/// as human-readable strings.
pub fn persist_probe_observation(
    observation: &ProbeObservation,
) -> std::result::Result<ProbeWriteback, String> {
    if observation.name.trim().is_empty() {
        return Ok(ProbeWriteback::NotWritten);
    }
    let Some(config_path) = Config::user_config_path() else {
        return Ok(ProbeWriteback::NotWritten);
    };
    let config_destination = crate::atomic_fs::ResolvedPath::resolve(&config_path)
        .map_err(|error| format!("resolve config destination: {error:#}"))?;
    let _lock = crate::atomic_fs::acquire_lock(&config_destination.lock_path())
        .map_err(|error| format!("lock {}: {error:#}", config_path.display()))?;
    let dir = config_path.with_file_name("backends");
    let path = dir.join(format!("{}.toml", observation.name));
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path)
        .map_err(|e| format!("resolve {}: {e:#}", path.display()))?;
    let mut merged = probe_machine_record(observation);
    if destination.as_path().is_file() {
        let text = std::fs::read_to_string(destination.as_path())
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        // Ownership is decided the SAME way the loader decides it — the raw
        // `record` header, else the legacy classifier — or an unambiguous
        // legacy probe cache would permanently block refresh.
        let owned_by_probe =
            match disk_record_tag(&text).map_err(|e| format!("parse {}: {e}", path.display()))? {
                Some(RecordTag::ProbeV1) => true,
                Some(RecordTag::OperatorV1) => false,
                None => {
                    let prior = toml::from_str::<BackendConfig>(&text)
                        .map_err(|e| format!("parse {}: {e}", path.display()))?;
                    match classify_untagged_dropin(&prior, &text) {
                        Ok(DropinOwner::Probe) => true,
                        Ok(DropinOwner::Operator) => false,
                        Err(reason) => return Err(format!("{}: {reason}", path.display())),
                    }
                }
            };
        if !owned_by_probe {
            return Ok(ProbeWriteback::SkippedOperatorOwned(path));
        }
        let prior = parse_probe_record(&text).map_err(|e| {
            format!(
                "{}: existing probe record is invalid ({e}) — delete it to re-probe",
                path.display()
            )
        })?;
        // Prior fields may be reused only for the SAME endpoint — an
        // endpoint change means every prior observation was about some
        // other server. serving/model are NEVER carried forward at all:
        // stale principal evidence must not be re-stamped under a fresh
        // probe date (an Unknown/model-less observation writes an
        // empty-principal record, it does not refresh the old one).
        if prior.endpoint == observation.endpoint {
            merged.kind = merged.kind.or(prior.kind);
            merged.api = merged.api.or(prior.api);
        }
    }
    validate_probe_record(&merged)
        .map_err(|e| format!("refusing to write an invalid probe record: {e}"))?;
    let body = toml::to_string(&merged).map_err(|e| format!("serialize probe record: {e}"))?;
    destination
        .atomic_write(body.as_bytes())
        .map_err(|e| format!("write {}: {e:#}", path.display()))?;
    Ok(ProbeWriteback::Written(path))
}

/// Deprecated compatibility shim for the pre-#1819 writeback API, which
/// took a raw [`BackendConfig`] patch and merged it into the drop-in. The
/// typed channel is [`persist_probe_observation`]; this shim converts the
/// patch — and REFUSES, before any write, a patch the typed channel cannot
/// represent, instead of reporting a lossy conversion as success:
///
/// * a `model` without `serving = "instance"` (a per-session pick is not
///   persistable backend truth);
/// * any operator-owned field (card, capability, auth, tiers, managed,
///   host, coexist, ram_gib, engine, model_path).
///
/// An operator-owned same-name file is likewise an ERROR naming the path —
/// the old API's `Ok(Some(path))` meant "persisted", and silently not
/// persisting is not compatibility. `Ok(None)` is returned only for the
/// true nothing-to-do cases (unnamed backend, no user config dir).
#[deprecated(note = "use persist_probe_observation — probe persistence is typed (#1819)")]
pub fn writeback_probed_backend(
    patch: &BackendConfig,
) -> std::result::Result<Option<std::path::PathBuf>, String> {
    if patch.model.is_some() && patch.serving != Some(Serving::Instance) {
        return Err(
            "probe writeback carries a model without serving = \"instance\" — only an \
             instance's model is backend truth; use persist_probe_observation"
                .to_string(),
        );
    }
    let operator_owned: &[(&str, bool)] = &[
        ("card", patch.card.is_some()),
        ("capability", patch.capability.is_some()),
        ("api_key_env", patch.api_key_env.is_some()),
        ("api_key_file", patch.api_key_file.is_some()),
        ("managed", patch.managed.is_some()),
        ("host", patch.host.is_some()),
        ("coexist", patch.coexist.is_some()),
        ("ram_gib", patch.ram_gib.is_some()),
        ("engine", patch.engine.is_some()),
        ("model_path", patch.model_path.is_some()),
        ("tiers", !patch.tiers.is_empty()),
    ];
    if let Some((field, _)) = operator_owned.iter().find(|(_, present)| *present) {
        return Err(format!(
            "probe writeback carries operator-owned field `{field}` — a probe record may \
             hold only endpoint/kind/api/serving (plus an instance's model); use \
             write_backend_dropin for operator configuration"
        ));
    }
    let serving = match patch.serving {
        Some(Serving::Instance) => ProbedServing::Instance {
            model: patch.model.clone(),
        },
        Some(Serving::Multiplexer) => ProbedServing::Multiplexer,
        None => ProbedServing::Unknown,
    };
    let observation = ProbeObservation {
        name: patch.name.clone(),
        endpoint: patch.endpoint.clone(),
        kind: patch.kind,
        api: patch.api,
        serving,
    };
    match persist_probe_observation(&observation)? {
        ProbeWriteback::Written(path) => Ok(Some(path)),
        ProbeWriteback::SkippedOperatorOwned(path) => Err(format!(
            "{}: the same-name drop-in is operator-owned — the probe record was NOT \
             written (delete the file, or keep it and stop probing this backend)",
            path.display()
        )),
        ProbeWriteback::NotWritten => Ok(None),
    }
}
