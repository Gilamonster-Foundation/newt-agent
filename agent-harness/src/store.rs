//! Verified content-addressed storage. The disk backend adds durability to NodeStore.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use content_addressable::{
    AddressedBytes, ContentAddressable, ContentId, NodeStore, NodeStoreExt, RawContentId,
    StoreError, StoreOperation,
};
use serde::de::DeserializeOwned;

/// Default per-object bound, independent of a turn's retrieval byte budget.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// An execution lease, held until the session drops (including unwinding).
/// Never unlink its sidecar: independent opens must lock the same inode.
pub(crate) struct RunWriter {
    _file: Option<std::fs::File>,
    directory: Option<PathBuf>,
    run: ContentId,
    process: u32,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationFailure {
    BeforeReplace,
    AfterReplace,
}

/// One storage interface with memory and durable backends.
pub struct FrameStore {
    directory: Option<PathBuf>,
    nodes: BTreeMap<ContentId, Vec<u8>>,
    sources: BTreeMap<RawContentId, Vec<u8>>,
    max_record_bytes: usize,
    #[cfg(test)]
    pub(crate) publication_failure: Option<PublicationFailure>,
}

impl FrameStore {
    pub fn memory() -> Self {
        Self {
            directory: None,
            nodes: BTreeMap::new(),
            sources: BTreeMap::new(),
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            #[cfg(test)]
            publication_failure: None,
        }
    }

    pub fn memory_with_max_bytes(max_record_bytes: usize) -> crate::Result<Self> {
        if max_record_bytes == 0 {
            return Err(crate::Error::Budget(
                "record bytes limit must be positive".into(),
            ));
        }
        Ok(Self {
            max_record_bytes,
            ..Self::memory()
        })
    }

    pub fn open(directory: impl AsRef<Path>) -> crate::Result<Self> {
        Self::open_with_max_bytes(directory, DEFAULT_MAX_RECORD_BYTES)
    }

    pub fn open_with_max_bytes(
        directory: impl AsRef<Path>,
        max_record_bytes: usize,
    ) -> crate::Result<Self> {
        let memory = Self::memory_with_max_bytes(max_record_bytes)?;
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|e| crate::Error::Storage(e.to_string()))?;
        let directory = directory
            .canonicalize()
            .map_err(|e| crate::Error::Storage(e.to_string()))?;
        Ok(Self {
            directory: Some(directory),
            ..memory
        })
    }

    pub fn max_record_bytes(&self) -> usize {
        self.max_record_bytes
    }

    pub fn directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    pub fn put<T: ContentAddressable>(&mut self, value: &T) -> crate::Result<ContentId> {
        NodeStoreExt::put_node(self, value).map_err(storage_error)
    }

    pub fn get<T: DeserializeOwned + ContentAddressable>(
        &self,
        id: &ContentId,
    ) -> crate::Result<T> {
        NodeStoreExt::get_node(self, id).map_err(storage_error)
    }

    pub fn put_source(&mut self, bytes: &[u8]) -> crate::Result<RawContentId> {
        check_size(bytes.len(), self.max_record_bytes)
            .map_err(|e| crate::Error::Budget(e.to_string()))?;
        let id = RawContentId::from_content(bytes);
        if let Some(dir) = &self.directory {
            write_once(&dir.join(id.to_string()), bytes, self.max_record_bytes)?;
        } else if let Some(existing) = self.sources.get(&id) {
            if existing != bytes {
                return Err(crate::Error::Integrity(format!("source collision: {id}")));
            }
        } else {
            self.sources.insert(id, bytes.to_vec());
        }
        Ok(id)
    }

    /// Opaque bytes are checked on every read, including after reopening the store.
    pub fn source(&self, id: &RawContentId) -> crate::Result<Vec<u8>> {
        let bytes = match &self.directory {
            Some(dir) => read_bounded(&dir.join(id.to_string()), self.max_record_bytes)
                .map_err(|e| crate::Error::Storage(e.to_string()))?,
            None => self
                .sources
                .get(id)
                .cloned()
                .ok_or_else(|| crate::Error::Storage(format!("source absent: {id}")))?,
        };
        if RawContentId::from_content(&bytes) != *id {
            return Err(crate::Error::Integrity(format!("source substituted: {id}")));
        }
        Ok(bytes)
    }

    /// Inspect size before allocating or fetching a source. This is only a
    /// budget check; source() still verifies the actual bytes before use.
    pub fn source_len(&self, id: &RawContentId) -> crate::Result<usize> {
        let len = match &self.directory {
            Some(dir) => usize::try_from(
                std::fs::metadata(dir.join(id.to_string()))
                    .map_err(|e| crate::Error::Storage(e.to_string()))?
                    .len(),
            )
            .map_err(|e| crate::Error::Budget(e.to_string())),
            None => self
                .sources
                .get(id)
                .map(Vec::len)
                .ok_or_else(|| crate::Error::Storage(format!("source absent: {id}"))),
        }?;
        check_size(len, self.max_record_bytes).map_err(|e| crate::Error::Budget(e.to_string()))?;
        Ok(len)
    }

    /// Units retain their existing identity over Derivation, rather than their
    /// lifecycle-bearing JSON presentation. Both consumers use this admission.
    pub fn unit(&self, id: ContentId) -> crate::Result<agent_frame::Unit> {
        let invalid = |e: &dyn std::fmt::Display| crate::Error::Integrity(e.to_string());
        let derivation: agent_frame::Derivation = self.get(&id)?;
        let source = self.source(&derivation.source)?;
        let root: agent_frame::RootEvent = self.get(&derivation.root)?;
        self.source(&root.content)?;
        let unit =
            agent_frame::Unit::seal(derivation.op, &source, derivation.span, derivation.root)
                .map_err(|e| invalid(&e))?;
        if unit.id().map_err(|e| invalid(&e))?.into_content_id() != id {
            return Err(crate::Error::Integrity("unit source substitution".into()));
        }
        agent_frame::verify_unit(&unit, &source).map_err(|e| invalid(&e))?;
        Ok(unit)
    }

    pub(crate) fn acquire_writer(
        &self,
        run: ContentId,
        expected: Option<ContentId>,
    ) -> crate::Result<RunWriter> {
        let file = if let Some(dir) = &self.directory {
            let locks = dir.join("locks");
            std::fs::create_dir_all(&locks).map_err(|e| crate::Error::Storage(e.to_string()))?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(locks.join(run.to_string()))
                .map_err(|e| crate::Error::Storage(e.to_string()))?;
            fs4::FileExt::try_lock(&file).map_err(|error| match error {
                fs4::TryLockError::WouldBlock => crate::Error::Conflict(format!(
                    "run {run} already has a writer; release its session before resuming"
                )),
                error => crate::Error::Storage(format!("cannot lock run {run}: {error}")),
            })?;
            Some(file)
        } else {
            None
        };
        let writer = RunWriter {
            _file: file,
            directory: self.directory.clone(),
            run,
            process: std::process::id(),
        };
        self.check_writer(&writer, expected)?;
        Ok(writer)
    }

    pub(crate) fn check_writer(
        &self,
        writer: &RunWriter,
        expected: Option<ContentId>,
    ) -> crate::Result<()> {
        if writer.process != std::process::id() || writer.directory != self.directory {
            return Err(crate::Error::Conflict(
                "session belongs to another process or store; open a fresh session".into(),
            ));
        }
        let Some(dir) = &self.directory else {
            return Ok(());
        };
        let path = dir.join("heads").join(writer.run.to_string());
        let current = match read_bounded(&path, 256) {
            Ok(bytes) => Some(
                std::str::from_utf8(&bytes)
                    .map_err(|e| crate::Error::Integrity(e.to_string()))?
                    .trim()
                    .parse::<ContentId>()
                    .map_err(|e| crate::Error::Integrity(e.to_string()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(crate::Error::Storage(error.to_string())),
        };
        if current != expected {
            return Err(crate::Error::Conflict(format!(
                "run {} expected head {}, current head {}; resume the current locator or create a new run explicitly",
                writer.run,
                expected.map_or_else(|| "<absent>".into(), |id| id.to_string()),
                current.map_or_else(|| "<absent>".into(), |id| id.to_string()),
            )));
        }
        Ok(())
    }

    /// Only the execution owner can select a new current checkpoint. The
    /// predecessor check also rejects stale restoration after a writer exits.
    pub(crate) fn publish_head(
        &self,
        writer: &RunWriter,
        expected: Option<ContentId>,
        head: ContentId,
    ) -> crate::Result<()> {
        self.check_writer(writer, expected)?;
        let Some(dir) = &self.directory else {
            return Ok(());
        };
        let publish = || -> std::io::Result<()> {
            let heads = dir.join("heads");
            std::fs::create_dir_all(&heads)?;
            // Persist the heads directory's name on its first publication too.
            #[cfg(unix)]
            std::fs::File::open(dir)?.sync_all()?;
            let mut file = tempfile::NamedTempFile::new_in(&heads)?;
            writeln!(file, "{head}")?;
            file.as_file().sync_all()?;
            #[cfg(test)]
            self.fail_publication(PublicationFailure::BeforeReplace)?;
            file.persist(heads.join(writer.run.to_string()))
                .map_err(|e| e.error)?;
            #[cfg(test)]
            self.fail_publication(PublicationFailure::AfterReplace)?;
            #[cfg(unix)]
            std::fs::File::open(&heads)?.sync_all()?;
            Ok(())
        };
        publish().map_err(|e| crate::Error::Storage(e.to_string()))
    }

    #[cfg(test)]
    fn fail_publication(&self, stage: PublicationFailure) -> std::io::Result<()> {
        if self.publication_failure == Some(stage) {
            return Err(std::io::Error::other(
                "injected checkpoint publication failure",
            ));
        }
        Ok(())
    }
}

impl NodeStore for FrameStore {
    fn get_unverified(&self, id: &ContentId) -> std::result::Result<Vec<u8>, StoreError> {
        match &self.directory {
            Some(dir) => read_bounded(&dir.join(format!("{id}.cbor")), self.max_record_bytes)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        StoreError::NotFound(*id)
                    } else {
                        StoreError::Backend {
                            operation: StoreOperation::Get,
                            source: Box::new(e),
                        }
                    }
                }),
            None => self.nodes.get(id).cloned().ok_or(StoreError::NotFound(*id)),
        }
    }

    fn insert(&mut self, item: AddressedBytes<'_>) -> std::result::Result<(), StoreError> {
        check_size(item.bytes().len(), self.max_record_bytes).map_err(|e| StoreError::Backend {
            operation: StoreOperation::Insert,
            source: Box::new(e),
        })?;
        if let Some(dir) = &self.directory {
            write_once(
                &dir.join(format!("{}.cbor", item.id())),
                item.bytes(),
                self.max_record_bytes,
            )
            .map_err(|e| StoreError::Backend {
                operation: StoreOperation::Insert,
                source: Box::new(e),
            })?;
        } else if let Some(existing) = self.nodes.get(&item.id()) {
            if existing != item.bytes() {
                return Err(StoreError::Collision { id: item.id() });
            }
        } else {
            self.nodes.insert(item.id(), item.bytes().to_vec());
        }
        Ok(())
    }
}

fn storage_error(error: StoreError) -> crate::Error {
    let detail = match error {
        StoreError::Backend { operation, source } => format!("{operation:?}: {source}"),
        other => other.to_string(),
    };
    crate::Error::Storage(detail)
}

/// Persist the object before any journal head can name it. A competing writer
/// may publish identical bytes, but may never rebind an occupied address.
fn write_once(path: &Path, bytes: &[u8], max_record_bytes: usize) -> crate::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::Storage("store has no parent".into()))?;
    let persist = || -> std::io::Result<()> {
        if path.exists() {
            return if read_bounded(path, max_record_bytes)? == bytes {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "occupied content address contains different bytes",
                ))
            };
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(path) {
            Ok(_) => (),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_bounded(path, max_record_bytes)? != bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "content address collision",
                    ));
                }
            }
            Err(error) => return Err(error.error),
        }
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    persist().map_err(|e| crate::Error::Storage(format!("{}: {e}", path.display())))
}

fn check_size(bytes: usize, max: usize) -> std::io::Result<()> {
    if bytes > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("record bytes exceed limit {max}"),
        ));
    }
    Ok(())
}

pub(crate) fn read_bounded(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "record is not a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "record is not a regular file",
        ));
    }
    let len = usize::try_from(metadata.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "record bytes exceed platform size",
        )
    })?;
    check_size(len, max)?;
    let mut bytes = Vec::with_capacity(len);
    file.take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    check_size(bytes.len(), max)?;
    Ok(bytes)
}
