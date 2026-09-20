use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

/// An explicit read-only review request in the canonical plan. This is neither
/// the subtask's write lane nor a permission grant; omission preserves work mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSubject {
    /// Existing staged, unstaged and untracked work, captured without staging.
    ExistingDiff,
    /// Explicit supplied artifacts, resolved under the original read authority.
    Artifacts { paths: Vec<String> },
}

impl<'de> Deserialize<'de> for ReviewSubject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            ExistingDiff {},
            Artifacts { paths: Vec<String> },
        }
        match Wire::deserialize(deserializer)? {
            Wire::ExistingDiff {} => Ok(Self::ExistingDiff),
            Wire::Artifacts { paths } => {
                if paths.is_empty() || paths.iter().any(|path| path.trim().is_empty()) {
                    return Err(serde::de::Error::custom(
                        "review artifacts require nonempty paths",
                    ));
                }
                Ok(Self::Artifacts { paths })
            }
        }
    }
}

/// Actual objective and existing external-turn context, not a generated review ID.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewObjective {
    pub instruction: String,
    pub turn_context: String,
}

impl ContentAddressable for ReviewObjective {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// Why the host could not present a complete authorized subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum CaptureFailure {
    #[error("review subject access denied")]
    Denied,
    #[error("review subject capture incomplete: {0}")]
    Incomplete(String),
    #[error("review subject exceeds capture limit")]
    OverLimit,
    #[error("review subject is not supported text")]
    Binary,
}

/// Actual text-file bytes and executable disposition. Absence lives in the
/// containing map, so an empty present file is never confused with deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub bytes: Vec<u8>,
    pub executable: bool,
}

impl From<Vec<u8>> for ReviewFile {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            executable: false,
        }
    }
}

/// Complete raw versions supplied by an authorized capture owner. Enumeration
/// failures must return CaptureFailure instead of manufacturing an empty map.
#[derive(Debug, Clone, Default)]
pub struct ReviewVersions {
    pub baseline: std::collections::BTreeMap<String, ReviewFile>,
    pub current: std::collections::BTreeMap<String, ReviewFile>,
    /// ExistingDiff includes actual index state; None is for non-Git subjects.
    pub index: Option<std::collections::BTreeMap<String, ReviewFile>>,
}

#[derive(Debug, Clone, Serialize)]
struct FileVersion {
    content: RawContentId,
    text: String,
    executable: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AddressedVersions {
    baseline: std::collections::BTreeMap<String, FileVersion>,
    current: std::collections::BTreeMap<String, FileVersion>,
    index: Option<std::collections::BTreeMap<String, FileVersion>>,
}

/// Declared generated/dependency exclusions and actual tracked-source exceptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationScope {
    pub excluded_directory_names: Vec<String>,
    pub baseline_excluded_roots: std::collections::BTreeSet<String>,
    pub current_excluded_roots: std::collections::BTreeSet<String>,
    pub baseline_tracked_overrides: std::collections::BTreeSet<String>,
    pub current_tracked_overrides: std::collections::BTreeSet<String>,
}

/// The actual declared coverage, copied into existing phase evidence as well
/// as model material; this carries no authority or independently minted ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCoverage {
    pub explicit_subject: Option<ReviewSubject>,
    pub implementation_scope: Option<ImplementationScope>,
}

/// An addressed structural subject: paths and baseline/index/current membership
/// cannot alias by injecting display delimiters. Text rendering is derived from
/// these versions, never an independent authority for what was captured.
#[derive(Debug, Clone, Serialize)]
pub struct PresentedSubject {
    objective: ContentId,
    coverage: ReviewCoverage,
    versions: AddressedVersions,
}

impl PresentedSubject {
    /// Build from complete actual versions; empty/no-diff subjects are valid.
    ///
    /// # Errors
    /// Rejects over-limit/binary material and incomplete ExistingDiff index
    /// capture. A present empty file remains distinct from an absent path.
    pub fn new(
        objective: ContentId,
        scope: Option<ReviewSubject>,
        versions: ReviewVersions,
        max_bytes: usize,
    ) -> Result<Self, CaptureFailure> {
        if matches!(scope, Some(ReviewSubject::ExistingDiff)) && versions.index.is_none() {
            return Err(CaptureFailure::Incomplete(
                "existing diff index was not captured".into(),
            ));
        }
        let mut remaining = max_bytes;
        let mut convert = |files: std::collections::BTreeMap<String, ReviewFile>| {
            files
                .into_iter()
                .map(|(path, version)| {
                    let bytes = version.bytes;
                    remaining = remaining
                        .checked_sub(path.len())
                        .and_then(|rest| rest.checked_sub(bytes.len()))
                        .ok_or(CaptureFailure::OverLimit)?;
                    let content = RawContentId::from_content(&bytes);
                    let text = String::from_utf8(bytes).map_err(|_| CaptureFailure::Binary)?;
                    if text.contains('\0') {
                        return Err(CaptureFailure::Binary);
                    }
                    Ok((
                        path,
                        FileVersion {
                            content,
                            text,
                            executable: version.executable,
                        },
                    ))
                })
                .collect::<Result<std::collections::BTreeMap<_, _>, CaptureFailure>>()
        };
        let baseline = convert(versions.baseline)?;
        let current = convert(versions.current)?;
        let index = versions.index.map(convert).transpose()?;
        let subject = Self {
            objective,
            coverage: ReviewCoverage {
                explicit_subject: scope,
                implementation_scope: None,
            },
            versions: AddressedVersions {
                baseline,
                current,
                index,
            },
        };
        // Raw bytes are not a presentation bound: escaping, addresses and
        // structure also consume the actual model input. Host admission must
        // additionally fit the complete request into its existing token budget.
        let rendered = subject.material().map_err(|error| {
            CaptureFailure::Incomplete(format!("review presentation encoding failed: {error}"))
        })?;
        if rendered.len() > max_bytes {
            return Err(CaptureFailure::OverLimit);
        }
        Ok(subject)
    }

    pub(crate) fn with_implementation_scope(
        mut self,
        scope: ImplementationScope,
        max_bytes: usize,
    ) -> Result<Self, CaptureFailure> {
        self.coverage.implementation_scope = Some(scope);
        let material = self
            .material()
            .map_err(|error| CaptureFailure::Incomplete(error.to_string()))?;
        if material.len() > max_bytes {
            return Err(CaptureFailure::OverLimit);
        }
        Ok(self)
    }

    #[must_use]
    pub fn coverage(&self) -> &ReviewCoverage {
        &self.coverage
    }

    #[must_use]
    pub fn objective(&self) -> ContentId {
        self.objective
    }

    /// JSON is presentation only; structured paths/versions own identity.
    ///
    /// # Errors
    /// Propagates a serialization failure without inventing empty material.
    pub fn material(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Material<'a> {
            #[serde(flatten)]
            versions: &'a AddressedVersions,
            #[serde(flatten)]
            coverage: &'a ReviewCoverage,
        }
        serde_json::to_string_pretty(&Material {
            versions: &self.versions,
            coverage: &self.coverage,
        })
    }
}

impl ContentAddressable for PresentedSubject {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
