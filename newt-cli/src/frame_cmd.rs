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
//! * `<content-id>.json` — a unit, root event or packet. Read back through
//!   **decode → admit → recompute the id → compare to the filename**, which is
//!   the `NodeStore::get` (verified) half. The stored bytes are never trusted:
//!   the id is recomputed from the canonical form of the decoded value.
//! * `<raw-content-id>` — opaque source bytes. Returned **unverified**, because
//!   hashing them is precisely what [`agent_frame::verify_unit`] does. That is
//!   the `get_unverified` half, and keeping it unverified here is what makes
//!   the mismatch case reachable and testable.

use std::path::{Path, PathBuf};

use agent_frame::{
    verify_unit, Packet, PacketId, RawUnit, RootEvent, SourceResolver, Unit, UnitId, VerifyError,
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use content_addressable::{ContentAddressable, ContentId, RawContentId};
use serde::Serialize;

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
    /// Report which source a unit represents, which range of it, under which
    /// operation, on whose authority, and at what depth.
    Explain {
        /// The unit's content id.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Report the IMMEDIATE parent link — one step, never a walk.
    ///
    /// Three outcomes, deliberately never conflated: `genesis` (no parents —
    /// the chain ended, and the answer is complete), `parent` (the link is
    /// named, follow it by calling again), or a loud failure to resolve the
    /// subject. An absent parent meaning "origin" and an absent parent meaning
    /// "I could not read it" are different facts and must not render alike.
    Parents {
        /// A packet or unit content id.
        cid: String,
        /// Frame store directory (default: `~/.newt/frame`).
        #[arg(long)]
        frame: Option<PathBuf>,
        /// Emit the report as JSON.
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

    /// Read a structured node, **verifying** that it hashes to the id it was
    /// filed under. The stored bytes are not trusted.
    fn read_node<T, F>(&self, id: &ContentId, decode: F) -> Result<T>
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
        self.read_node(id.as_content_id(), |bytes| {
            let raw: RawUnit = serde_json::from_slice(bytes).context("decoding the stored unit")?;
            Unit::try_from(raw).map_err(|e| {
                anyhow::anyhow!("the stored unit is not admissible under the v0 contract: {e}")
            })
        })
    }

    fn root(&self, id: &ContentId) -> Result<RootEvent> {
        self.read_node(id, |bytes| {
            serde_json::from_slice(bytes).context("decoding the stored root event")
        })
    }

    fn packet(&self, id: &PacketId) -> Result<Packet> {
        self.read_node(id.as_content_id(), |bytes| {
            serde_json::from_slice(bytes).context("decoding the stored packet")
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
        FrameCmd::Explain { cid, frame, json } => run_explain(cid, frame.clone(), *json),
        FrameCmd::Parents { cid, frame, json } => run_parents(cid, frame.clone(), *json),
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

fn run_explain(cid: &str, frame: Option<PathBuf>, json: bool) -> Result<i32> {
    let store = FrameStore::open(frame)?;
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

/// **One link. There is no walker here, and that is deliberate.**
///
/// `Unit.wf` says `depth <= 1`: nothing is more than one derivation from source
/// material. So one-link verification *is* complete verification — there is no
/// deeper chain to regress into, by construction rather than by policy. Genesis
/// (a frame with no parents) is the defined bottom. The forensic obligation and
/// the depth bound are the same constraint seen from two directions, which is
/// also why refusing `concise` and `generate` in v0 is not merely caution: it
/// is what keeps regress impossible.
///
/// A caller who wants the next link calls this command again on the id it
/// reported. The recursion lives in the caller, by composition — we ship none
/// of it, so there is no unbounded walk over data we do not control.
fn run_parents(cid: &str, frame: Option<PathBuf>, json: bool) -> Result<i32> {
    let store = FrameStore::open(frame)?;
    let pid = cid
        .parse::<PacketId>()
        .map_err(|e| anyhow::anyhow!("`{cid}` is not a content id: {e}"))?;

    // A packet has a parent link; a unit does not. Try reading a packet first
    // and fall back to reading a unit — the id carries no type tag, so there is
    // nothing to branch on but the read.
    let report = match store.packet(&pid) {
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
