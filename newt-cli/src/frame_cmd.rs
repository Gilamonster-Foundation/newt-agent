//! `newt frame` — forensics over the derivation frame.
//!
//! # The same verifier the harness runs
//!
//! Every verification decision here is [`agent_frame::verify_unit`]. This
//! module fetches bytes and renders reports; it does not decide whether a unit
//! is what it claims. A CLI that re-implemented the check would be a second
//! answer to the same question, and the one that quietly disagrees is the one
//! that ships — so the seam is deliberately thin enough that there is nowhere
//! for a second opinion to live.
//!
//! # Structured first, pretty second (CRAFT-20)
//!
//! Each subcommand builds ONE `Serialize` report and then either prints it as
//! JSON or renders it. The human form is a projection of the report, never a
//! separate assembly of facts — so a fact cannot exist only in the pretty
//! output, which is exactly the failure the rule names.
//!
//! # One link, and the recursion lives in the caller
//!
//! `verify` checks ONE derivation: this unit, against the source it names. It
//! does not resolve the source's own ancestry, and it does not fail because the
//! source is itself derived — that is the source's business.
//!
//! This is complete rather than merely pragmatic, because `Unit.wf` bounds
//! depth at 1: nothing is more than one derivation from source material, and a
//! frame with no parents is genesis, the defined bottom. There is no deeper
//! chain to regress into, by construction.
//!
//! `verify` still **emits the source id it checked against**, so a caller who
//! wants the next link composes rather than asking us for a walker:
//!
//! ```text
//! newt frame verify <cid> --json | jq -r .source
//! ```
//!
//! One caveat worth knowing before piping that straight back in: a source is an
//! **opaque byte string** with a `raw`-profile id, and a unit id is a
//! `dag-cbor`-profile id. They are different types on purpose. If a source is
//! itself a derived artifact, the unit that derived it has its own separate id,
//! and mapping one to the other is the caller's step — this command will not
//! guess it. The report says `source_profile` so that is visible rather than
//! discovered.
//!
//! # The store
//!
//! A directory of content-addressed files (`~/.newt/frame` by default). Two
//! profiles, matching the two id types:
//!
//! * `<content-id>.cbor` — canonical records, read through the harness's
//!   verified `FrameStore`. Unit reconstruction uses its shared admission seam.
//! * `<content-id>.json` — earlier forensic fixtures, accepted only when the
//!   canonical record is absent. This read-only migration boundary retains
//!   decode, admission and identity checks; corruption never falls back to JSON.
//! * `<raw-content-id>` — opaque source bytes. Returned **unverified**, because
//!   hashing them is precisely what [`agent_frame::verify_unit`] does. That is
//!   the `get_unverified` half, and keeping it unverified here is what makes
//!   the mismatch case reachable and testable.

use std::io::Write;
use std::path::{Path, PathBuf};

use agent_frame::{
    verify_unit, Packet, PacketAdmitError, PacketId, RawPacket, RawUnit, RootEvent, SourceResolver,
    Unit, UnitId, VerifyError,
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use content_addressable::{ContentAddressable, ContentId, RawContentId};
use serde::Serialize;

/// Explicit read bounds for smart-frame inspection; no ancestry is traversed.
#[derive(clap::Args, Debug, Clone, Copy)]
pub struct InspectionArgs {
    #[arg(long, default_value_t = agent_harness::forensics::InspectionLimits::default().max_bytes)]
    max_bytes: usize,
    #[arg(long, default_value_t = agent_harness::forensics::InspectionLimits::default().max_references)]
    max_references: usize,
}

impl From<InspectionArgs> for agent_harness::forensics::InspectionLimits {
    fn from(value: InspectionArgs) -> Self {
        Self {
            max_bytes: value.max_bytes,
            max_references: value.max_references,
        }
    }
}

/// Forensics subcommands.
#[derive(Subcommand, Debug)]
pub enum FrameCmd {
    /// Recompute a unit's claims from its source and compare. **Exits non-zero
    /// on mismatch**, so a script can gate on it.
    Verify {
        /// The unit's content id.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Read the source from this file instead of the store.
        #[arg(long)]
        source: Option<PathBuf>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Explain a unit, causal event, request, projection, or session journal entry.
    Explain {
        /// The addressed record's content id, including a solve's reported head.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        limits: InspectionArgs,
    },
    /// Report immediate parents and references, without traversing ancestry.
    /// Missing or substituted immediate references fail visibly.
    Parents {
        /// A packet, unit, causal event, or journal entry content id.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        limits: InspectionArgs,
    },
    /// Reconstruct a request and verify its recorded dispatch commitment.
    /// Writes the exact request bytes to stdout, without an added newline.
    Replay {
        /// The recorded request's content id.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Emit a JSON receipt containing the verified request body.
        #[arg(long)]
        json: bool,
    },
}

// ---- the store -------------------------------------------------------------

/// A directory of content-addressed files.
pub struct FrameStore {
    dir: PathBuf,
}

impl FrameStore {
    /// Open the store at `dir`, or the default `~/.newt/frame`.
    ///
    /// # Errors
    ///
    /// If no directory was given and no home directory can be resolved.
    pub fn open(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(d) => d,
            None => newt_core::Config::user_config_dir()
                .context("no --frame given and no home directory to default to")?
                .join("frame"),
        };
        Ok(Self { dir })
    }

    /// Where this store lives — reported so a reader knows what was searched.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read_raw(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.dir.join(key);
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    fn canonical(&self) -> Result<agent_harness::store::FrameStore> {
        // Forensics must not create a missing store as a side effect of a read.
        if !self.dir.is_dir() {
            bail!("frame store is not a directory: {}", self.dir.display());
        }
        Ok(agent_harness::store::FrameStore::open(&self.dir)?)
    }

    fn has_canonical(&self, id: &ContentId) -> Result<bool> {
        match std::fs::symlink_metadata(self.dir.join(format!("{id}.cbor"))) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Read the earlier JSON format only when no canonical record exists.
    /// All new writes use the harness store's canonical encoding.
    fn read_legacy_node<T, F>(&self, id: &ContentId, decode: F) -> Result<T>
    where
        F: FnOnce(&[u8]) -> Result<T>,
        T: ContentAddressable,
    {
        let bytes = self.read_raw(&format!("{id}.json"))?;
        let value = decode(&bytes)?;
        let actual = value.content_id().context("recomputing the node's id")?;
        if actual != *id {
            bail!(
                "store corruption: {id}.json decodes to a value whose id is {actual}. \
                 The file is not what it is filed as; nothing here can be trusted."
            );
        }
        Ok(value)
    }

    /// Load and **admit** a unit. Both halves are required: bytes that decode
    /// are not yet a unit.
    fn unit(&self, id: &UnitId) -> Result<Unit> {
        if self.has_canonical(id.as_content_id())? {
            return Ok(self.canonical()?.unit(*id.as_content_id())?);
        }
        self.read_legacy_node(id.as_content_id(), |bytes| {
            let raw: RawUnit = serde_json::from_slice(bytes).context("decoding the stored unit")?;
            Unit::try_from(raw).map_err(|e| {
                anyhow::anyhow!("the stored unit is not admissible under the v0 contract: {e}")
            })
        })
    }

    fn root(&self, id: &ContentId) -> Result<RootEvent> {
        if self.has_canonical(id)? {
            return Ok(self.canonical()?.get(id)?);
        }
        self.read_legacy_node(id, |bytes| {
            serde_json::from_slice(bytes).context("decoding the stored root event")
        })
    }

    /// Load and **admit** a packet — the same two halves as [`Self::unit`].
    ///
    /// A stored node can be perfectly well addressed and still not be a v0
    /// packet: `MerkleNode` carries a parent set, and a two-parent node hashes
    /// to a real id, files under it, and passes the id check. Admission is what
    /// refuses it, so `parents` can no longer report a packet with two parents
    /// as an origin.
    fn packet(&self, id: &PacketId) -> Result<Packet> {
        if self.has_canonical(id.as_content_id())? {
            let raw: RawPacket = self.canonical()?.get(id.as_content_id())?;
            return Packet::try_from(raw)
                .context("the stored packet is not admissible under the v0 contract");
        }
        self.read_legacy_node(id.as_content_id(), |bytes| {
            let raw: RawPacket =
                serde_json::from_slice(bytes).context("decoding the stored packet")?;
            // `context`, not `anyhow!("{e}")`: it keeps the typed
            // `PacketAdmitError` in the chain, which is how `parents` tells
            // "these bytes are not a packet at all" apart from "they are a
            // packet and I refused it".
            Packet::try_from(raw)
                .context("the stored packet is not admissible under the v0 contract")
        })
    }
}

impl SourceResolver for FrameStore {
    /// Unverified by design — see the module docs.
    fn resolve_unverified(&self, id: &RawContentId) -> Result<Vec<u8>, String> {
        self.read_raw(&id.to_string()).map_err(|e| e.to_string())
    }
}

// ---- reports ---------------------------------------------------------------

/// A span, rendered.
#[derive(Serialize)]
pub struct SpanOut {
    start: u64,
    end: u64,
    len: u64,
}

/// The facts a unit's derivation carries. Shared by `verify` and `explain` so
/// the two cannot drift into describing the same unit differently.
#[derive(Serialize)]
pub struct DerivationOut {
    unit: String,
    op: String,
    check: String,
    depth: u32,
    life: String,
    source: String,
    span: SpanOut,
    elided: String,
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_unresolved: Option<String>,
}

fn derivation_out(store: &FrameStore, id: &UnitId, u: &Unit) -> DerivationOut {
    let d = u.derivation();
    let (kind, seq, content, unresolved) = match store.root(&d.root) {
        Ok(ev) => (
            Some(format!("{:?}", ev.kind)),
            Some(ev.seq),
            Some(ev.content.to_string()),
            None,
        ),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    DerivationOut {
        unit: id.to_string(),
        op: format!("{:?}", d.op),
        check: format!("{:?}", d.op.check()),
        depth: u.depth(),
        life: format!("{:?}", u.life()),
        source: d.source.to_string(),
        span: SpanOut {
            start: d.span.start,
            end: d.span.end,
            len: d.span.len(),
        },
        elided: d.elided.to_string(),
        root: d.root.to_string(),
        root_kind: kind,
        root_seq: seq,
        root_content: content,
        root_unresolved: unresolved,
    }
}

/// The `verify` report. `verified` is the machine-readable verdict; `failure`
/// names both sides when it is false.
#[derive(Serialize)]
pub struct VerifyReport {
    command: &'static str,
    store: String,
    verified: bool,
    #[serde(flatten)]
    derivation: DerivationOut,
    source_from: String,
    /// Always `"raw"`: a source is an opaque byte string, not a canonical
    /// value. Reported so a caller composing on `source` can see that it is not
    /// a unit id without having to know the CID profile table.
    source_profile: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<FailureOut>,
}

/// A verification failure, with both sides. "It did not verify" that does not
/// say what did not match sends the reader to a debugger.
#[derive(Serialize)]
pub struct FailureOut {
    kind: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual: Option<String>,
}

impl From<&VerifyError> for FailureOut {
    fn from(e: &VerifyError) -> Self {
        let (kind, declared, actual) = match e {
            VerifyError::SourceMismatch { at, .. } => (
                "source_mismatch",
                Some(at.declared.to_string()),
                Some(at.actual.to_string()),
            ),
            VerifyError::SpanOutOfBounds { .. } => ("span_out_of_bounds", None, None),
            VerifyError::ElisionMismatch { at, .. } => (
                "elision_mismatch",
                Some(at.declared.to_string()),
                Some(at.actual.to_string()),
            ),
            VerifyError::SourceUnavailable { .. } => ("source_unavailable", None, None),
        };
        Self {
            kind,
            message: e.to_string(),
            declared,
            actual,
        }
    }
}

/// The `explain` report.
#[derive(Serialize)]
pub struct ExplainReport {
    command: &'static str,
    store: String,
    #[serde(flatten)]
    derivation: DerivationOut,
    asserts: bool,
    well_formed: bool,
    evictable: bool,
}

/// The `parents` report — **one link**.
///
/// `outcome` is the discriminator a reader keys on, and it exists so that
/// "there is nothing beneath this" and "I could not read what is beneath this"
/// can never be the same rendering. A failure to resolve the subject is not a
/// value of this field at all: it is an error, a non-zero exit and a message on
/// stderr, which is the loudest of the three.
#[derive(Serialize)]
pub struct ParentsReport {
    command: &'static str,
    store: String,
    subject: String,
    kind: &'static str,
    /// `genesis` | `parent` | `antecedents`.
    outcome: &'static str,
    /// Human-readable statement of the outcome, carried so the meaning is in
    /// the data and not only in the renderer.
    meaning: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    units: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    antecedents: Vec<Antecedent>,
}

/// A unit's immediate antecedents — what its derivation addresses.
#[derive(Serialize)]
pub struct Antecedent {
    relation: &'static str,
    id: String,
    profile: &'static str,
}

// ---- rendering -------------------------------------------------------------

fn render_derivation(d: &DerivationOut) {
    println!("  unit    : {}", d.unit);
    println!("  op      : {} (check: {})", d.op, d.check);
    println!("  depth   : {}", d.depth);
    println!("  life    : {}", d.life);
    println!("  source  : {}", d.source);
    println!(
        "  span    : [{}, {})  ({} bytes)",
        d.span.start, d.span.end, d.span.len
    );
    println!("  elided  : {}", d.elided);
    print!("  root    : {}", d.root);
    match (&d.root_kind, d.root_seq) {
        (Some(k), Some(s)) => println!("  ({k}, seq {s})"),
        _ => println!(),
    }
    if let Some(c) = &d.root_content {
        println!("  root material : {c}");
    }
    if let Some(why) = &d.root_unresolved {
        println!("  root UNRESOLVED : {why}");
    }
}

// ---- entry point -----------------------------------------------------------

/// Run a `newt frame` subcommand, returning the process exit code.
///
/// # Errors
///
/// Propagates store and decode failures. A verification *mismatch* is not an
/// error — it is a verdict, and it comes back as a non-zero code with a report.
pub fn run(cmd: &FrameCmd) -> Result<i32> {
    match cmd {
        FrameCmd::Verify {
            cid,
            frame,
            source,
            json,
        } => run_verify(cid, frame.clone(), source.as_deref(), *json),
        FrameCmd::Explain {
            cid,
            frame,
            json,
            limits,
        } => run_explain(cid, frame.clone(), *json, (*limits).into()),
        FrameCmd::Parents {
            cid,
            frame,
            json,
            limits,
        } => run_parents(cid, frame.clone(), *json, (*limits).into()),
        FrameCmd::Replay { cid, frame, json } => run_replay(cid, frame.clone(), *json),
    }
}

fn emit<T: Serialize>(report: &T, json: bool, render: impl FnOnce(&T)) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        render(report);
    }
    Ok(())
}

fn parse_unit(cid: &str) -> Result<UnitId> {
    cid.parse::<UnitId>()
        .map_err(|e| anyhow::anyhow!("`{cid}` is not a content id: {e}"))
}

fn run_replay(cid: &str, frame: Option<PathBuf>, json: bool) -> Result<i32> {
    let store = FrameStore::open(frame)?;
    let request = cid
        .parse::<ContentId>()
        .map_err(|e| anyhow::anyhow!("`{cid}` is not a content id: {e}"))?;
    let bytes = agent_harness::replay_from_store(&store.canonical()?, request)?;
    // Resolve and verify everything before stdout receives any bytes.
    if json {
        let report = serde_json::json!({
            "command": "frame.replay",
            "store": store.dir().display().to_string(),
            "request": cid,
            "verified": true,
            "byte_count": bytes.len(),
            "body": String::from_utf8(bytes)?,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        std::io::stdout().lock().write_all(&bytes)?;
    }
    Ok(0)
}

fn run_verify(cid: &str, frame: Option<PathBuf>, source: Option<&Path>, json: bool) -> Result<i32> {
    let store = FrameStore::open(frame)?;
    let id = parse_unit(cid)?;
    let unit = store.unit(&id)?;

    // ONE verifier. The only choice here is where the bytes come from.
    let (outcome, source_from) = match source {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading --source {}", path.display()))?;
            (verify_unit(&unit, &bytes), path.display().to_string())
        }
        None => (
            agent_frame::verify_unit_with(&unit, &store),
            format!("store:{}", unit.derivation().source),
        ),
    };

    let report = VerifyReport {
        command: "frame.verify",
        store: store.dir().display().to_string(),
        verified: outcome.is_ok(),
        derivation: derivation_out(&store, &id, &unit),
        source_from,
        source_profile: "raw",
        source_len: outcome.as_ref().ok().map(|v| v.source_len),
        failure: outcome.as_ref().err().map(FailureOut::from),
    };

    emit(&report, json, |r| {
        println!(
            "frame verify — {}\n",
            if r.verified {
                "VERIFIED"
            } else {
                "NOT VERIFIED"
            }
        );
        render_derivation(&r.derivation);
        println!("  source from   : {}", r.source_from);
        if let Some(n) = r.source_len {
            println!("  source bytes  : {n}");
        }
        println!(
            "  source profile: {}  (opaque bytes, not a unit id)",
            r.source_profile
        );
        if let Some(f) = &r.failure {
            println!("\n  FAILURE [{}]\n  {}", f.kind, f.message);
        }
    })?;

    Ok(i32::from(!report.verified))
}

fn emit_inspection(
    store: &FrameStore,
    cid: &str,
    json: bool,
    command: &str,
    limits: agent_harness::forensics::InspectionLimits,
) -> Result<bool> {
    let id: ContentId = cid.parse().context("parsing frame content id")?;
    if !store.has_canonical(&id)? {
        return Ok(false);
    }
    let Some(inspection) =
        agent_harness::forensics::inspect_from_store(&store.canonical()?, id, limits)?
    else {
        return Ok(false);
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": command, "store": store.dir().display().to_string(), "inspection": inspection,
            }))?
        );
    } else {
        println!("{command} — {} {}\n", inspection.kind, inspection.id);
        println!("{}", serde_json::to_string_pretty(&inspection.record)?);
        for reference in &inspection.references {
            println!(
                "  {}: {} [{}]",
                reference.relation, reference.cid, reference.profile
            );
        }
        println!("\n{}", inspection.validation);
    }
    Ok(true)
}

fn run_explain(
    cid: &str,
    frame: Option<PathBuf>,
    json: bool,
    limits: agent_harness::forensics::InspectionLimits,
) -> Result<i32> {
    let store = FrameStore::open(frame)?;
    if emit_inspection(&store, cid, json, "frame.explain", limits)? {
        return Ok(0);
    }
    let id = parse_unit(cid)?;
    let unit = store.unit(&id)?;

    let report = ExplainReport {
        command: "frame.explain",
        store: store.dir().display().to_string(),
        derivation: derivation_out(&store, &id, &unit),
        asserts: unit.op().asserts(),
        well_formed: unit.is_well_formed(),
        evictable: unit.evictable(),
    };

    emit(&report, json, |r| {
        println!("frame explain\n");
        render_derivation(&r.derivation);
        println!("  asserts : {}", r.asserts);
        println!("  wf      : {}  (depth <= 1)", r.well_formed);
        println!("  evictable : {}", r.evictable);
    })?;
    Ok(0)
}

/// Inspect one link at a time. Causal ancestry can be arbitrarily long even
/// though generated material has derivation depth at most one. Immediate
/// inspection does not establish full graph admission or execution authority.
fn run_parents(
    cid: &str,
    frame: Option<PathBuf>,
    json: bool,
    limits: agent_harness::forensics::InspectionLimits,
) -> Result<i32> {
    let store = FrameStore::open(frame)?;
    if emit_inspection(&store, cid, json, "frame.parents", limits)? {
        return Ok(0);
    }
    let pid = cid
        .parse::<PacketId>()
        .map_err(|e| anyhow::anyhow!("`{cid}` is not a content id: {e}"))?;

    // A packet has a parent link; a unit does not. Try reading a packet first
    // and fall back to reading a unit — the id carries no type tag, so there is
    // nothing to branch on but the read.
    let report = match store.packet(&pid) {
        // The subject decoded as a packet and was REFUSED. That is a verdict
        // about the packet, not evidence it might be a unit — falling through
        // here would report an "unknown field payload" decode error for a node
        // whose real defect is that it forks the chain.
        Err(e) if e.downcast_ref::<PacketAdmitError>().is_some() => return Err(e),
        Ok(p) => {
            let units: Vec<String> = p.units().iter().map(ToString::to_string).collect();
            match p.prior() {
                None => ParentsReport {
                    command: "frame.parents",
                    store: store.dir().display().to_string(),
                    subject: cid.to_string(),
                    kind: "packet",
                    outcome: "genesis",
                    meaning: "no parents: this frame is an origin, and the chain ends here",
                    parent: None,
                    units,
                    antecedents: Vec::new(),
                },
                Some(prev) => ParentsReport {
                    command: "frame.parents",
                    store: store.dir().display().to_string(),
                    subject: cid.to_string(),
                    kind: "packet",
                    outcome: "parent",
                    meaning: "one parent link, named but NOT followed: run this command on it \
                              to go one link deeper",
                    parent: Some(prev.to_string()),
                    units,
                    antecedents: Vec::new(),
                },
            }
        }
        Err(_) => {
            // Not a packet — a unit, whose antecedents are what its derivation
            // addresses. Reading it can still fail, and that failure propagates
            // rather than being reported as an absence.
            let uid = parse_unit(cid)?;
            let unit = store.unit(&uid)?;
            let d = unit.derivation();
            ParentsReport {
                command: "frame.parents",
                store: store.dir().display().to_string(),
                subject: cid.to_string(),
                kind: "unit",
                outcome: "antecedents",
                meaning: "a unit is a leaf: it names the source it was derived from and the \
                          root event that caused it, and neither is followed here",
                parent: None,
                units: Vec::new(),
                antecedents: vec![
                    Antecedent {
                        relation: "source",
                        id: d.source.to_string(),
                        profile: "raw",
                    },
                    Antecedent {
                        relation: "root",
                        id: d.root.to_string(),
                        profile: "dag-cbor",
                    },
                ],
            }
        }
    };

    emit(&report, json, |r| {
        println!("frame parents — {} {}\n", r.kind, r.subject);
        println!("  outcome : {}", r.outcome);
        println!("  {}", r.meaning);
        if let Some(p) = &r.parent {
            println!("  parent  : {p}");
        }
        for u in &r.units {
            println!("  unit    : {u}");
        }
        for a in &r.antecedents {
            println!("  {:<7} : {}  [{}]", a.relation, a.id, a.profile);
        }
    })?;
    Ok(0)
}
