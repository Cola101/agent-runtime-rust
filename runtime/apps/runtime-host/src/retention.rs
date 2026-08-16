use crate::embedded::{
    RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
    RuntimeControlReceipt, RuntimeControlReceiptState,
};
use crate::{LocalRunRecord, LocalRunState, LocalRuntimeError, LocalRuntimeHost};
use agent_protocol::{RunStatus, RuntimeInvocationContext};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub const RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION: u32 = 1;
const RUNTIME_TERMINAL_LEDGER_MANIFEST_SCHEMA_VERSION: u32 = 2;
const RUNTIME_TERMINAL_LEDGER_SEGMENT_SCHEMA_VERSION: u32 = 1;
const RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT: usize = 256;

/// Bounded local-state policy for one canonical Workspace state root. Exact
/// tombstones are retained until an external archival authority is added; if
/// their configured capacity is exhausted the Runtime fails closed instead of
/// forgetting replay evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRetentionPolicy {
    pub max_run_directories_per_workspace: usize,
    pub max_run_directories_per_tenant: usize,
    pub retain_terminal_runs_per_workspace: usize,
    pub min_terminal_age: Duration,
    pub max_run_tombstones_per_workspace: usize,
    pub max_run_tombstones_per_tenant: usize,
    pub max_control_tombstones_per_workspace: usize,
    pub max_control_tombstones_per_tenant: usize,
}

impl Default for RuntimeRetentionPolicy {
    fn default() -> Self {
        Self {
            max_run_directories_per_workspace: 10_000,
            max_run_directories_per_tenant: 100_000,
            retain_terminal_runs_per_workspace: 512,
            min_terminal_age: Duration::from_secs(30 * 24 * 60 * 60),
            max_run_tombstones_per_workspace: 10_000,
            max_run_tombstones_per_tenant: 100_000,
            max_control_tombstones_per_workspace: 40_000,
            max_control_tombstones_per_tenant: 400_000,
        }
    }
}

impl RuntimeRetentionPolicy {
    pub(crate) fn validate(self) -> Result<Self, LocalRuntimeError> {
        if self.max_run_directories_per_workspace == 0
            || self.max_run_directories_per_tenant < self.max_run_directories_per_workspace
            || self.retain_terminal_runs_per_workspace >= self.max_run_directories_per_workspace
            || self.max_run_tombstones_per_workspace == 0
            || self.max_run_tombstones_per_tenant < self.max_run_tombstones_per_workspace
            || self.max_control_tombstones_per_workspace == 0
            || self.max_control_tombstones_per_tenant < self.max_control_tombstones_per_workspace
            || self.min_terminal_age > Duration::from_secs(365 * 24 * 60 * 60)
        {
            return Err(LocalRuntimeError::Configuration(
                "Runtime retention limits are invalid".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTerminalTombstone {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub run_binding_digest: String,
    pub owner_epoch: u64,
    pub status: RunStatus,
    pub terminal_event_id: Uuid,
    pub terminal_sequence: u64,
    pub terminal_event_digest: String,
    pub completed_at: DateTime<Utc>,
    pub artifacts_removed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRetentionReport {
    pub run_directories_before: usize,
    pub run_directories_after: usize,
    pub terminal_records_before: usize,
    pub unmanaged_run_directories: usize,
    pub strongly_referenced_runs: usize,
    pub tombstoned_runs: usize,
    pub tombstoned_control_commands: usize,
    pub repaired_tombstones: usize,
    pub total_run_tombstones: usize,
    pub total_control_tombstones: usize,
    pub terminal_ledger_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetiredControlCommand {
    pub command_id: Uuid,
    pub command_digest: String,
    pub run_id: Uuid,
    pub applied_owner_epoch: u64,
    pub run_status: RunStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTerminalLedger {
    schema_version: u32,
    run_tombstones: BTreeMap<Uuid, RuntimeTerminalTombstone>,
    control_tombstones: BTreeMap<Uuid, RetiredControlCommand>,
    digest: String,
}

/// Schema 2 keeps old tombstones in immutable, digest-bound segments and only
/// rewrites a bounded active segment. The manifest is the sole authority: files
/// not referenced by it are crash leftovers and are never merged implicitly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTerminalLedgerManifest {
    schema_version: u32,
    active_segment_id: u64,
    sealed_segments: Vec<RuntimeTerminalLedgerSegmentDescriptor>,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTerminalLedgerSegmentDescriptor {
    segment_id: u64,
    run_tombstones: usize,
    control_tombstones: usize,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTerminalLedgerSegment {
    schema_version: u32,
    segment_id: u64,
    sealed: bool,
    run_tombstones: BTreeMap<Uuid, RuntimeTerminalTombstone>,
    control_tombstones: BTreeMap<Uuid, RetiredControlCommand>,
    digest: String,
}

struct RuntimeTerminalLedgerStore {
    manifest: RuntimeTerminalLedgerManifest,
    active: RuntimeTerminalLedgerSegment,
    aggregate: RuntimeTerminalLedger,
}

impl Default for RuntimeTerminalLedger {
    fn default() -> Self {
        let mut ledger = Self {
            schema_version: RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION,
            run_tombstones: BTreeMap::new(),
            control_tombstones: BTreeMap::new(),
            digest: String::new(),
        };
        ledger.digest = ledger.calculate_digest();
        ledger
    }
}

impl RuntimeTerminalLedger {
    fn calculate_digest(&self) -> String {
        let material = serde_json::json!({
            "schema_version": self.schema_version,
            "run_tombstones": self.run_tombstones,
            "control_tombstones": self.control_tombstones,
        });
        hex::encode(Sha256::digest(
            serde_json::to_vec(&material).expect("terminal ledger material is serializable"),
        ))
    }

    fn validate(&self) -> Result<(), LocalRuntimeError> {
        if self.schema_version != RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION
            || !is_sha256(&self.digest)
            || self.digest != self.calculate_digest()
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal ledger digest or schema is invalid".into(),
            ));
        }
        validate_ledger_entries(&self.run_tombstones, &self.control_tombstones)
    }
}

fn validate_ledger_entries(
    run_tombstones: &BTreeMap<Uuid, RuntimeTerminalTombstone>,
    control_tombstones: &BTreeMap<Uuid, RetiredControlCommand>,
) -> Result<(), LocalRuntimeError> {
    for (run_id, tombstone) in run_tombstones {
        tombstone.invocation.validate().map_err(|error| {
            LocalRuntimeError::StateRoot(format!(
                "Runtime terminal tombstone invocation is invalid: {error}"
            ))
        })?;
        if tombstone.schema_version != RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION
            || run_id != &tombstone.run_id
            || run_id.is_nil()
            || tombstone.owner_epoch == 0
            || tombstone.terminal_event_id.is_nil()
            || tombstone.terminal_sequence == 0
            || !is_sha256(&tombstone.run_binding_digest)
            || !is_sha256(&tombstone.terminal_event_digest)
            || !is_collectable_status(tombstone.status)
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal tombstone is invalid".into(),
            ));
        }
    }
    for (command_id, tombstone) in control_tombstones {
        if command_id != &tombstone.command_id
            || command_id.is_nil()
            || tombstone.run_id.is_nil()
            || tombstone.applied_owner_epoch == 0
            || !is_sha256(&tombstone.command_digest)
            || !is_collectable_status(tombstone.run_status)
            || !run_tombstones.contains_key(&tombstone.run_id)
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime control tombstone is invalid".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct RuntimeRetentionCandidate {
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    tombstone: RuntimeTerminalTombstone,
    pub(crate) control_tombstones: Vec<RetiredControlCommand>,
    receipt_paths: Vec<PathBuf>,
}

pub(crate) struct RuntimeRetentionScan {
    pub run_directories: usize,
    pub terminal_records: usize,
    pub unmanaged_run_directories: usize,
    pub strongly_referenced_runs: usize,
    pub candidates: Vec<RuntimeRetentionCandidate>,
    ledger: RuntimeTerminalLedger,
}

fn retention_root(state_root: &Path) -> PathBuf {
    state_root.join("retention")
}

fn legacy_ledger_path(state_root: &Path) -> PathBuf {
    retention_root(state_root).join("terminal-ledger.json")
}

fn ledger_manifest_path(state_root: &Path) -> PathBuf {
    retention_root(state_root).join("terminal-ledger-manifest.json")
}

fn sealed_segment_path(state_root: &Path, segment_id: u64) -> PathBuf {
    retention_root(state_root)
        .join("segments")
        .join(format!("segment-{segment_id:020}.json"))
}

fn active_segment_path(state_root: &Path, segment_id: u64) -> PathBuf {
    retention_root(state_root).join(format!("active-{segment_id:020}.json"))
}

fn durable_replace(path: &Path, body: &[u8]) -> Result<(), LocalRuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| LocalRuntimeError::StateRoot("retention path has no parent".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let staging = path.with_extension("json.partial");
    use std::io::Write as _;
    let mut file = std::fs::File::create(&staging)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    file.write_all(body)
        .and_then(|()| file.sync_all())
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    std::fs::rename(&staging, path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), LocalRuntimeError> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    Ok(())
}

impl RuntimeTerminalLedgerManifest {
    fn new(active_segment_id: u64) -> Self {
        let mut manifest = Self {
            schema_version: RUNTIME_TERMINAL_LEDGER_MANIFEST_SCHEMA_VERSION,
            active_segment_id,
            sealed_segments: Vec::new(),
            digest: String::new(),
        };
        manifest.digest = manifest.calculate_digest();
        manifest
    }

    fn calculate_digest(&self) -> String {
        let material = serde_json::json!({
            "schema_version": self.schema_version,
            "active_segment_id": self.active_segment_id,
            "sealed_segments": self.sealed_segments,
        });
        hex::encode(Sha256::digest(
            serde_json::to_vec(&material).expect("terminal ledger manifest is serializable"),
        ))
    }

    fn validate(&self) -> Result<(), LocalRuntimeError> {
        let ordered = self
            .sealed_segments
            .iter()
            .enumerate()
            .all(|(index, segment)| {
                segment.segment_id == u64::try_from(index).unwrap_or(u64::MAX)
                    && segment.segment_id < self.active_segment_id
                    && segment.run_tombstones > 0
                    && segment.run_tombstones <= RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT
                    && is_sha256(&segment.digest)
            });
        if self.schema_version != RUNTIME_TERMINAL_LEDGER_MANIFEST_SCHEMA_VERSION
            || self.active_segment_id
                != u64::try_from(self.sealed_segments.len()).unwrap_or(u64::MAX)
            || !ordered
            || !is_sha256(&self.digest)
            || self.digest != self.calculate_digest()
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal ledger manifest is invalid".into(),
            ));
        }
        Ok(())
    }
}

impl RuntimeTerminalLedgerSegment {
    fn empty(segment_id: u64) -> Self {
        let mut segment = Self {
            schema_version: RUNTIME_TERMINAL_LEDGER_SEGMENT_SCHEMA_VERSION,
            segment_id,
            sealed: false,
            run_tombstones: BTreeMap::new(),
            control_tombstones: BTreeMap::new(),
            digest: String::new(),
        };
        segment.digest = segment.calculate_digest();
        segment
    }

    fn calculate_digest(&self) -> String {
        let material = serde_json::json!({
            "schema_version": self.schema_version,
            "segment_id": self.segment_id,
            "sealed": self.sealed,
            "run_tombstones": self.run_tombstones,
            "control_tombstones": self.control_tombstones,
        });
        hex::encode(Sha256::digest(
            serde_json::to_vec(&material).expect("terminal ledger segment is serializable"),
        ))
    }

    fn validate(&self) -> Result<(), LocalRuntimeError> {
        validate_ledger_entries(&self.run_tombstones, &self.control_tombstones)?;
        if self.schema_version != RUNTIME_TERMINAL_LEDGER_SEGMENT_SCHEMA_VERSION
            || !is_sha256(&self.digest)
            || self.digest != self.calculate_digest()
            || (self.sealed
                && (self.run_tombstones.is_empty()
                    || self.run_tombstones.len() > RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT
                    || self
                        .run_tombstones
                        .values()
                        .any(|tombstone| !tombstone.artifacts_removed)))
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal ledger segment is invalid".into(),
            ));
        }
        Ok(())
    }
}

fn write_manifest(
    state_root: &Path,
    manifest: &mut RuntimeTerminalLedgerManifest,
) -> Result<(), LocalRuntimeError> {
    manifest.digest = manifest.calculate_digest();
    manifest.validate()?;
    let body = serde_json::to_vec_pretty(manifest)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    durable_replace(&ledger_manifest_path(state_root), &body)
}

fn write_segment(
    state_root: &Path,
    segment: &mut RuntimeTerminalLedgerSegment,
) -> Result<(), LocalRuntimeError> {
    segment.digest = segment.calculate_digest();
    segment.validate()?;
    let path = if segment.sealed {
        sealed_segment_path(state_root, segment.segment_id)
    } else {
        active_segment_path(state_root, segment.segment_id)
    };
    let body = serde_json::to_vec_pretty(segment)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    durable_replace(&path, &body)
}

fn merge_segment(
    aggregate: &mut RuntimeTerminalLedger,
    segment: &RuntimeTerminalLedgerSegment,
) -> Result<(), LocalRuntimeError> {
    for (run_id, tombstone) in &segment.run_tombstones {
        if aggregate
            .run_tombstones
            .insert(*run_id, tombstone.clone())
            .is_some()
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal Run appears in multiple ledger segments".into(),
            ));
        }
    }
    for (command_id, command) in &segment.control_tombstones {
        if aggregate
            .control_tombstones
            .insert(*command_id, command.clone())
            .is_some()
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime control command appears in multiple ledger segments".into(),
            ));
        }
    }
    Ok(())
}

fn read_segment(
    path: &Path,
    segment_id: u64,
    sealed: bool,
) -> Result<RuntimeTerminalLedgerSegment, LocalRuntimeError> {
    let body =
        std::fs::read(path).map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let segment: RuntimeTerminalLedgerSegment = serde_json::from_slice(&body)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    segment.validate()?;
    if segment.segment_id != segment_id || segment.sealed != sealed {
        return Err(LocalRuntimeError::StateRoot(
            "Runtime terminal ledger segment identity is invalid".into(),
        ));
    }
    Ok(segment)
}

fn migrate_legacy_ledger(
    state_root: &Path,
    mut legacy: RuntimeTerminalLedger,
) -> Result<RuntimeTerminalLedgerStore, LocalRuntimeError> {
    legacy.validate()?;
    let mut remaining_runs = legacy.run_tombstones.clone();
    let mut remaining_controls = legacy.control_tombstones.clone();
    let mut sealed_segments = Vec::new();
    let mut next_segment_id = 0_u64;
    loop {
        let run_ids = remaining_runs
            .iter()
            .take(RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT)
            .map(|(run_id, tombstone)| (*run_id, tombstone.artifacts_removed))
            .collect::<Vec<_>>();
        if run_ids.len() < RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT
            || run_ids.iter().any(|(_, removed)| !removed)
        {
            break;
        }
        let ids = run_ids
            .into_iter()
            .map(|(run_id, _)| run_id)
            .collect::<BTreeSet<_>>();
        let mut segment = RuntimeTerminalLedgerSegment::empty(next_segment_id);
        segment.sealed = true;
        for run_id in &ids {
            segment.run_tombstones.insert(
                *run_id,
                remaining_runs
                    .remove(run_id)
                    .expect("selected legacy tombstone remains"),
            );
        }
        let command_ids = remaining_controls
            .iter()
            .filter(|(_, command)| ids.contains(&command.run_id))
            .map(|(command_id, _)| *command_id)
            .collect::<Vec<_>>();
        for command_id in command_ids {
            segment.control_tombstones.insert(
                command_id,
                remaining_controls
                    .remove(&command_id)
                    .expect("selected legacy control remains"),
            );
        }
        write_segment(state_root, &mut segment)?;
        sealed_segments.push(RuntimeTerminalLedgerSegmentDescriptor {
            segment_id: segment.segment_id,
            run_tombstones: segment.run_tombstones.len(),
            control_tombstones: segment.control_tombstones.len(),
            digest: segment.digest.clone(),
        });
        next_segment_id = next_segment_id.saturating_add(1);
    }

    let mut active = RuntimeTerminalLedgerSegment::empty(next_segment_id);
    active.run_tombstones = remaining_runs;
    active.control_tombstones = remaining_controls;
    write_segment(state_root, &mut active)?;
    let mut manifest = RuntimeTerminalLedgerManifest::new(next_segment_id);
    manifest.sealed_segments = sealed_segments;
    write_manifest(state_root, &mut manifest)?;

    for path in [
        legacy_ledger_path(state_root),
        legacy_ledger_path(state_root).with_extension("json.partial"),
    ] {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(LocalRuntimeError::StateRoot(error.to_string()));
        }
    }
    sync_directory(&retention_root(state_root))?;
    legacy.digest = legacy.calculate_digest();
    Ok(RuntimeTerminalLedgerStore {
        manifest,
        active,
        aggregate: legacy,
    })
}

fn load_ledger_store(state_root: &Path) -> Result<RuntimeTerminalLedgerStore, LocalRuntimeError> {
    let manifest_path = ledger_manifest_path(state_root);
    let manifest_body = match std::fs::read(&manifest_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy_path = legacy_ledger_path(state_root);
            let legacy = match std::fs::read(&legacy_path) {
                Ok(body) => serde_json::from_slice(&body)
                    .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    RuntimeTerminalLedger::default()
                }
                Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
            };
            return migrate_legacy_ledger(state_root, legacy);
        }
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let manifest: RuntimeTerminalLedgerManifest = serde_json::from_slice(&manifest_body)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    manifest.validate()?;
    let mut aggregate = RuntimeTerminalLedger::default();
    for descriptor in &manifest.sealed_segments {
        let segment = read_segment(
            &sealed_segment_path(state_root, descriptor.segment_id),
            descriptor.segment_id,
            true,
        )?;
        if descriptor.run_tombstones != segment.run_tombstones.len()
            || descriptor.control_tombstones != segment.control_tombstones.len()
            || descriptor.digest != segment.digest
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime terminal ledger descriptor does not match its segment".into(),
            ));
        }
        merge_segment(&mut aggregate, &segment)?;
    }
    let active = read_segment(
        &active_segment_path(state_root, manifest.active_segment_id),
        manifest.active_segment_id,
        false,
    )?;
    merge_segment(&mut aggregate, &active)?;
    // Every segment was independently digest-validated and `merge_segment`
    // rejected duplicate identities, so repeating every entry validation on
    // the aggregate would make a read scale twice with retained history.
    aggregate.digest = aggregate.calculate_digest();
    Ok(RuntimeTerminalLedgerStore {
        manifest,
        active,
        aggregate,
    })
}

fn load_manifest_and_active(
    state_root: &Path,
) -> Result<(RuntimeTerminalLedgerManifest, RuntimeTerminalLedgerSegment), LocalRuntimeError> {
    if !ledger_manifest_path(state_root).is_file() {
        let _ = load_ledger_store(state_root)?;
    }
    let body = std::fs::read(ledger_manifest_path(state_root))
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let manifest: RuntimeTerminalLedgerManifest = serde_json::from_slice(&body)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    manifest.validate()?;
    let active = read_segment(
        &active_segment_path(state_root, manifest.active_segment_id),
        manifest.active_segment_id,
        false,
    )?;
    Ok((manifest, active))
}

fn load_ledger(state_root: &Path) -> Result<RuntimeTerminalLedger, LocalRuntimeError> {
    Ok(load_ledger_store(state_root)?.aggregate)
}

fn validate_capacity(
    policy: RuntimeRetentionPolicy,
    ledger: &RuntimeTerminalLedger,
) -> Result<(), LocalRuntimeError> {
    if ledger.run_tombstones.len() > policy.max_run_tombstones_per_workspace
        || ledger.control_tombstones.len() > policy.max_control_tombstones_per_workspace
    {
        return Err(LocalRuntimeError::StateRoot(
            "Runtime terminal tombstone capacity is exhausted".into(),
        ));
    }
    Ok(())
}

fn persist_active_segment(
    state_root: &Path,
    policy: RuntimeRetentionPolicy,
    store: &mut RuntimeTerminalLedgerStore,
) -> Result<(), LocalRuntimeError> {
    validate_capacity(policy, &store.aggregate)?;
    // The aggregate was assembled from digest-validated segments and every
    // mutation is mirrored into the active segment. Re-hashing and validating
    // the complete history here would make each bounded append O(total
    // retained history), recreating the write-amplification problem that the
    // segmented ledger is intended to remove. `write_segment` validates the
    // only mutable authority before committing it.
    write_segment(state_root, &mut store.active)
}

fn seal_full_active_segments(
    state_root: &Path,
    store: &mut RuntimeTerminalLedgerStore,
) -> Result<(), LocalRuntimeError> {
    while store.active.run_tombstones.len() >= RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT
        && store
            .active
            .run_tombstones
            .values()
            .take(RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT)
            .all(|tombstone| tombstone.artifacts_removed)
    {
        let old_active_path = active_segment_path(state_root, store.active.segment_id);
        let run_ids = store
            .active
            .run_tombstones
            .keys()
            .take(RUNTIME_TERMINAL_LEDGER_SEGMENT_RUN_LIMIT)
            .copied()
            .collect::<BTreeSet<_>>();
        let mut sealed = RuntimeTerminalLedgerSegment::empty(store.active.segment_id);
        sealed.sealed = true;
        for run_id in &run_ids {
            sealed.run_tombstones.insert(
                *run_id,
                store
                    .active
                    .run_tombstones
                    .remove(run_id)
                    .expect("selected active tombstone remains"),
            );
        }
        let command_ids = store
            .active
            .control_tombstones
            .iter()
            .filter(|(_, command)| run_ids.contains(&command.run_id))
            .map(|(command_id, _)| *command_id)
            .collect::<Vec<_>>();
        for command_id in command_ids {
            sealed.control_tombstones.insert(
                command_id,
                store
                    .active
                    .control_tombstones
                    .remove(&command_id)
                    .expect("selected active control remains"),
            );
        }
        write_segment(state_root, &mut sealed)?;

        let next_id = store.active.segment_id.checked_add(1).ok_or_else(|| {
            LocalRuntimeError::StateRoot("terminal ledger segment id exhausted".into())
        })?;
        store.active.segment_id = next_id;
        write_segment(state_root, &mut store.active)?;
        store
            .manifest
            .sealed_segments
            .push(RuntimeTerminalLedgerSegmentDescriptor {
                segment_id: sealed.segment_id,
                run_tombstones: sealed.run_tombstones.len(),
                control_tombstones: sealed.control_tombstones.len(),
                digest: sealed.digest,
            });
        store.manifest.active_segment_id = next_id;
        write_manifest(state_root, &mut store.manifest)?;
        if let Err(error) = std::fs::remove_file(old_active_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(LocalRuntimeError::StateRoot(error.to_string()));
        }
        sync_directory(&retention_root(state_root))?;
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_collectable_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled | RunStatus::TimedOut
    )
}

fn terminal_status(state: &LocalRunState) -> Option<RunStatus> {
    match state {
        LocalRunState::Finished { status } => match status.as_str() {
            "succeeded" => Some(RunStatus::Succeeded),
            "cancelled" => Some(RunStatus::Cancelled),
            "timed_out" => Some(RunStatus::TimedOut),
            "indeterminate" => Some(RunStatus::Indeterminate),
            _ => Some(RunStatus::Failed),
        },
        LocalRunState::Cancelled { .. } => Some(RunStatus::Cancelled),
        _ => None,
    }
}

fn event_status(event_type: &str) -> Option<RunStatus> {
    match event_type {
        "run.succeeded" => Some(RunStatus::Succeeded),
        "run.failed" => Some(RunStatus::Failed),
        "run.cancelled" => Some(RunStatus::Cancelled),
        "run.timed_out" => Some(RunStatus::TimedOut),
        "run.indeterminate" => Some(RunStatus::Indeterminate),
        _ => None,
    }
}

fn record_invocation(record: &LocalRunRecord) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: record.tenant_id,
        application_id: record.application_id,
        workload_identity_id: record.workload_identity_id,
        workspace_id: record.workspace_id,
        agent_version_id: record.agent_version_id,
        model_policy_id: record.model_policy_id,
    }
}

pub(crate) fn run_binding_digest(
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
    input: &str,
) -> String {
    let material = serde_json::json!({
        "schema_version": 1,
        "invocation": invocation,
        "run_id": run_id,
        "input": input,
    });
    hex::encode(Sha256::digest(
        serde_json::to_vec(&material).expect("Run binding material is serializable"),
    ))
}

fn validated_terminal_event(
    state_root: &Path,
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
    expected_status: RunStatus,
) -> Result<Option<crate::LocalEvent>, LocalRuntimeError> {
    let events = LocalRuntimeHost::replay_events(state_root, run_id, 0)?;
    if events.is_empty() {
        return Ok(None);
    }
    for (index, event) in events.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| LocalRuntimeError::StateRoot("event sequence is exhausted".into()))?;
        let payload_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&event.payload)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?,
        ));
        if event.sequence != expected_sequence
            || event.run_id != run_id
            || event.event_id.is_nil()
            || event.attempt_id.is_nil()
            || event.schema_version != 1
            || event.tenant_id != invocation.tenant_id
            || event.application_id != invocation.application_id
            || event.workload_identity_id != invocation.workload_identity_id
            || event.workspace_id != invocation.workspace_id
            || event.agent_version_id != invocation.agent_version_id
            || event.model_policy_id != invocation.model_policy_id
            || event.digest != payload_digest
        {
            return Ok(None);
        }
    }
    let terminal = events.last().expect("non-empty event log");
    if event_status(&terminal.event_type) != Some(expected_status) {
        return Ok(None);
    }
    Ok(Some(terminal.clone()))
}

fn load_receipts_by_run(
    state_root: &Path,
) -> Result<HashMap<Uuid, Vec<(RuntimeControlReceipt, PathBuf)>>, LocalRuntimeError> {
    let directory = state_root.join("control-receipts");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let mut receipts = HashMap::<Uuid, Vec<(RuntimeControlReceipt, PathBuf)>>::new();
    for entry in entries {
        let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let command_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                LocalRuntimeError::StateRoot("Runtime control receipt filename is invalid".into())
            })?;
        let body = std::fs::read(&path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let receipt: RuntimeControlReceipt = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let command = receipt.command();
        let command_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&command)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?,
        ));
        if receipt.schema_version != RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION
            || command.schema_version != RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION
            || receipt.command_id != command_id
            || receipt.command_digest != command_digest
            || !is_sha256(&receipt.command_digest)
            || receipt.run_id.is_nil()
            || receipt.expected_owner_epoch == 0
            || receipt.applied_owner_epoch == 0
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime control receipt is invalid".into(),
            ));
        }
        receipts
            .entry(receipt.run_id)
            .or_default()
            .push((receipt, path));
    }
    Ok(receipts)
}

pub(crate) fn scan_retention_candidates(
    state_root: &Path,
    policy: RuntimeRetentionPolicy,
    now: DateTime<Utc>,
) -> Result<RuntimeRetentionScan, LocalRuntimeError> {
    let ledger = load_ledger(state_root)?;
    let strong_references = LocalRuntimeHost::retention_strong_run_references(state_root)?;
    let receipts = load_receipts_by_run(state_root)?;
    let entries = match std::fs::read_dir(state_root.join("runs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RuntimeRetentionScan {
                run_directories: 0,
                terminal_records: 0,
                unmanaged_run_directories: 0,
                strongly_referenced_runs: 0,
                candidates: Vec::new(),
                ledger,
            });
        }
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let mut run_directories = 0usize;
    let mut terminal_records = 0usize;
    let mut unmanaged_run_directories = 0usize;
    let mut strongly_referenced_runs = 0usize;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        run_directories = run_directories.saturating_add(1);
        let Some(run_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            unmanaged_run_directories = unmanaged_run_directories.saturating_add(1);
            continue;
        };
        let Some(record) = LocalRuntimeHost::read_run_record(state_root, run_id)? else {
            unmanaged_run_directories = unmanaged_run_directories.saturating_add(1);
            continue;
        };
        let Some(status) = terminal_status(&record.state) else {
            continue;
        };
        terminal_records = terminal_records.saturating_add(1);
        if strong_references.contains(&run_id) {
            strongly_referenced_runs = strongly_referenced_runs.saturating_add(1);
            continue;
        }
        if !is_collectable_status(status) || ledger.run_tombstones.contains_key(&run_id) {
            continue;
        }
        let invocation = record_invocation(&record);
        if invocation.validate().is_err() {
            continue;
        }
        let Some(terminal) = validated_terminal_event(state_root, invocation, run_id, status)?
        else {
            continue;
        };
        let age = now.signed_duration_since(terminal.timestamp).to_std().ok();
        if age.is_none_or(|age| age < policy.min_terminal_age) {
            continue;
        }
        let run_receipts = receipts.get(&run_id).cloned().unwrap_or_default();
        if run_receipts.iter().any(|(receipt, _)| {
            receipt.invocation != invocation
                || receipt.state != RuntimeControlReceiptState::Completed
                || receipt.run_status != Some(status)
        }) {
            continue;
        }
        let control_tombstones = run_receipts
            .iter()
            .map(|(receipt, _)| RetiredControlCommand {
                command_id: receipt.command_id,
                command_digest: receipt.command_digest.clone(),
                run_id,
                applied_owner_epoch: receipt.applied_owner_epoch,
                run_status: status,
            })
            .collect();
        let receipt_paths = run_receipts.into_iter().map(|(_, path)| path).collect();
        candidates.push(RuntimeRetentionCandidate {
            invocation,
            run_id,
            tombstone: RuntimeTerminalTombstone {
                schema_version: RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION,
                invocation,
                run_id,
                run_binding_digest: run_binding_digest(invocation, run_id, &record.input),
                owner_epoch: record.owner_epoch,
                status,
                terminal_event_id: terminal.event_id,
                terminal_sequence: terminal.sequence,
                terminal_event_digest: terminal.digest,
                completed_at: terminal.timestamp,
                artifacts_removed: false,
            },
            control_tombstones,
            receipt_paths,
        });
    }
    candidates.sort_by_key(|candidate| (candidate.tombstone.completed_at, candidate.run_id));
    Ok(RuntimeRetentionScan {
        run_directories,
        terminal_records,
        unmanaged_run_directories,
        strongly_referenced_runs,
        candidates,
        ledger,
    })
}

pub(crate) fn available_tombstone_capacity(
    scan: &RuntimeRetentionScan,
    policy: RuntimeRetentionPolicy,
) -> (usize, usize) {
    (
        policy
            .max_run_tombstones_per_workspace
            .saturating_sub(scan.ledger.run_tombstones.len()),
        policy
            .max_control_tombstones_per_workspace
            .saturating_sub(scan.ledger.control_tombstones.len()),
    )
}

pub(crate) fn repair_committed_tombstones(
    state_root: &Path,
    _policy: RuntimeRetentionPolicy,
) -> Result<usize, LocalRuntimeError> {
    let (manifest, active) = load_manifest_and_active(state_root)?;
    let mut aggregate = RuntimeTerminalLedger {
        schema_version: RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION,
        run_tombstones: active.run_tombstones.clone(),
        control_tombstones: active.control_tombstones.clone(),
        digest: String::new(),
    };
    aggregate.digest = aggregate.calculate_digest();
    let mut store = RuntimeTerminalLedgerStore {
        manifest,
        active,
        aggregate,
    };
    let pending = store
        .active
        .run_tombstones
        .values()
        .filter(|tombstone| !tombstone.artifacts_removed)
        .map(|tombstone| tombstone.run_id)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(0);
    }
    for run_id in &pending {
        remove_run_artifacts(state_root, *run_id, &store.aggregate)?;
        store
            .aggregate
            .run_tombstones
            .get_mut(run_id)
            .expect("pending tombstone remains in the ledger")
            .artifacts_removed = true;
        store
            .active
            .run_tombstones
            .get_mut(run_id)
            .ok_or_else(|| {
                LocalRuntimeError::StateRoot(
                    "unremoved tombstone was found outside the active ledger segment".into(),
                )
            })?
            .artifacts_removed = true;
    }
    // No capacity can grow during repair; sealed descriptors already account
    // for the historical portion, so only the bounded active segment is read.
    write_segment(state_root, &mut store.active)?;
    seal_full_active_segments(state_root, &mut store)?;
    Ok(pending.len())
}

fn remove_run_artifacts(
    state_root: &Path,
    run_id: Uuid,
    ledger: &RuntimeTerminalLedger,
) -> Result<(), LocalRuntimeError> {
    let run_dir = state_root.join("runs").join(run_id.to_string());
    if let Err(error) = std::fs::remove_dir_all(&run_dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(LocalRuntimeError::StateRoot(error.to_string()));
    }
    let receipt_directory = state_root.join("control-receipts");
    for control in ledger
        .control_tombstones
        .values()
        .filter(|control| control.run_id == run_id)
    {
        let path = receipt_directory.join(format!("{}.json", control.command_id));
        for candidate in [path.clone(), path.with_extension("json.partial")] {
            if let Err(error) = std::fs::remove_file(candidate)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(LocalRuntimeError::StateRoot(error.to_string()));
            }
        }
    }
    let runs = state_root.join("runs");
    if runs.is_dir() {
        sync_directory(&runs)?;
    }
    if receipt_directory.is_dir() {
        sync_directory(&receipt_directory)?;
    }
    Ok(())
}

pub(crate) fn commit_retention_candidates(
    state_root: &Path,
    policy: RuntimeRetentionPolicy,
    candidates: Vec<RuntimeRetentionCandidate>,
) -> Result<(usize, usize), LocalRuntimeError> {
    if candidates.is_empty() {
        return Ok((0, 0));
    }
    let mut store = load_ledger_store(state_root)?;
    let mut control_count = 0usize;
    let mut committed_run_ids = Vec::new();
    for candidate in &candidates {
        if store
            .aggregate
            .run_tombstones
            .contains_key(&candidate.run_id)
        {
            continue;
        }
        store
            .aggregate
            .run_tombstones
            .insert(candidate.run_id, candidate.tombstone.clone());
        store
            .active
            .run_tombstones
            .insert(candidate.run_id, candidate.tombstone.clone());
        committed_run_ids.push(candidate.run_id);
        for control in &candidate.control_tombstones {
            if let Some(existing) = store.aggregate.control_tombstones.get(&control.command_id)
                && existing != control
            {
                return Err(LocalRuntimeError::StateRoot(
                    "control tombstone id conflicts with existing evidence".into(),
                ));
            }
            if store
                .aggregate
                .control_tombstones
                .insert(control.command_id, control.clone())
                .is_none()
            {
                control_count = control_count.saturating_add(1);
            }
            store
                .active
                .control_tombstones
                .insert(control.command_id, control.clone());
        }
    }
    if committed_run_ids.is_empty() {
        return Ok((0, 0));
    }
    // This is the replay-safety commit point. No artifact is deleted before
    // the exact Run and command digests are durable.
    persist_active_segment(state_root, policy, &mut store)?;
    for run_id in &committed_run_ids {
        remove_run_artifacts(state_root, *run_id, &store.aggregate)?;
        store
            .aggregate
            .run_tombstones
            .get_mut(run_id)
            .expect("committed tombstone remains in the ledger")
            .artifacts_removed = true;
        store
            .active
            .run_tombstones
            .get_mut(run_id)
            .expect("new tombstone remains in the active segment")
            .artifacts_removed = true;
    }
    for candidate in &candidates {
        for path in &candidate.receipt_paths {
            debug_assert!(!path.exists());
        }
    }
    // A crash before this second commit merely leaves `artifacts_removed=false`;
    // the next maintenance pass repeats deletion and closes the transaction.
    persist_active_segment(state_root, policy, &mut store)?;
    seal_full_active_segments(state_root, &mut store)?;
    Ok((committed_run_ids.len(), control_count))
}

pub(crate) fn load_run_tombstone_index(
    state_root: &Path,
) -> Result<HashMap<Uuid, RuntimeTerminalTombstone>, LocalRuntimeError> {
    Ok(load_ledger(state_root)?
        .run_tombstones
        .into_iter()
        .collect())
}

pub(crate) fn read_retired_control(
    state_root: &Path,
    command_id: Uuid,
) -> Result<Option<RetiredControlCommand>, LocalRuntimeError> {
    Ok(load_ledger(state_root)?
        .control_tombstones
        .get(&command_id)
        .cloned())
}

pub(crate) fn ledger_counts_and_bytes(
    state_root: &Path,
) -> Result<(usize, usize, u64), LocalRuntimeError> {
    let (manifest, active) = load_manifest_and_active(state_root)?;
    let run_tombstones = manifest
        .sealed_segments
        .iter()
        .fold(active.run_tombstones.len(), |count, segment| {
            count.saturating_add(segment.run_tombstones)
        });
    let control_tombstones = manifest
        .sealed_segments
        .iter()
        .fold(active.control_tombstones.len(), |count, segment| {
            count.saturating_add(segment.control_tombstones)
        });
    let bytes = retention_file_bytes(&retention_root(state_root))?;
    Ok((run_tombstones, control_tombstones, bytes))
}

fn retention_file_bytes(path: &Path) -> Result<u64, LocalRuntimeError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let metadata = entry
            .metadata()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if metadata.is_dir() {
            bytes = bytes.saturating_add(retention_file_bytes(&entry.path())?);
        } else if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

#[cfg(test)]
fn write_ledger(
    state_root: &Path,
    policy: RuntimeRetentionPolicy,
    ledger: &mut RuntimeTerminalLedger,
) -> Result<(), LocalRuntimeError> {
    validate_capacity(policy, ledger)?;
    ledger.digest = ledger.calculate_digest();
    ledger.validate()?;
    let body = serde_json::to_vec_pretty(ledger)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    durable_replace(&legacy_ledger_path(state_root), &body)?;
    let _ = load_ledger_store(state_root)?;
    Ok(())
}

pub(crate) fn count_run_directories(state_root: &Path) -> Result<usize, LocalRuntimeError> {
    let entries = match std::fs::read_dir(state_root.join("runs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    };
    let mut count = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if entry
            .file_type()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
            .is_dir()
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedded::{RuntimeControlAction, RuntimeControlReceipt};
    use crate::{LOCAL_STORE_VERSION, LocalEvent};
    use agent_protocol::EventEnvelope;
    use agent_protocol::{ContentPart, Message, Role, SessionConversationTurn};

    fn invocation() -> RuntimeInvocationContext {
        RuntimeInvocationContext {
            schema_version: 1,
            tenant_id: Uuid::now_v7(),
            application_id: Uuid::now_v7(),
            workload_identity_id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            agent_version_id: Uuid::now_v7(),
            model_policy_id: Uuid::now_v7(),
        }
    }

    fn policy() -> RuntimeRetentionPolicy {
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 8,
            max_run_directories_per_tenant: 16,
            retain_terminal_runs_per_workspace: 2,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 16,
            max_run_tombstones_per_tenant: 32,
            max_control_tombstones_per_workspace: 16,
            max_control_tombstones_per_tenant: 32,
        }
    }

    fn tombstone(invocation: RuntimeInvocationContext, run_id: Uuid) -> RuntimeTerminalTombstone {
        RuntimeTerminalTombstone {
            schema_version: RUNTIME_TERMINAL_LEDGER_SCHEMA_VERSION,
            invocation,
            run_id,
            run_binding_digest: "a".repeat(64),
            owner_epoch: 1,
            status: RunStatus::Succeeded,
            terminal_event_id: Uuid::now_v7(),
            terminal_sequence: 1,
            terminal_event_digest: "b".repeat(64),
            completed_at: Utc::now(),
            artifacts_removed: false,
        }
    }

    fn write_event(
        root: &Path,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        event_type: &str,
    ) {
        let envelope = EventEnvelope::new(
            invocation.tenant_id,
            Uuid::now_v7(),
            run_id,
            1,
            Uuid::now_v7(),
            event_type,
            serde_json::json!({"status": event_type}),
        );
        let event = LocalEvent {
            event_id: envelope.event_id,
            schema_version: envelope.schema_version,
            tenant_id: invocation.tenant_id,
            application_id: invocation.application_id,
            workload_identity_id: invocation.workload_identity_id,
            workspace_id: invocation.workspace_id,
            agent_version_id: invocation.agent_version_id,
            model_policy_id: invocation.model_policy_id,
            session_id: envelope.session_id,
            sequence: envelope.sequence,
            run_id,
            attempt_id: envelope.attempt_id,
            timestamp: envelope.timestamp,
            trace_id: envelope.trace_id,
            event_type: envelope.event_type,
            payload: envelope.payload,
            digest: envelope.digest,
        };
        let directory = root.join("runs").join(run_id.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let mut body = serde_json::to_vec(&event).unwrap();
        body.push(b'\n');
        std::fs::write(directory.join("events.jsonl"), body).unwrap();
    }

    fn record(
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        state: LocalRunState,
    ) -> LocalRunRecord {
        LocalRunRecord {
            store_version: LOCAL_STORE_VERSION,
            tenant_id: invocation.tenant_id,
            application_id: invocation.application_id,
            workload_identity_id: invocation.workload_identity_id,
            workspace_id: invocation.workspace_id,
            agent_version_id: invocation.agent_version_id,
            model_policy_id: invocation.model_policy_id,
            run_id,
            input: "retention-test".into(),
            state,
            owner_epoch: 1,
        }
    }

    fn write_session_record(
        root: &Path,
        session_id: Uuid,
        branch_id: Uuid,
        history: Vec<SessionConversationTurn>,
        active_run_id: Option<Uuid>,
    ) {
        let history_digest = agent_protocol::session_conversation_history_digest(&history);
        let active_turn = active_run_id.map(|run_id| {
            serde_json::json!({
                "run_id": run_id,
                "generation": 1,
                "history_digest": history_digest,
                "input": "retention-test"
            })
        });
        let body = serde_json::json!({
            "store_version": 1,
            "session_id": session_id,
            "branches": {
                branch_id.to_string(): {
                    "branch_id": branch_id,
                    "generation": 1,
                    "history": history,
                    "archived_generations": {},
                    "active_turn": active_turn
                }
            }
        });
        let directory = root.join("sessions").join(session_id.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("session.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_crash_after_the_replay_barrier_is_repaired_without_forgetting_the_run() {
        let root = tempfile::tempdir().unwrap();
        let identity = invocation();
        let run_id = Uuid::now_v7();
        let command_id = Uuid::now_v7();
        let run_dir = root.path().join("runs").join(run_id.to_string());
        let receipt_dir = root.path().join("control-receipts");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(&receipt_dir).unwrap();
        std::fs::write(run_dir.join("checkpoint.json.partial"), b"interrupted").unwrap();
        std::fs::write(receipt_dir.join(format!("{command_id}.json")), b"retired").unwrap();

        let mut ledger = RuntimeTerminalLedger::default();
        ledger
            .run_tombstones
            .insert(run_id, tombstone(identity, run_id));
        ledger.control_tombstones.insert(
            command_id,
            RetiredControlCommand {
                command_id,
                command_digest: "c".repeat(64),
                run_id,
                applied_owner_epoch: 1,
                run_status: RunStatus::Succeeded,
            },
        );
        // Exact state after the first ledger fsync and before artifact deletion.
        write_ledger(root.path(), policy(), &mut ledger).unwrap();

        assert_eq!(
            repair_committed_tombstones(root.path(), policy()).unwrap(),
            1
        );
        assert!(!run_dir.exists());
        assert!(!receipt_dir.join(format!("{command_id}.json")).exists());
        let repaired = load_ledger(root.path()).unwrap();
        assert!(repaired.run_tombstones[&run_id].artifacts_removed);
        assert_eq!(
            repaired.control_tombstones[&command_id].command_digest,
            "c".repeat(64)
        );
    }

    #[test]
    fn indeterminate_and_incomplete_control_evidence_are_never_gc_candidates() {
        let root = tempfile::tempdir().unwrap();
        let identity = invocation();
        let indeterminate_id = Uuid::now_v7();
        LocalRuntimeHost::write_run_record(
            root.path(),
            &record(
                identity,
                indeterminate_id,
                LocalRunState::Finished {
                    status: "indeterminate".into(),
                },
            ),
        )
        .unwrap();
        write_event(root.path(), identity, indeterminate_id, "run.indeterminate");

        let accepted_id = Uuid::now_v7();
        LocalRuntimeHost::write_run_record(
            root.path(),
            &record(
                identity,
                accepted_id,
                LocalRunState::Finished {
                    status: "succeeded".into(),
                },
            ),
        )
        .unwrap();
        write_event(root.path(), identity, accepted_id, "run.succeeded");
        let command = crate::embedded::RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: identity,
            run_id: accepted_id,
            expected_owner_epoch: 1,
            action: RuntimeControlAction::Cancel {
                reason: "test".into(),
            },
        };
        let command_digest = hex::encode(Sha256::digest(serde_json::to_vec(&command).unwrap()));
        let receipt = RuntimeControlReceipt {
            schema_version: RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
            command_id: command.command_id,
            command_digest,
            invocation: identity,
            run_id: accepted_id,
            expected_owner_epoch: 1,
            action: command.action,
            state: RuntimeControlReceiptState::Accepted,
            applied_owner_epoch: 1,
            run_status: None,
        };
        let receipt_dir = root.path().join("control-receipts");
        std::fs::create_dir_all(&receipt_dir).unwrap();
        std::fs::write(
            receipt_dir.join(format!("{}.json", receipt.command_id)),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();

        let scan = scan_retention_candidates(root.path(), policy(), Utc::now()).unwrap();
        assert_eq!(scan.run_directories, 2);
        assert_eq!(scan.terminal_records, 2);
        assert!(scan.candidates.is_empty());
    }

    #[test]
    fn only_recovery_edges_not_completed_session_provenance_protect_hot_run_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let identity = invocation();
        let active_run_id = Uuid::now_v7();
        let completed_run_id = Uuid::now_v7();
        for run_id in [active_run_id, completed_run_id] {
            LocalRuntimeHost::write_run_record(
                root.path(),
                &record(
                    identity,
                    run_id,
                    LocalRunState::Finished {
                        status: "succeeded".into(),
                    },
                ),
            )
            .unwrap();
            write_event(root.path(), identity, run_id, "run.succeeded");
        }

        write_session_record(
            root.path(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Vec::new(),
            Some(active_run_id),
        );
        let completed_turn = SessionConversationTurn::new(
            1,
            completed_run_id,
            vec![
                Message {
                    role: Role::User,
                    content: vec![ContentPart::Text {
                        text: "retention-test".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Text { text: "ok".into() }],
                },
            ],
        );
        write_session_record(
            root.path(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            vec![completed_turn],
            None,
        );

        let scan = scan_retention_candidates(root.path(), policy(), Utc::now()).unwrap();
        assert_eq!(scan.strongly_referenced_runs, 1);
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].run_id, completed_run_id);
    }

    #[test]
    fn legacy_single_file_migrates_to_bounded_digest_bound_segments() {
        let root = tempfile::tempdir().unwrap();
        let identity = invocation();
        let mut ledger = RuntimeTerminalLedger::default();
        for _ in 0..600 {
            let run_id = Uuid::now_v7();
            let mut entry = tombstone(identity, run_id);
            entry.artifacts_removed = true;
            ledger.run_tombstones.insert(run_id, entry);
        }
        let migration_policy = RuntimeRetentionPolicy {
            max_run_tombstones_per_workspace: 1_000,
            max_run_tombstones_per_tenant: 2_000,
            ..policy()
        };
        write_ledger(root.path(), migration_policy, &mut ledger).unwrap();

        assert!(!legacy_ledger_path(root.path()).exists());
        let store = load_ledger_store(root.path()).unwrap();
        assert_eq!(store.aggregate.run_tombstones.len(), 600);
        assert_eq!(store.manifest.sealed_segments.len(), 2);
        assert_eq!(store.active.run_tombstones.len(), 88);
        assert!(
            store
                .manifest
                .sealed_segments
                .iter()
                .all(|segment| segment.run_tombstones == 256)
        );
    }

    #[test]
    fn sealed_history_is_immutable_while_the_bounded_active_segment_advances() {
        let root = tempfile::tempdir().unwrap();
        let identity = invocation();
        let segment_policy = RuntimeRetentionPolicy {
            max_run_tombstones_per_workspace: 1_000,
            max_run_tombstones_per_tenant: 2_000,
            ..policy()
        };
        let mut store = load_ledger_store(root.path()).unwrap();
        for _ in 0..520 {
            let run_id = Uuid::now_v7();
            let mut entry = tombstone(identity, run_id);
            entry.artifacts_removed = true;
            store.active.run_tombstones.insert(run_id, entry.clone());
            store.aggregate.run_tombstones.insert(run_id, entry);
        }
        persist_active_segment(root.path(), segment_policy, &mut store).unwrap();
        seal_full_active_segments(root.path(), &mut store).unwrap();
        assert_eq!(store.manifest.sealed_segments.len(), 2);
        assert_eq!(store.active.run_tombstones.len(), 8);
        let first_segment = sealed_segment_path(root.path(), 0);
        let first_digest = Sha256::digest(std::fs::read(&first_segment).unwrap());

        for _ in 0..10 {
            let run_id = Uuid::now_v7();
            let mut entry = tombstone(identity, run_id);
            entry.artifacts_removed = true;
            store.active.run_tombstones.insert(run_id, entry.clone());
            store.aggregate.run_tombstones.insert(run_id, entry);
        }
        persist_active_segment(root.path(), segment_policy, &mut store).unwrap();
        assert_eq!(
            first_digest,
            Sha256::digest(std::fs::read(&first_segment).unwrap()),
            "appending new tombstones must not rewrite sealed history"
        );
        assert_eq!(load_ledger(root.path()).unwrap().run_tombstones.len(), 530);
    }
}
