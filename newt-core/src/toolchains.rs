//! Operator-signed runtime closures, admitted only in the host build harness.
//! Startup freezes the source and verifying key. No tool/config/environment
//! supplied by a running agent can replace that snapshot or sign a policy.
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, ensure, Context, Result};
use content_addressable::{canonical, ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

use crate::ocap_policy::{ApproveVerifier, Ed25519ApproveVerifier};
use crate::sandbox_seam::HeldReadRoot;
use crate::Scope;

/// A directory containing `bin/`; a pin uses exactly `newt caps digest`'s CID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: String,
    pub expected_tree: Option<ContentId>,
}

/// Ordered, domain-separated body. The signature is deliberately outside it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub schema: String,
    pub roots: Vec<Root>,
}
impl ContentAddressable for Body {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sig: Option<String>,
    roots: Vec<Root>,
}

fn parse(text: &str) -> Result<(Body, Option<String>)> {
    let file: File = toml::from_str(text)?;
    let body = Body {
        schema: file.schema,
        roots: file.roots,
    };
    validate_body(&body)?;
    Ok((body, file.sig))
}

fn validate_body(body: &Body) -> Result<()> {
    ensure!(
        body.schema == "newt.build-toolchains/v1",
        "unsupported toolchain schema"
    );
    for (index, root) in body.roots.iter().enumerate() {
        absolute(Path::new(&root.path))?;
        ensure!(
            !body.roots[..index].iter().any(|r| r.path == root.path),
            "duplicate toolchain root"
        );
    }
    Ok(())
}

/// Production load verification, also used to check an explicit operator write.
/// Unsigned input is never accepted by a build, including after signing review.
pub fn verify(text: &str, verifier: &impl ApproveVerifier) -> Result<Body> {
    let (body, sig) = parse(text)?;
    let sig = sig.context("missing toolchain signature")?;
    let sig = crate::ocap_policy::hex_decode(&sig).map_err(anyhow::Error::msg)?;
    verifier
        .verify(&body.canonical_form()?, &sig)
        .map_err(anyhow::Error::msg)?;
    Ok(body)
}

/// Encode an explicitly approved body using the existing OCAP signature seam.
/// This pure helper owns no key and is not called by startup or any model tool.
pub fn signed_toml(body: &Body, sign: &impl Fn(&[u8]) -> [u8; 64]) -> Result<String> {
    validate_body(body)?;
    let sig = crate::ocap_store::sign_entry(&body.canonical_form()?, sign);
    Ok(toml::to_string(&File {
        schema: body.schema.clone(),
        sig: Some(sig),
        roots: body.roots.clone(),
    })?)
}

/// Read-only host review for the operator signing ceremony. Reuse every live
/// admission check; this does not install policy or mint session authority.
/// A signing command has no model session writes; each later build checks its
/// complete write scope independently against the signed roots.
pub fn review_for_signing(config_dir: &Path, home: Option<PathBuf>, text: &str) -> Result<Body> {
    let (body, _) = parse(text)?;
    let policy = Captured {
        body,
        source: config_dir.join("ocap/toolchains.toml"),
        source_id: RawContentId::from_content(text.as_bytes()),
        key: config_dir.join("identity.pem"),
        home: home
            .map(|path| newt_policy::paths::resolve_uncreated_path(&path))
            .transpose()?,
    };
    admit_captured(&policy, &Scope::none())?;
    Ok(policy.body)
}

#[derive(Debug)]
struct Captured {
    body: Body,
    source: PathBuf,
    source_id: RawContentId,
    key: PathBuf,
    #[cfg_attr(
        not(all(unix, feature = "confine")),
        expect(
            dead_code,
            reason = "unsupported targets refuse admission before reading home"
        )
    )]
    home: Option<PathBuf>,
}
static POLICY: OnceLock<Result<Option<Captured>, String>> = OnceLock::new();

/// Called once by the host, after operator `--config-dir` and before sessions.
/// `key` loads the existing identity read-only; it is never called for a missing
/// policy. Repeated initialization, reload, `--config` and child env cannot
/// replace the snapshot. Embedders must call this at their startup boundary.
pub fn initialize(
    config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    key: impl FnOnce(&Path) -> Result<[u8; 32], String>,
) {
    POLICY.get_or_init(|| capture(config_dir, home, key).map_err(|e| format!("{e:#}")));
}

fn capture(
    config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    key: impl FnOnce(&Path) -> Result<[u8; 32], String>,
) -> Result<Option<Captured>> {
    let Some(dir) = config_dir else {
        return Ok(None);
    };
    let source = dir.join("ocap/toolchains.toml");
    match std::fs::symlink_metadata(&source) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        metadata => {
            metadata?;
        }
    }
    let key_path = dir.join("identity.pem");
    trusted_source(&source)?;
    trusted_source(&key_path)?;
    let verifier = Ed25519ApproveVerifier {
        verifying_key: key(&key_path).map_err(anyhow::Error::msg)?,
    };
    let text = std::fs::read_to_string(&source)?;
    let body = verify(&text, &verifier)?;
    let source_id = RawContentId::from_content(text.as_bytes());
    let home = home
        .map(|path| newt_policy::paths::resolve_uncreated_path(&path))
        .transpose()?;
    Ok(Some(Captured {
        body,
        source,
        source_id,
        key: key_path,
        home,
    }))
}

fn snapshot() -> Result<Option<&'static Captured>> {
    match POLICY.get() {
        None | Some(Ok(None)) => Ok(None),
        Some(Ok(Some(policy))) => Ok(Some(policy)),
        Some(Err(error)) => bail!("{error}"),
    }
}

/// Ceilings see only signed entries. Admission still verifies each live tree
/// and all session writes before either build executor may consume the request.
pub(crate) fn configured_roots() -> Vec<String> {
    snapshot()
        .ok()
        .flatten()
        .map(|p| p.body.roots.iter().map(|r| r.path.clone()).collect())
        .unwrap_or_default()
}
pub(crate) fn requires_admission() -> bool {
    !matches!(snapshot(), Ok(None))
}

/// Host-owned evidence, separate from child stdout and Bridle's axis strengths.
#[derive(Clone, Debug, Serialize)]
pub struct Admission {
    pub policy: ContentId,
    pub root: String,
    pub bin: String,
    pub expected_tree: Option<ContentId>,
    pub observed_tree: Option<ContentId>,
    pub identity_check: &'static str,
    pub access: &'static str,
    pub root_binding: &'static str,
}

pub(crate) fn report(admissions: &[Admission]) -> Option<String> {
    (!admissions.is_empty()).then(|| {
        format!(
            "Build toolchains (configured, host verified): {}",
            serde_json::to_string(admissions).expect("serializable admission")
        )
    })
}

pub(crate) fn admit(writes: &Scope<String>) -> Result<Vec<(Admission, HeldReadRoot)>> {
    let Some(policy) = snapshot()? else {
        return Ok(Vec::new());
    };
    admit_captured(policy, writes)
}

fn admit_captured(
    policy: &Captured,
    writes: &Scope<String>,
) -> Result<Vec<(Admission, HeldReadRoot)>> {
    // Git deliberately exempts ambient writes. Toolchains must not inherit it.
    let Scope::Only(writes) = writes else {
        bail!("toolchains require bounded model writes")
    };
    let mut bound = Vec::new();
    for write in writes {
        let path = Path::new(write);
        absolute(path)?;
        bound.push(path.to_path_buf());
        bound.push(newt_policy::paths::resolve_uncreated_path(path)?);
    }
    for path in [&policy.source, &policy.key] {
        check_disjoint(path, &bound)?;
        trusted_source(path)?;
    }
    ensure!(
        RawContentId::from_content(&std::fs::read(&policy.source)?) == policy.source_id,
        "toolchain policy changed since startup; restart after operator review"
    );
    let ctx = crate::git_staging::TrustContext::bind(&Scope::only(
        bound.iter().map(|p| p.to_string_lossy().into_owned()),
    ))
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    admit_with(&policy.body, |root| admit_root(root, policy, &bound, &ctx))
}

// Collect every validation before installing any authority: no partial activation.
fn admit_with<T>(body: &Body, mut check: impl FnMut(&Root) -> Result<T>) -> Result<Vec<T>> {
    body.roots
        .iter()
        .map(|r| check(r).with_context(|| format!("toolchain {} refused before spawn", r.path)))
        .collect()
}

fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "toolchains require canonical path spelling"
    );
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|p| matches!(p, Component::ParentDir | Component::CurDir)),
        "toolchains require absolute paths without traversal"
    );
    Ok(())
}
fn check_disjoint(path: &Path, other: &[PathBuf]) -> Result<()> {
    ensure!(
        !other
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path)),
        "toolchain overlaps model writes or protected state"
    );
    Ok(())
}
#[cfg(all(unix, any(test, feature = "confine")))]
fn check_location(path: &Path, canonical: &Path, protected: &[PathBuf]) -> Result<()> {
    absolute(path)?;
    ensure!(
        path == canonical,
        "toolchain path must be canonical (no symlinks)"
    );
    ensure!(
        path.parent().is_some(),
        "whole filesystem toolchain refused"
    );
    check_disjoint(path, protected)?;
    for hidden in ["/tmp", "/dev"] {
        check_disjoint(path, &[PathBuf::from(hidden)])?;
    }
    Ok(())
}

#[cfg(all(unix, feature = "confine"))]
fn trusted_source(path: &Path) -> Result<()> {
    let ctx = crate::git_staging::TrustContext::bind(&Scope::none())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    trusted_path(path, &ctx)?;
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(meta.is_file(), "policy/key must be a regular file");
    check_member(&meta)
}
#[cfg(not(all(unix, feature = "confine")))]
fn trusted_source(_: &Path) -> Result<()> {
    bail!("toolchain ownership/held-root checks unavailable on this platform")
}

#[cfg(all(unix, feature = "confine"))]
fn trusted_path(path: &Path, ctx: &crate::git_staging::TrustContext) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    absolute(path)?;
    ensure!(
        std::fs::canonicalize(path)? == path,
        "toolchain path must be canonical"
    );
    crate::git_staging::trust_check(path, ctx).map_err(|e| anyhow::anyhow!("{e}"))?;
    for ancestor in path.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "toolchain symlink ancestor refused"
        );
        check_owner(meta.uid(), host_uid())?;
    }
    Ok(())
}
#[cfg(any(test, all(unix, feature = "confine")))]
fn check_owner(owner: u32, host: u32) -> Result<()> {
    ensure!(
        owner == 0 || owner == host,
        "toolchain foreign owner refused"
    );
    Ok(())
}
#[cfg(all(unix, feature = "confine"))]
fn host_uid() -> u32 {
    // SAFETY: geteuid has no preconditions or memory effects; host admission only.
    unsafe { libc::geteuid() }
}
#[cfg(any(test, all(unix, feature = "confine")))]
fn member_facts(
    owner: u32,
    host: u32,
    mode: u32,
    regular: bool,
    directory: bool,
    links: u64,
) -> Result<()> {
    check_owner(owner, host)?;
    ensure!(mode & 0o022 == 0, "toolchain writable member refused");
    ensure!(
        regular || directory,
        "toolchain symlink/special file refused"
    );
    ensure!(!regular || links == 1, "toolchain hard link refused");
    Ok(())
}
#[cfg(all(unix, feature = "confine"))]
fn check_member(meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    member_facts(
        meta.uid(),
        host_uid(),
        meta.mode(),
        meta.is_file(),
        meta.is_dir(),
        meta.nlink(),
    )
}
#[cfg(all(unix, feature = "confine"))]
fn walk(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    check_member(&meta).with_context(|| format!("inspecting {}", path.display()))?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            walk(&entry?.path())?;
        }
    }
    Ok(())
}

#[cfg(all(unix, feature = "confine"))]
fn admit_root(
    root: &Root,
    policy: &Captured,
    writes: &[PathBuf],
    ctx: &crate::git_staging::TrustContext,
) -> Result<(Admission, HeldReadRoot)> {
    let path = Path::new(&root.path);
    let canonical = std::fs::canonicalize(path)?;
    let mut protected = writes.to_vec();
    protected.push(
        policy
            .source
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf(),
    );
    check_location(path, &canonical, &protected)?;
    if let Some(home) = &policy.home {
        ensure!(!home.starts_with(path), "whole home toolchain refused");
    }
    trusted_path(path, ctx)?;
    let held = agent_bridle_fdguard::GrantedRoot::acquire(path)?;
    ensure!(
        std::fs::symlink_metadata(path.join("bin"))?.is_dir(),
        "toolchain bin/ must be a directory"
    );
    walk(path)?;
    let observed_tree = root
        .expected_tree
        .as_ref()
        .map(|_| crate::tool_tree::digest(path).map(|d| d.cid))
        .transpose()?;
    ensure!(
        root.expected_tree == observed_tree,
        "toolchain content identity mismatch"
    );
    // bind rechecks the path/fd identity after traversal. Linux keeps this
    // descriptor through dispatch; Seatbelt remains explicitly path-only.
    let handle = HeldReadRoot::bind(root.path.clone(), held.as_fd().try_clone_to_owned()?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((
        Admission {
            policy: policy.body.content_id()?,
            root: root.path.clone(),
            bin: path.join("bin").to_string_lossy().into_owned(),
            expected_tree: root.expected_tree,
            observed_tree,
            identity_check: if root.expected_tree.is_some() {
                "verified"
            } else {
                "unpinned"
            },
            access: "read + executable runtime; no new writes",
            root_binding: if cfg!(target_os = "linux") {
                "held descriptor"
            } else {
                "path-only; replacement after admission is not isolated"
            },
        },
        handle,
    ))
}
#[cfg(not(all(unix, feature = "confine")))]
fn admit_root(
    _: &Root,
    _: &Captured,
    _: &[PathBuf],
    _: &crate::git_staging::TrustContext,
) -> Result<(Admission, HeldReadRoot)> {
    bail!("toolchain ownership/held-root checks unavailable on this platform")
}

#[cfg(test)]
mod tests;
