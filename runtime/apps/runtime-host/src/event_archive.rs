use crate::retention::RuntimeTerminalTombstone;
use crate::{LOCAL_EVENT_LOG_LINE_MAX_BYTES, LocalRuntimeError, durable_file};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const EVENT_ARCHIVE_INDEX_SCHEMA_VERSION: u32 = 1;
const EVENT_ARCHIVE_ENTRY_SCHEMA_VERSION: u32 = 1;
const EVENT_ARCHIVE_INDEX_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventArchiveEntry {
    schema_version: u32,
    pub(crate) run_id: Uuid,
    terminal_event_id: Uuid,
    terminal_sequence: u64,
    terminal_event_digest: String,
    event_count: u64,
    committed_bytes: u64,
    content_digest: String,
    completed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EventArchiveIndex {
    schema_version: u32,
    entries: BTreeMap<Uuid, EventArchiveEntry>,
    digest: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EventArchiveStats {
    pub(crate) entries: usize,
    pub(crate) committed_bytes: u64,
    pub(crate) evicted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventArchiveLookupError {
    Corrupt,
    StorageUnavailable,
}

enum ArchiveFileValidationError {
    Corrupt,
    Io(std::io::Error),
}

impl Default for EventArchiveIndex {
    fn default() -> Self {
        let mut index = Self {
            schema_version: EVENT_ARCHIVE_INDEX_SCHEMA_VERSION,
            entries: BTreeMap::new(),
            digest: String::new(),
        };
        index.digest = index.calculate_digest();
        index
    }
}

impl EventArchiveIndex {
    fn calculate_digest(&self) -> String {
        let material = serde_json::json!({
            "schema_version": self.schema_version,
            "entries": self.entries,
        });
        hex::encode(Sha256::digest(
            serde_json::to_vec(&material).expect("Event archive index is serializable"),
        ))
    }

    fn validate(&self) -> Result<(), LocalRuntimeError> {
        if self.schema_version != EVENT_ARCHIVE_INDEX_SCHEMA_VERSION
            || !is_sha256(&self.digest)
            || self.digest != self.calculate_digest()
            || self.entries.iter().any(|(run_id, entry)| {
                run_id != &entry.run_id
                    || entry.schema_version != EVENT_ARCHIVE_ENTRY_SCHEMA_VERSION
                    || entry.run_id.is_nil()
                    || entry.terminal_event_id.is_nil()
                    || entry.terminal_sequence == 0
                    || entry.event_count != entry.terminal_sequence
                    || entry.committed_bytes == 0
                    || !is_sha256(&entry.terminal_event_digest)
                    || !is_sha256(&entry.content_digest)
            })
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime Event archive index is invalid".into(),
            ));
        }
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn archive_root(state_root: &Path) -> PathBuf {
    state_root.join("retention").join("event-archives")
}

fn archive_index_path(state_root: &Path) -> PathBuf {
    archive_root(state_root).join("index.json")
}

fn archive_objects_root(state_root: &Path) -> PathBuf {
    archive_root(state_root).join("objects")
}

fn archive_object_path(state_root: &Path, digest: &str) -> PathBuf {
    archive_objects_root(state_root).join(format!("{digest}.jsonl"))
}

fn hot_event_path(state_root: &Path, run_id: Uuid) -> PathBuf {
    state_root
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl")
}

fn inspect_committed_prefix(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<(u64, String)>, LocalRuntimeError> {
    let file = std::fs::File::open(path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut committed_bytes = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let mut line = Vec::new();
        let mut bounded = reader
            .by_ref()
            .take((LOCAL_EVENT_LOG_LINE_MAX_BYTES + 1) as u64);
        let read = bounded
            .read_until(b'\n', &mut line)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if read == 0 {
            break;
        }
        if line.len() > LOCAL_EVENT_LOG_LINE_MAX_BYTES {
            return Err(LocalRuntimeError::StateRoot(
                "terminal Event history contains an oversized row".into(),
            ));
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        committed_bytes = committed_bytes
            .checked_add(u64::try_from(line.len()).map_err(|_| {
                LocalRuntimeError::StateRoot("Event archive length exceeds u64".into())
            })?)
            .ok_or_else(|| LocalRuntimeError::StateRoot("Event archive length overflow".into()))?;
        if committed_bytes > max_bytes {
            return Ok(None);
        }
        digest.update(&line);
    }
    if committed_bytes == 0 {
        return Err(LocalRuntimeError::StateRoot(
            "terminal Run has no committed Event archive content".into(),
        ));
    }
    Ok(Some((committed_bytes, hex::encode(digest.finalize()))))
}

pub(crate) fn inspect_event_archive(
    state_root: &Path,
    tombstone: &RuntimeTerminalTombstone,
    max_bytes_per_run: u64,
) -> Result<Option<EventArchiveEntry>, LocalRuntimeError> {
    if max_bytes_per_run == 0 {
        return Ok(None);
    }
    let Some((committed_bytes, content_digest)) = inspect_committed_prefix(
        &hot_event_path(state_root, tombstone.run_id),
        max_bytes_per_run,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(EventArchiveEntry {
        schema_version: EVENT_ARCHIVE_ENTRY_SCHEMA_VERSION,
        run_id: tombstone.run_id,
        terminal_event_id: tombstone.terminal_event_id,
        terminal_sequence: tombstone.terminal_sequence,
        terminal_event_digest: tombstone.terminal_event_digest.clone(),
        event_count: tombstone.terminal_sequence,
        committed_bytes,
        content_digest,
        completed_at: tombstone.completed_at,
    }))
}

fn load_index(state_root: &Path) -> Result<EventArchiveIndex, LocalRuntimeError> {
    let path = archive_index_path(state_root);
    let body = match read_bounded_file(&path, EVENT_ARCHIVE_INDEX_MAX_BYTES) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EventArchiveIndex::default());
        }
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let index: EventArchiveIndex = serde_json::from_slice(&body)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    index.validate()?;
    Ok(index)
}

fn persist_index(
    state_root: &Path,
    index: &mut EventArchiveIndex,
) -> Result<(), LocalRuntimeError> {
    index.digest = index.calculate_digest();
    index.validate()?;
    let body = serde_json::to_vec_pretty(index)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    if u64::try_from(body.len())
        .ok()
        .is_none_or(|length| length > EVENT_ARCHIVE_INDEX_MAX_BYTES)
    {
        return Err(LocalRuntimeError::StateRoot(
            "Runtime Event archive index exceeds its hard size limit".into(),
        ));
    }
    durable_file::replace(&archive_index_path(state_root), &body)?;
    secure_file(&archive_index_path(state_root))
}

fn read_bounded_file(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds its hard size limit",
        ));
    }
    let mut body = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut body)?;
    if u64::try_from(body.len())
        .ok()
        .is_none_or(|length| length > max_bytes)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds its hard size limit",
        ));
    }
    Ok(body)
}

fn secure_directory(path: &Path) -> Result<(), LocalRuntimeError> {
    std::fs::create_dir_all(path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<(), LocalRuntimeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    }
    Ok(())
}

fn validate_archive_file(
    mut file: std::fs::File,
    entry: &EventArchiveEntry,
) -> Result<std::fs::File, ArchiveFileValidationError> {
    let length = file
        .metadata()
        .map_err(ArchiveFileValidationError::Io)?
        .len();
    if length != entry.committed_bytes {
        return Err(ArchiveFileValidationError::Corrupt);
    }
    let mut buffer = [0_u8; 64 * 1024];
    let mut digest = Sha256::new();
    let mut last_byte = None;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(ArchiveFileValidationError::Io)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        last_byte = Some(buffer[read - 1]);
    }
    if last_byte != Some(b'\n') || hex::encode(digest.finalize()) != entry.content_digest {
        return Err(ArchiveFileValidationError::Corrupt);
    }
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(ArchiveFileValidationError::Io)?;
    Ok(file)
}

fn archive_validation_state_error(
    error: ArchiveFileValidationError,
    corrupt_message: &'static str,
) -> LocalRuntimeError {
    match error {
        ArchiveFileValidationError::Corrupt => LocalRuntimeError::StateRoot(corrupt_message.into()),
        ArchiveFileValidationError::Io(error) => LocalRuntimeError::StateRoot(error.to_string()),
    }
}

fn write_archive_object(
    state_root: &Path,
    entry: &EventArchiveEntry,
    path: &Path,
) -> Result<(), LocalRuntimeError> {
    let source_path = hot_event_path(state_root, entry.run_id);
    let mut source = std::fs::File::open(&source_path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let staging = path.with_extension("json.partial");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut destination = options
        .open(&staging)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let copied = std::io::copy(
        &mut std::io::Read::by_ref(&mut source).take(entry.committed_bytes),
        &mut destination,
    )
    .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    if copied != entry.committed_bytes {
        return Err(LocalRuntimeError::StateRoot(
            "terminal Event history was truncated before archive commit".into(),
        ));
    }
    destination
        .flush()
        .and_then(|()| destination.sync_all())
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    drop(destination);

    let inspected = inspect_committed_prefix(&source_path, entry.committed_bytes)?;
    if inspected != Some((entry.committed_bytes, entry.content_digest.clone())) {
        return Err(LocalRuntimeError::StateRoot(
            "terminal Event history changed before archive commit".into(),
        ));
    }
    durable_file::rename(&staging, path)
}

fn materialize_entry(
    state_root: &Path,
    entry: &EventArchiveEntry,
) -> Result<(), LocalRuntimeError> {
    secure_directory(&archive_root(state_root))?;
    secure_directory(&archive_objects_root(state_root))?;
    let path = archive_object_path(state_root, &entry.content_digest);
    match std::fs::File::open(&path) {
        Ok(file) => {
            validate_archive_file(file, entry).map_err(|error| {
                archive_validation_state_error(
                    error,
                    "content-addressed Event archive conflicts with existing bytes",
                )
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_archive_object(state_root, entry, &path)?;
        }
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    }
    secure_file(&path)?;
    let readback = std::fs::File::open(&path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    validate_archive_file(readback, entry).map_err(|error| {
        archive_validation_state_error(error, "Event archive readback verification failed")
    })?;
    Ok(())
}

fn prune_to_capacity(
    index: &mut EventArchiveIndex,
    previously_committed: &BTreeSet<Uuid>,
    max_entries: usize,
    max_bytes: u64,
) -> usize {
    let mut evicted = 0usize;
    loop {
        let bytes = index.entries.values().fold(0_u64, |total, entry| {
            total.saturating_add(entry.committed_bytes)
        });
        if index.entries.len() <= max_entries && bytes <= max_bytes {
            break;
        }
        let Some(oldest) = index
            .entries
            .values()
            .min_by_key(|entry| (entry.completed_at, entry.run_id))
            .map(|entry| entry.run_id)
        else {
            break;
        };
        index.entries.remove(&oldest);
        if previously_committed.contains(&oldest) {
            evicted = evicted.saturating_add(1);
        }
    }
    evicted
}

fn cleanup_unreferenced_objects(
    state_root: &Path,
    index: &EventArchiveIndex,
) -> Result<(), LocalRuntimeError> {
    let root = archive_objects_root(state_root);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let referenced = index
        .entries
        .values()
        .map(|entry| entry.content_digest.as_str())
        .collect::<BTreeSet<_>>();
    for entry in entries {
        let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
            .is_file()
        {
            return Err(LocalRuntimeError::StateRoot(
                "Event archive object directory contains an unexpected entry".into(),
            ));
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            LocalRuntimeError::StateRoot("Event archive object filename is invalid".into())
        })?;
        let (digest, staging) = if let Some(digest) = name.strip_suffix(".jsonl") {
            (digest, false)
        } else if let Some(digest) = name.strip_suffix(".json.partial") {
            (digest, true)
        } else {
            return Err(LocalRuntimeError::StateRoot(
                "Event archive object filename is invalid".into(),
            ));
        };
        if !is_sha256(digest) {
            return Err(LocalRuntimeError::StateRoot(
                "Event archive object digest filename is invalid".into(),
            ));
        }
        if staging || !referenced.contains(digest) {
            std::fs::remove_file(entry.path())
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
    }
    #[cfg(unix)]
    std::fs::File::open(&root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    Ok(())
}

pub(crate) fn reconcile_event_archives(
    state_root: &Path,
    candidates: &[EventArchiveEntry],
    max_entries: usize,
    max_bytes: u64,
) -> Result<EventArchiveStats, LocalRuntimeError> {
    let index_path_exists = archive_index_path(state_root).exists();
    let mut index = load_index(state_root)?;
    let previously_committed = index.entries.keys().copied().collect::<BTreeSet<_>>();
    for candidate in candidates {
        if let Some(existing) = index.entries.get(&candidate.run_id)
            && existing != candidate
        {
            return Err(LocalRuntimeError::StateRoot(
                "Run Event archive identity conflicts with existing cold history".into(),
            ));
        }
        index.entries.insert(candidate.run_id, candidate.clone());
    }
    let evicted = prune_to_capacity(&mut index, &previously_committed, max_entries, max_bytes);
    for candidate in candidates {
        if index.entries.get(&candidate.run_id) == Some(candidate) {
            materialize_entry(state_root, candidate)?;
        }
    }
    if index_path_exists || !index.entries.is_empty() || !candidates.is_empty() || evicted > 0 {
        secure_directory(&archive_root(state_root))?;
        persist_index(state_root, &mut index)?;
        cleanup_unreferenced_objects(state_root, &index)?;
    }
    Ok(EventArchiveStats {
        entries: index.entries.len(),
        committed_bytes: index.entries.values().fold(0_u64, |total, entry| {
            total.saturating_add(entry.committed_bytes)
        }),
        evicted,
    })
}

pub(crate) fn event_archive_stats(
    state_root: &Path,
) -> Result<EventArchiveStats, LocalRuntimeError> {
    let index = load_index(state_root)?;
    Ok(EventArchiveStats {
        entries: index.entries.len(),
        committed_bytes: index.entries.values().fold(0_u64, |total, entry| {
            total.saturating_add(entry.committed_bytes)
        }),
        evicted: 0,
    })
}

pub(crate) fn open_event_archive(
    state_root: &Path,
    tombstone: &RuntimeTerminalTombstone,
) -> Result<Option<std::fs::File>, EventArchiveLookupError> {
    let path = archive_index_path(state_root);
    let body = match read_bounded_file(&path, EVENT_ARCHIVE_INDEX_MAX_BYTES) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(EventArchiveLookupError::Corrupt);
        }
        Err(_) => return Err(EventArchiveLookupError::StorageUnavailable),
    };
    let index: EventArchiveIndex =
        serde_json::from_slice(&body).map_err(|_| EventArchiveLookupError::Corrupt)?;
    index
        .validate()
        .map_err(|_| EventArchiveLookupError::Corrupt)?;
    let Some(entry) = index.entries.get(&tombstone.run_id) else {
        return Ok(None);
    };
    if entry.run_id != tombstone.run_id
        || entry.terminal_event_id != tombstone.terminal_event_id
        || entry.terminal_sequence != tombstone.terminal_sequence
        || entry.terminal_event_digest != tombstone.terminal_event_digest
        || entry.completed_at != tombstone.completed_at
    {
        return Err(EventArchiveLookupError::Corrupt);
    }
    let path = archive_object_path(state_root, &entry.content_digest);
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EventArchiveLookupError::Corrupt);
        }
        Err(_) => return Err(EventArchiveLookupError::StorageUnavailable),
    };
    let file = validate_archive_file(file, entry).map_err(|error| match error {
        ArchiveFileValidationError::Corrupt => EventArchiveLookupError::Corrupt,
        ArchiveFileValidationError::Io(_) => EventArchiveLookupError::StorageUnavailable,
    })?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_file_reader_rejects_content_larger_than_its_hard_limit() {
        let root = tempfile::tempdir().expect("temporary archive root");
        let path = root.path().join("index.json");
        std::fs::write(&path, b"12345").expect("oversized fixture");

        let error = read_bounded_file(&path, 4).expect_err("oversized file must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            read_bounded_file(&path, 5).expect("exact hard limit"),
            b"12345"
        );
    }
}
