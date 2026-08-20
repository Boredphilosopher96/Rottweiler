//! Content-addressed touched-file checkpoints and deterministic rewind.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use rw_types::{ReviewFileDecision, ReviewFileStatus, SessionId, SessionReview, SessionReviewFile};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MANIFEST_VERSION: u16 = 1;
const OPAQUE_PENDING_VERSION: u16 = 1;
const REWIND_TRANSACTION_VERSION: u16 = 1;
const REVIEW_LEDGER_VERSION: u16 = 1;
const MAX_REVIEW_FILES: usize = 1_024;
const MAX_REVIEW_FILE_BYTES: usize = 256 * 1024;
const MAX_REVIEW_IDENTITY_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REVIEW_TOTAL_DIFF_BYTES: usize = 2 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
type CapturedRegular = (Vec<u8>, Option<u32>);
type CapturedReview = (ReviewCurrentState, Option<Vec<u8>>);

/// Pre-mutation state for one workspace-relative path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckpointFileState {
    /// File bytes are available in the content-addressed store.
    Present {
        /// Lowercase BLAKE3 digest.
        blob: String,
        /// Original byte length.
        bytes: u64,
        /// Unix permission bits where available.
        unix_mode: Option<u32>,
    },
    /// The path did not exist before the mutation.
    Absent,
    /// The mutation touched a path whose prior state was never captured.
    Unrestorable {
        /// Sanitized explanation surfaced by review/rewind.
        reason: String,
    },
}

/// Versioned per-turn manifest of files affected by a mutating tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointManifest {
    /// Manifest schema version.
    pub version: u16,
    /// Stable session id.
    pub session_id: String,
    /// Turn whose mutation this manifest precedes.
    pub turn: u64,
    /// Workspace-relative slash-normalized paths in deterministic order.
    pub files: BTreeMap<String, CheckpointFileState>,
}

/// Result of applying every manifest after a requested turn in reverse order.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RewindReport {
    /// Paths restored from blobs, including repeated historical restores.
    pub restored: Vec<String>,
    /// Paths removed because their prior state was absent.
    pub removed: Vec<String>,
    /// Paths that cannot honestly be restored.
    pub unrestorable: BTreeMap<String, String>,
}

/// Durable identity returned before an opaque command may execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueMutation {
    /// Stable session id.
    pub session_id: String,
    /// Turn containing the opaque command.
    pub turn: u64,
}

/// Caller-supplied identity of a two-phase rewind operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RewindHandle {
    /// Stable session id.
    pub session_id: String,
    /// Stable request/command id used to deduplicate recovery events.
    pub operation_id: String,
}

/// Workspace commit which must be recorded in the conversation before ack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindCommit {
    /// Durable operation identity to put in the conversation event.
    pub handle: RewindHandle,
    /// Conversation turn retained after rewind.
    pub target_turn: u64,
    /// Final restoration result.
    pub report: RewindReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum InventoryEntry {
    Regular { digest: String },
    Symlink { target: String },
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitTrackedEntry {
    object_id: String,
    unix_mode: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitTrackedBaseline {
    entries: BTreeMap<String, GitTrackedEntry>,
    paths: BTreeSet<String>,
    complete: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitDirtyPaths {
    paths: BTreeSet<String>,
    complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpaquePending {
    version: u16,
    session_id: String,
    turn: u64,
    before: BTreeMap<String, InventoryEntry>,
    tracked: BTreeMap<String, GitTrackedEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindStep {
    path: String,
    state: CheckpointFileState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RewindPhase {
    Applying,
    WorkspaceCommitted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindTransaction {
    version: u16,
    handle: RewindHandle,
    target_turn: u64,
    steps: Vec<RewindStep>,
    next_step: usize,
    report: RewindReport,
    phase: RewindPhase,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ReviewCurrentState {
    Present {
        content_blake3: String,
        bytes: u64,
        unix_mode: Option<u32>,
    },
    Absent,
    Unsupported {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDecisionRecord {
    decision: ReviewFileDecision,
    current: ReviewCurrentState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewLedger {
    version: u16,
    session_id: String,
    files: BTreeMap<String, ReviewDecisionRecord>,
}

/// Checkpoint storage bound to one canonical workspace root.
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    root: PathBuf,
    workspace: PathBuf,
    storage_relative: Option<String>,
    git_program: PathBuf,
}

impl CheckpointStore {
    /// Canonical workspace root whose paths this store snapshots and restores.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace
    }

    /// Creates a checkpoint store under `storage_root/checkpoints`.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace cannot be canonicalized or storage
    /// directories cannot be created.
    pub fn open(storage_root: &Path, workspace: &Path) -> Result<Self, CheckpointError> {
        let workspace = fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(CheckpointError::WorkspaceNotDirectory);
        }
        let root = storage_root.join("checkpoints");
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("manifests"))?;
        fs::create_dir_all(root.join("pending"))?;
        fs::create_dir_all(root.join("rewinds"))?;
        fs::create_dir_all(root.join("reviews"))?;
        let root = fs::canonicalize(root)?;
        let storage_relative = root
            .strip_prefix(&workspace)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .map(normalize_relative)
            .transpose()?;
        let store = Self {
            root,
            workspace,
            storage_relative,
            git_program: PathBuf::from("git"),
        };
        store.cleanup_stale_temporaries()?;
        Ok(store)
    }

    /// Captures the known pre-mutation state of the supplied relative paths.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe path, unreadable file, unsupported file
    /// kind, or durable blob/manifest write failure.
    pub fn checkpoint_known(
        &self,
        session_id: &str,
        turn: u64,
        relative_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(session_id)?;
        let path = self.manifest_path(session_id, turn);
        let mut files = if path.exists() {
            self.load_manifest(session_id, turn)?.files
        } else {
            BTreeMap::new()
        };
        for relative in relative_paths {
            let key = normalize_relative(&relative)?;
            if let Entry::Vacant(entry) = files.entry(key) {
                let key = entry.key().clone();
                let state = self.capture(&key)?;
                entry.insert(state);
            }
        }
        self.persist_manifest(session_id, turn, files)
    }

    /// Adds post-scan paths whose pre-state was not captured (for example a
    /// new file produced by opaque shell execution).
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths or a durable manifest rewrite failure.
    pub fn mark_unrestorable(
        &self,
        manifest: &mut CheckpointManifest,
        relative_paths: impl IntoIterator<Item = PathBuf>,
        reason: &str,
    ) -> Result<(), CheckpointError> {
        if manifest.version != MANIFEST_VERSION {
            return Err(CheckpointError::UnsupportedManifestVersion(
                manifest.version,
            ));
        }
        for relative in relative_paths {
            let key = normalize_relative(&relative)?;
            manifest
                .files
                .entry(key)
                .or_insert_with(|| CheckpointFileState::Unrestorable {
                    reason: reason.to_owned(),
                });
        }
        self.write_manifest(manifest)
    }

    /// Loads one persisted turn manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe ids, I/O, JSON, or schema mismatch.
    pub fn load_manifest(
        &self,
        session_id: &str,
        turn: u64,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(session_id)?;
        let bytes = fs::read(self.manifest_path(session_id, turn))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&bytes)?;
        Self::validate_manifest(&manifest, session_id, turn)?;
        Ok(manifest)
    }

    /// Persists a complete opaque-mutation baseline before shell execution.
    ///
    /// The caller must not start the command until this method returns. Dirty
    /// tracked files are snapshotted immediately; clean tracked files retain
    /// their Git object ids; untracked paths retain only fingerprints so an
    /// unknown overwritten preimage is reported honestly.
    ///
    /// # Errors
    ///
    /// Returns an error for an existing pending mutation, unsafe workspace
    /// entries, or a durable baseline/manifest write failure.
    pub fn begin_opaque_mutation(
        &self,
        session_id: &str,
        turn: u64,
    ) -> Result<OpaqueMutation, CheckpointError> {
        validate_session_id(session_id)?;
        let pending_path = self.pending_path(session_id, turn);
        if pending_path.exists() {
            return Err(CheckpointError::OpaqueMutationPending);
        }
        let before = self.workspace_inventory()?;
        let tracked = self.git_tracked_baseline()?;
        let dirty = self.git_dirty_tracked_paths()?;
        let dirty = if tracked.complete && !dirty.complete {
            tracked.paths.clone()
        } else {
            dirty.paths
        };
        let mut files = self
            .load_manifest_if_exists(session_id, turn)?
            .map_or_else(BTreeMap::new, |manifest| manifest.files);
        for path in dirty {
            if let Entry::Vacant(entry) = files.entry(path.clone()) {
                let state = match self.capture(&path) {
                    Ok(state) => state,
                    Err(CheckpointError::UnsupportedFileKind(_)) => {
                        CheckpointFileState::Unrestorable {
                            reason: "opaque command pre-state is not a regular file".to_owned(),
                        }
                    }
                    Err(error) => return Err(error),
                };
                entry.insert(state);
            }
        }
        self.persist_manifest(session_id, turn, files)?;
        let pending = OpaquePending {
            version: OPAQUE_PENDING_VERSION,
            session_id: session_id.to_owned(),
            turn,
            before,
            tracked: tracked.entries,
        };
        atomic_replace(&pending_path, &serde_json::to_vec(&pending)?)?;
        Ok(OpaqueMutation {
            session_id: session_id.to_owned(),
            turn,
        })
    }

    /// Finishes the post-scan for a completed or interrupted opaque command.
    ///
    /// The final manifest is durable before the pending marker is removed. A
    /// kill in either phase is therefore recovered idempotently on startup.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing/corrupt marker, unsafe path, unavailable
    /// required preimage, or durable manifest failure.
    pub fn finish_opaque_mutation(
        &self,
        mutation: &OpaqueMutation,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(&mutation.session_id)?;
        let pending = self.load_pending(&mutation.session_id, mutation.turn)?;
        let after = self.workspace_inventory()?;
        let mut manifest = self
            .load_manifest_if_exists(&mutation.session_id, mutation.turn)?
            .ok_or(CheckpointError::CorruptManifest)?;
        let changed = changed_inventory_paths(&pending.before, &after);
        for path in changed {
            if manifest.files.contains_key(&path) {
                continue;
            }
            let state = if !pending.before.contains_key(&path) {
                match after.get(&path) {
                    Some(InventoryEntry::Directory) => CheckpointFileState::Unrestorable {
                        reason: "opaque command created a directory that M2 cannot remove safely"
                            .to_owned(),
                    },
                    Some(InventoryEntry::Regular { .. } | InventoryEntry::Symlink { .. })
                    | None => CheckpointFileState::Absent,
                }
            } else if let Some(tracked) = pending.tracked.get(&path) {
                self.capture_git_preimage(tracked).unwrap_or_else(|| {
                    CheckpointFileState::Unrestorable {
                        reason: "opaque command changed a tracked path whose Git preimage is unavailable"
                            .to_owned(),
                    }
                })
            } else {
                CheckpointFileState::Unrestorable {
                    reason:
                        "opaque command changed an untracked path before its bytes were snapshotted"
                            .to_owned(),
                }
            };
            manifest.files.insert(path, state);
        }
        self.write_manifest(&manifest)?;
        remove_durable(&self.pending_path(&mutation.session_id, mutation.turn))?;
        Ok(manifest)
    }

    /// Completes every durable opaque post-scan left by a killed process.
    ///
    /// # Errors
    ///
    /// Returns an error rather than silently discarding an invalid marker.
    pub fn recover_opaque_mutations(&self) -> Result<Vec<CheckpointManifest>, CheckpointError> {
        let mut pending = self.enumerate_pending()?;
        pending.sort_by(|left, right| {
            (&left.session_id, left.turn).cmp(&(&right.session_id, right.turn))
        });
        pending
            .into_iter()
            .map(|mutation| self.finish_opaque_mutation(&mutation))
            .collect()
    }

    /// Prevalidates and durably stages a rewind before any workspace mutation.
    ///
    /// `operation_id` should be the client request id and must also be included
    /// in the eventual conversation event for recovery deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe id, another pending rewind, or any
    /// manifest/blob corruption. No workspace path is changed on error.
    pub fn prepare_rewind(
        &self,
        session_id: &str,
        target_turn: u64,
        operation_id: &str,
    ) -> Result<RewindHandle, CheckpointError> {
        validate_session_id(session_id)?;
        validate_operation_id(operation_id)?;
        let path = self.rewind_path(session_id);
        if path.exists() {
            let existing = self.load_rewind_transaction(session_id)?;
            if existing.handle.operation_id == operation_id && existing.target_turn == target_turn {
                return Ok(existing.handle);
            }
            return Err(CheckpointError::RewindPending);
        }
        let steps = self.build_rewind_steps(session_id, target_turn)?;
        self.validate_rewind_steps(&steps, true)?;
        let handle = RewindHandle {
            session_id: session_id.to_owned(),
            operation_id: operation_id.to_owned(),
        };
        let transaction = RewindTransaction {
            version: REWIND_TRANSACTION_VERSION,
            handle: handle.clone(),
            target_turn,
            steps,
            next_step: 0,
            report: RewindReport::default(),
            phase: RewindPhase::Applying,
        };
        self.write_rewind_transaction(&transaction)?;
        Ok(handle)
    }

    /// Applies or resumes a staged rewind and leaves a durable commit marker.
    ///
    /// The caller must append a conversation rewind event containing
    /// `handle.operation_id`, then call [`Self::acknowledge_rewind`].
    ///
    /// # Errors
    ///
    /// Returns an error for identity mismatch or a confined filesystem write.
    pub fn apply_rewind(&self, handle: &RewindHandle) -> Result<RewindCommit, CheckpointError> {
        let mut transaction = self.load_rewind_transaction(&handle.session_id)?;
        if transaction.handle != *handle {
            return Err(CheckpointError::RewindIdentityMismatch);
        }
        while transaction.phase == RewindPhase::Applying
            && transaction.next_step < transaction.steps.len()
        {
            let step = transaction.steps[transaction.next_step].clone();
            self.restore_state(&step.path, &step.state, &mut transaction.report)?;
            transaction.next_step += 1;
            self.write_rewind_transaction(&transaction)?;
        }
        if transaction.phase == RewindPhase::Applying {
            transaction.phase = RewindPhase::WorkspaceCommitted;
            self.write_rewind_transaction(&transaction)?;
        }
        Ok(RewindCommit {
            handle: transaction.handle,
            target_turn: transaction.target_turn,
            report: transaction.report,
        })
    }

    /// Resumes all staged rewinds and returns workspace commits needing a
    /// deduplicated conversation event and acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns an error for a corrupt transaction or confined write failure.
    pub fn recover_rewinds(&self) -> Result<Vec<RewindCommit>, CheckpointError> {
        let handles = self.enumerate_rewinds()?;
        handles
            .iter()
            .map(|handle| self.apply_rewind(handle))
            .collect()
    }

    /// Discards a staged rewind before any workspace step has been applied.
    ///
    /// Multi-root coordinators use this to roll back preparation when another
    /// root cannot stage the same operation. Once application begins, only
    /// idempotent completion is safe.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity differs or workspace application has
    /// already started.
    pub fn discard_prepared_rewind(
        &self,
        handle: &RewindHandle,
        target_turn: u64,
    ) -> Result<(), CheckpointError> {
        if !self.rewind_path(&handle.session_id).try_exists()? {
            return Ok(());
        }
        let transaction = self.load_rewind_transaction(&handle.session_id)?;
        if transaction.handle != *handle || transaction.target_turn != target_turn {
            return Err(CheckpointError::RewindIdentityMismatch);
        }
        if transaction.phase != RewindPhase::Applying
            || transaction.next_step != 0
            || transaction.report != RewindReport::default()
        {
            return Err(CheckpointError::RewindCannotDiscard);
        }
        remove_durable(&self.rewind_path(&handle.session_id))
    }

    /// Removes a workspace-commit marker after its conversation event is
    /// durably appended.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace is not committed or identity differs.
    pub fn acknowledge_rewind(&self, handle: &RewindHandle) -> Result<(), CheckpointError> {
        let transaction = self.load_rewind_transaction(&handle.session_id)?;
        if transaction.handle != *handle {
            return Err(CheckpointError::RewindIdentityMismatch);
        }
        if transaction.phase != RewindPhase::WorkspaceCommitted {
            return Err(CheckpointError::RewindNotCommitted);
        }
        remove_durable(&self.rewind_path(&handle.session_id))
    }

    /// Computes the cumulative session diff from each path's earliest captured
    /// preimage to its current confined workspace state.
    ///
    /// Accepted and reverted decisions are fingerprint-bound. A later edit
    /// automatically returns that path to `pending` without mutating history.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt manifests/blobs, unsafe paths, an excessive
    /// file count, or an unreadable workspace state.
    pub fn session_review(&self, session_id: &str) -> Result<SessionReview, CheckpointError> {
        validate_session_id(session_id)?;
        let baselines = self.cumulative_baselines(session_id)?;
        if baselines.len() > MAX_REVIEW_FILES {
            return Err(CheckpointError::ReviewFileLimit);
        }
        let ledger = self.load_review_ledger(session_id)?;
        let mut remaining_diff_bytes = MAX_REVIEW_TOTAL_DIFF_BYTES;
        let mut files = Vec::with_capacity(baselines.len());
        for (path, baseline) in baselines {
            let (current, current_content) = self.capture_review_current(&path)?;
            let matching_decision = ledger
                .files
                .get(&path)
                .filter(|record| record.current == current);
            let unchanged = baseline_matches_current(&baseline, &current);
            if unchanged && matching_decision.is_none() {
                continue;
            }
            let status = matching_decision.map_or(ReviewFileStatus::Pending, |record| match record
                .decision
            {
                ReviewFileDecision::Accept => ReviewFileStatus::Accepted,
                ReviewFileDecision::Revert => ReviewFileStatus::Reverted,
            });
            let (unified_diff, truncated, unrestorable_reason) = self.render_review_diff(
                &path,
                &baseline,
                &current,
                current_content.as_deref(),
                remaining_diff_bytes,
            )?;
            remaining_diff_bytes = remaining_diff_bytes.saturating_sub(unified_diff.len());
            files.push(SessionReviewFile {
                path,
                unified_diff,
                status,
                truncated,
                unrestorable_reason,
                original_hash: review_identity(&baseline)?,
                current_hash: review_identity(&current)?,
            });
        }
        Ok(SessionReview {
            session_id: SessionId(session_id.to_owned()),
            files,
        })
    }

    /// Accepts or reverts exactly one cumulative-review path and returns a
    /// complete refreshed review snapshot.
    ///
    /// # Errors
    ///
    /// Decisions fail closed for unrestorable entries. All paths are normalized
    /// and restored through the same confined filesystem boundary as rewind.
    pub fn resolve_review_file(
        &self,
        session_id: &str,
        relative_path: &Path,
        decision: ReviewFileDecision,
        expected_current_hash: &str,
    ) -> Result<SessionReview, CheckpointError> {
        validate_session_id(session_id)?;
        let path = normalize_relative(relative_path)?;
        let before = self.session_review(session_id)?;
        let file = before
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or(CheckpointError::ReviewPathNotFound)?;
        if file.current_hash != expected_current_hash {
            return Err(CheckpointError::ReviewPathChanged);
        }
        if file.unrestorable_reason.is_some() {
            return Err(CheckpointError::ReviewPathNotRevertible);
        }
        let baselines = self.cumulative_baselines(session_id)?;
        let baseline = baselines
            .get(&path)
            .ok_or(CheckpointError::ReviewPathNotFound)?;
        let (current_before_decision, _) = self.capture_review_current(&path)?;
        if review_identity(&current_before_decision)? != expected_current_hash {
            return Err(CheckpointError::ReviewPathChanged);
        }
        let current = if decision == ReviewFileDecision::Revert {
            self.restore_state(&path, baseline, &mut RewindReport::default())?;
            let (restored, _) = self.capture_review_current(&path)?;
            if !baseline_matches_current(baseline, &restored) {
                return Err(CheckpointError::ReviewPathChanged);
            }
            restored
        } else {
            current_before_decision
        };
        let mut ledger = self.load_review_ledger(session_id)?;
        ledger
            .files
            .insert(path, ReviewDecisionRecord { decision, current });
        self.write_review_ledger(&ledger)?;
        self.session_review(session_id)
    }

    /// Copies checkpoint ownership through `through_turn` to a child session.
    /// Immutable content-addressed blobs remain shared; parent manifests and
    /// review decisions are never modified or inherited.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ids, a pre-existing child checkpoint
    /// namespace, or corrupt parent manifests.
    pub fn fork_session(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        through_turn: Option<u64>,
    ) -> Result<(), CheckpointError> {
        self.fork_into(self, parent_session_id, child_session_id, through_turn)
    }

    /// Copies one session's checkpoint prefix into a different session-bound
    /// checkpoint store. Every referenced blob is revalidated and installed in
    /// the target content-addressed store before its child manifest is exposed.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched workspaces, invalid identities, corrupt
    /// source data, or a pre-existing child namespace.
    pub fn fork_into(
        &self,
        target: &CheckpointStore,
        parent_session_id: &str,
        child_session_id: &str,
        through_turn: Option<u64>,
    ) -> Result<(), CheckpointError> {
        validate_session_id(parent_session_id)?;
        validate_session_id(child_session_id)?;
        if parent_session_id == child_session_id {
            return Err(CheckpointError::ForkIdentityConflict);
        }
        if self.workspace != target.workspace {
            return Err(CheckpointError::ForkWorkspaceMismatch);
        }
        let manifests_directory = target.root.join("manifests");
        let child_directory = manifests_directory.join(child_session_id);
        let mut turns = self.manifest_turns(parent_session_id)?;
        turns.sort_unstable();
        if let Some(through_turn) = through_turn {
            turns.retain(|turn| *turn <= through_turn);
        }
        if child_directory.exists() {
            return Err(CheckpointError::ForkTargetExists);
        }
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging = manifests_directory.join(format!(".rw-{}-{nonce}.tmp", std::process::id()));
        match fs::create_dir(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(CheckpointError::ForkTargetExists);
            }
            Err(error) => return Err(error.into()),
        }
        let result = (|| {
            for turn in turns {
                let mut manifest = self.load_manifest(parent_session_id, turn)?;
                for state in manifest.files.values() {
                    if let CheckpointFileState::Present { blob, bytes, .. } = state {
                        let content = self.read_valid_blob(blob, *bytes)?;
                        target.write_blob(blob, &content)?;
                    }
                }
                child_session_id.clone_into(&mut manifest.session_id);
                Self::validate_manifest(&manifest, child_session_id, turn)?;
                atomic_replace(
                    &staging.join(format!("{turn:020}.json")),
                    &serde_json::to_vec_pretty(&manifest)?,
                )?;
            }
            File::open(&staging)?.sync_all()?;
            fs::rename(&staging, &child_directory)?;
            File::open(&manifests_directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn cumulative_baselines(
        &self,
        session_id: &str,
    ) -> Result<BTreeMap<String, CheckpointFileState>, CheckpointError> {
        let mut turns = self.manifest_turns(session_id)?;
        turns.sort_unstable();
        let mut baselines = BTreeMap::new();
        for turn in turns {
            for (path, state) in self.load_manifest(session_id, turn)?.files {
                if path.chars().any(char::is_control) {
                    return Err(CheckpointError::UnsafePath);
                }
                baselines.entry(path).or_insert(state);
            }
        }
        Ok(baselines)
    }

    fn capture_review_current(
        &self,
        path: &str,
    ) -> Result<(ReviewCurrentState, Option<Vec<u8>>), CheckpointError> {
        match capture_review_regular_confined(&self.workspace, path) {
            Ok(Some(captured)) => Ok(captured),
            Ok(None) => Ok((ReviewCurrentState::Absent, None)),
            Err(CheckpointError::UnsupportedFileKind(_)) => Ok((
                ReviewCurrentState::Unsupported {
                    reason: "current path is not a regular file".to_owned(),
                },
                None,
            )),
            Err(CheckpointError::ReviewIdentityLimit) => Ok((
                ReviewCurrentState::Unsupported {
                    reason: "current file exceeds the review identity scan limit".to_owned(),
                },
                None,
            )),
            Err(error) => Err(error),
        }
    }

    fn render_review_diff(
        &self,
        path: &str,
        baseline: &CheckpointFileState,
        current: &ReviewCurrentState,
        current_content: Option<&[u8]>,
        remaining_bytes: usize,
    ) -> Result<(String, bool, Option<String>), CheckpointError> {
        let unrestorable = match (baseline, current) {
            (CheckpointFileState::Unrestorable { reason }, _)
            | (_, ReviewCurrentState::Unsupported { reason }) => Some(reason.clone()),
            _ => None,
        };
        if let Some(reason) = unrestorable {
            return Ok((String::new(), false, Some(reason)));
        }
        let original = match baseline {
            CheckpointFileState::Present { blob, bytes, .. } => {
                if *bytes > 256 * 1024 {
                    return Ok((String::new(), true, None));
                }
                Some(self.read_valid_blob(blob, *bytes)?)
            }
            CheckpointFileState::Absent => None,
            CheckpointFileState::Unrestorable { .. } => unreachable!(),
        };
        let current_bytes = match current {
            ReviewCurrentState::Present { bytes, .. } if *bytes > 256 * 1024 => {
                return Ok((String::new(), true, None));
            }
            ReviewCurrentState::Present { .. } => current_content.map(<[u8]>::to_vec),
            ReviewCurrentState::Absent => None,
            ReviewCurrentState::Unsupported { .. } => unreachable!(),
        };
        let (diff, mut truncated) = render_whole_file_diff(
            path,
            original.as_deref(),
            current_bytes.as_deref(),
            remaining_bytes.min(MAX_REVIEW_FILE_BYTES),
        );
        if remaining_bytes == 0 {
            truncated = true;
        }
        Ok((diff, truncated, None))
    }

    fn load_review_ledger(&self, session_id: &str) -> Result<ReviewLedger, CheckpointError> {
        let path = self.review_path(session_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReviewLedger {
                    version: REVIEW_LEDGER_VERSION,
                    session_id: session_id.to_owned(),
                    files: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let ledger: ReviewLedger = serde_json::from_slice(&bytes)?;
        if ledger.version != REVIEW_LEDGER_VERSION || ledger.session_id != session_id {
            return Err(CheckpointError::CorruptReviewLedger);
        }
        for (path, record) in &ledger.files {
            if normalize_relative(Path::new(path))? != *path {
                return Err(CheckpointError::CorruptReviewLedger);
            }
            validate_review_current(&record.current)?;
        }
        Ok(ledger)
    }

    fn write_review_ledger(&self, ledger: &ReviewLedger) -> Result<(), CheckpointError> {
        atomic_replace(
            &self.review_path(&ledger.session_id),
            &serde_json::to_vec(ledger)?,
        )
    }

    fn capture(&self, key: &str) -> Result<CheckpointFileState, CheckpointError> {
        let Some((bytes, unix_mode)) = capture_regular_confined(&self.workspace, key)? else {
            return Ok(CheckpointFileState::Absent);
        };
        self.capture_bytes(&bytes, unix_mode)
    }

    fn capture_bytes(
        &self,
        bytes: &[u8],
        unix_mode: Option<u32>,
    ) -> Result<CheckpointFileState, CheckpointError> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        self.write_blob(&digest, bytes)?;
        Ok(CheckpointFileState::Present {
            blob: digest,
            bytes: u64::try_from(bytes.len()).map_err(|_| CheckpointError::CorruptBlob)?,
            unix_mode,
        })
    }

    fn capture_git_preimage(&self, tracked: &GitTrackedEntry) -> Option<CheckpointFileState> {
        let unix_mode = tracked.unix_mode?;
        let output = self
            .git_command()
            .arg("-C")
            .arg(&self.workspace)
            .args(["cat-file", "blob", &tracked.object_id])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        self.capture_bytes(&output.stdout, Some(unix_mode)).ok()
    }

    fn persist_manifest(
        &self,
        session_id: &str,
        turn: u64,
        files: BTreeMap<String, CheckpointFileState>,
    ) -> Result<CheckpointManifest, CheckpointError> {
        let manifest = CheckpointManifest {
            version: MANIFEST_VERSION,
            session_id: session_id.to_owned(),
            turn,
            files,
        };
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    fn write_manifest(&self, manifest: &CheckpointManifest) -> Result<(), CheckpointError> {
        Self::validate_manifest(manifest, &manifest.session_id, manifest.turn)?;
        let bytes = serde_json::to_vec_pretty(manifest)?;
        atomic_replace(
            &self.manifest_path(&manifest.session_id, manifest.turn),
            &bytes,
        )
    }

    fn write_blob(&self, digest: &str, bytes: &[u8]) -> Result<(), CheckpointError> {
        let prefix = digest.get(..2).ok_or(CheckpointError::CorruptBlob)?;
        let path = self.root.join("blobs").join(prefix).join(digest);
        if path.exists() {
            let existing = fs::read(&path)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(CheckpointError::CorruptBlob);
        }
        atomic_replace(&path, bytes)
    }

    fn restore_state(
        &self,
        key: &str,
        state: &CheckpointFileState,
        report: &mut RewindReport,
    ) -> Result<(), CheckpointError> {
        let normalized = normalize_relative(Path::new(key))?;
        if normalized != key {
            return Err(CheckpointError::CorruptManifest);
        }
        match state {
            CheckpointFileState::Present {
                blob,
                bytes,
                unix_mode,
            } => {
                let content = self.read_valid_blob(blob, *bytes)?;
                restore_regular_confined(&self.workspace, key, &content, *unix_mode)?;
                report.unrestorable.remove(key);
                report.restored.push(key.to_owned());
            }
            CheckpointFileState::Absent => {
                remove_file_or_symlink_confined(&self.workspace, key)?;
                report.unrestorable.remove(key);
                report.removed.push(key.to_owned());
            }
            CheckpointFileState::Unrestorable { reason } => {
                report.unrestorable.insert(key.to_owned(), reason.clone());
            }
        }
        Ok(())
    }

    fn read_valid_blob(&self, blob: &str, bytes: u64) -> Result<Vec<u8>, CheckpointError> {
        if !is_lower_blake3(blob) {
            return Err(CheckpointError::CorruptBlob);
        }
        let prefix = blob.get(..2).ok_or(CheckpointError::CorruptBlob)?;
        let content = fs::read(self.root.join("blobs").join(prefix).join(blob))?;
        if u64::try_from(content.len()).map_err(|_| CheckpointError::CorruptBlob)? != bytes
            || blake3::hash(&content).to_hex().as_str() != blob
        {
            return Err(CheckpointError::CorruptBlob);
        }
        Ok(content)
    }

    fn load_manifest_if_exists(
        &self,
        session_id: &str,
        turn: u64,
    ) -> Result<Option<CheckpointManifest>, CheckpointError> {
        match self.load_manifest(session_id, turn) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(CheckpointError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn workspace_inventory(&self) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
        inventory_confined(&self.workspace, self.storage_relative.as_deref())
    }

    fn git_tracked_baseline(&self) -> Result<GitTrackedBaseline, CheckpointError> {
        let output = match self
            .git_command()
            .arg("-C")
            .arg(&self.workspace)
            .args(["ls-files", "--cached", "--stage", "-z", "--", "."])
            .output()
        {
            Ok(output) if output.status.success() => output,
            Ok(_) | Err(_) => return Ok(GitTrackedBaseline::default()),
        };
        let mut baseline = GitTrackedBaseline {
            complete: true,
            ..GitTrackedBaseline::default()
        };
        for record in output.stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let record = std::str::from_utf8(record).map_err(|_| CheckpointError::UnsafePath)?;
            let (metadata, path) = record
                .split_once('\t')
                .ok_or(CheckpointError::GitBaselineCorrupt)?;
            let mut fields = metadata.split_ascii_whitespace();
            let mode = fields.next().ok_or(CheckpointError::GitBaselineCorrupt)?;
            let object_id = fields
                .next()
                .ok_or(CheckpointError::GitBaselineCorrupt)?
                .to_owned();
            let stage = fields.next().ok_or(CheckpointError::GitBaselineCorrupt)?;
            let key = normalize_relative(Path::new(path))?;
            baseline.paths.insert(key.clone());
            if stage != "0" {
                continue;
            }
            if !matches!(object_id.len(), 40 | 64)
                || !object_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(CheckpointError::GitBaselineCorrupt);
            }
            let unix_mode = match mode {
                "100644" => Some(0o644),
                "100755" => Some(0o755),
                _ => None,
            };
            baseline.entries.insert(
                key,
                GitTrackedEntry {
                    object_id,
                    unix_mode,
                },
            );
        }
        Ok(baseline)
    }

    fn git_dirty_tracked_paths(&self) -> Result<GitDirtyPaths, CheckpointError> {
        let mut query = GitDirtyPaths {
            complete: true,
            ..GitDirtyPaths::default()
        };
        for arguments in [
            [
                "diff",
                "--relative",
                "--name-only",
                "-z",
                "--cached",
                "--",
                ".",
            ],
            ["diff", "--relative", "--name-only", "-z", "--", ".", ""],
        ] {
            let args = arguments
                .iter()
                .copied()
                .filter(|argument| !argument.is_empty());
            let output = match self
                .git_command()
                .arg("-C")
                .arg(&self.workspace)
                .args(args)
                .output()
            {
                Ok(output) if output.status.success() => output,
                Ok(_) | Err(_) => {
                    query.complete = false;
                    continue;
                }
            };
            for path in output.stdout.split(|byte| *byte == 0) {
                if path.is_empty() {
                    continue;
                }
                let path = std::str::from_utf8(path).map_err(|_| CheckpointError::UnsafePath)?;
                query.paths.insert(normalize_relative(Path::new(path))?);
            }
        }
        Ok(query)
    }

    fn git_command(&self) -> Command {
        Command::new(&self.git_program)
    }

    #[cfg(test)]
    fn with_git_program(mut self, program: PathBuf) -> Self {
        self.git_program = program;
        self
    }

    fn load_pending(&self, session_id: &str, turn: u64) -> Result<OpaquePending, CheckpointError> {
        let pending: OpaquePending =
            serde_json::from_slice(&fs::read(self.pending_path(session_id, turn))?)?;
        if pending.version != OPAQUE_PENDING_VERSION
            || pending.session_id != session_id
            || pending.turn != turn
        {
            return Err(CheckpointError::CorruptOpaqueMutation);
        }
        validate_session_id(&pending.session_id)?;
        for (path, entry) in &pending.before {
            if normalize_relative(Path::new(path))? != *path {
                return Err(CheckpointError::CorruptOpaqueMutation);
            }
            let digest = match entry {
                InventoryEntry::Regular { digest } => Some(digest),
                InventoryEntry::Symlink { target } => Some(target),
                InventoryEntry::Directory => None,
            };
            if digest.is_some_and(|digest| !is_lower_blake3(digest)) {
                return Err(CheckpointError::CorruptOpaqueMutation);
            }
        }
        for (path, entry) in &pending.tracked {
            if normalize_relative(Path::new(path))? != *path
                || !matches!(entry.object_id.len(), 40 | 64)
                || !entry
                    .object_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                || !matches!(entry.unix_mode, None | Some(0o644 | 0o755))
            {
                return Err(CheckpointError::CorruptOpaqueMutation);
            }
        }
        Ok(pending)
    }

    fn enumerate_pending(&self) -> Result<Vec<OpaqueMutation>, CheckpointError> {
        let root = self.root.join("pending");
        cleanup_stale_temporaries_in(&root)?;
        let mut mutations = Vec::new();
        for session in fs::read_dir(&root)? {
            let session = session?;
            let metadata = fs::symlink_metadata(session.path())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(CheckpointError::CorruptOpaqueMutation);
            }
            let session_id = session
                .file_name()
                .into_string()
                .map_err(|_| CheckpointError::CorruptOpaqueMutation)?;
            validate_session_id(&session_id)?;
            cleanup_stale_temporaries_in(&session.path())?;
            for entry in fs::read_dir(session.path())? {
                let entry = entry?;
                let turn = parse_exact_turn_filename(&entry.file_name())
                    .ok_or(CheckpointError::CorruptOpaqueMutation)?;
                if !fs::symlink_metadata(entry.path())?.is_file() {
                    return Err(CheckpointError::CorruptOpaqueMutation);
                }
                mutations.push(OpaqueMutation {
                    session_id: session_id.clone(),
                    turn,
                });
            }
        }
        Ok(mutations)
    }

    fn build_rewind_steps(
        &self,
        session_id: &str,
        target_turn: u64,
    ) -> Result<Vec<RewindStep>, CheckpointError> {
        let mut turns = self.manifest_turns(session_id)?;
        turns.retain(|turn| *turn > target_turn);
        turns.sort_unstable_by(|left, right| right.cmp(left));
        let mut steps = Vec::new();
        for turn in turns {
            let manifest = self.load_manifest(session_id, turn)?;
            steps.extend(
                manifest
                    .files
                    .into_iter()
                    .map(|(path, state)| RewindStep { path, state }),
            );
        }
        Ok(steps)
    }

    fn validate_rewind_steps(
        &self,
        steps: &[RewindStep],
        validate_blobs: bool,
    ) -> Result<(), CheckpointError> {
        for step in steps {
            if normalize_relative(Path::new(&step.path))? != step.path {
                return Err(CheckpointError::CorruptManifest);
            }
            match &step.state {
                CheckpointFileState::Present {
                    blob,
                    bytes,
                    unix_mode,
                } => {
                    if unix_mode.is_some_and(|mode| mode > 0o7777) {
                        return Err(CheckpointError::CorruptRewindTransaction);
                    }
                    if validate_blobs {
                        self.read_valid_blob(blob, *bytes)?;
                    } else if !is_lower_blake3(blob) {
                        return Err(CheckpointError::CorruptRewindTransaction);
                    }
                }
                CheckpointFileState::Unrestorable { reason }
                    if reason.is_empty()
                        || reason.len() > 1_024
                        || reason.chars().any(char::is_control) =>
                {
                    return Err(CheckpointError::CorruptRewindTransaction);
                }
                CheckpointFileState::Absent | CheckpointFileState::Unrestorable { .. } => {}
            }
        }
        Ok(())
    }

    fn manifest_turns(&self, session_id: &str) -> Result<Vec<u64>, CheckpointError> {
        let directory = self.root.join("manifests").join(session_id);
        if !directory.exists() {
            return Ok(Vec::new());
        }
        cleanup_stale_temporaries_in(&directory)?;
        let mut turns = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !fs::symlink_metadata(entry.path())?.is_file() {
                return Err(CheckpointError::CorruptManifest);
            }
            turns.push(
                parse_exact_turn_filename(&entry.file_name())
                    .ok_or(CheckpointError::CorruptManifest)?,
            );
        }
        Ok(turns)
    }

    fn load_rewind_transaction(
        &self,
        session_id: &str,
    ) -> Result<RewindTransaction, CheckpointError> {
        validate_session_id(session_id)?;
        let transaction: RewindTransaction =
            serde_json::from_slice(&fs::read(self.rewind_path(session_id))?)?;
        if transaction.version != REWIND_TRANSACTION_VERSION
            || transaction.handle.session_id != session_id
            || transaction.next_step > transaction.steps.len()
            || (transaction.phase == RewindPhase::WorkspaceCommitted
                && transaction.next_step != transaction.steps.len())
        {
            return Err(CheckpointError::CorruptRewindTransaction);
        }
        validate_operation_id(&transaction.handle.operation_id)?;
        self.validate_rewind_steps(&transaction.steps, false)?;
        if transaction.phase == RewindPhase::Applying {
            self.validate_rewind_steps(&transaction.steps[transaction.next_step..], true)?;
        }
        validate_rewind_report(&transaction.report)?;
        Ok(transaction)
    }

    fn write_rewind_transaction(
        &self,
        transaction: &RewindTransaction,
    ) -> Result<(), CheckpointError> {
        atomic_replace(
            &self.rewind_path(&transaction.handle.session_id),
            &serde_json::to_vec(transaction)?,
        )
    }

    fn enumerate_rewinds(&self) -> Result<Vec<RewindHandle>, CheckpointError> {
        let directory = self.root.join("rewinds");
        cleanup_stale_temporaries_in(&directory)?;
        let mut handles = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !fs::symlink_metadata(entry.path())?.is_file() {
                return Err(CheckpointError::CorruptRewindTransaction);
            }
            let filename = entry
                .file_name()
                .into_string()
                .map_err(|_| CheckpointError::CorruptRewindTransaction)?;
            let session_id = filename
                .strip_suffix(".json")
                .ok_or(CheckpointError::CorruptRewindTransaction)?;
            validate_session_id(session_id)?;
            handles.push(self.load_rewind_transaction(session_id)?.handle);
        }
        handles.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(handles)
    }

    fn cleanup_stale_temporaries(&self) -> Result<(), CheckpointError> {
        for directory in [
            self.root.join("blobs"),
            self.root.join("manifests"),
            self.root.join("pending"),
            self.root.join("rewinds"),
            self.root.join("reviews"),
        ] {
            cleanup_stale_temporaries_in(&directory)?;
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                if fs::symlink_metadata(entry.path())?.is_dir() {
                    cleanup_stale_temporaries_in(&entry.path())?;
                }
            }
        }
        Ok(())
    }

    fn validate_manifest(
        manifest: &CheckpointManifest,
        session_id: &str,
        turn: u64,
    ) -> Result<(), CheckpointError> {
        if manifest.version != MANIFEST_VERSION {
            return Err(CheckpointError::UnsupportedManifestVersion(
                manifest.version,
            ));
        }
        if manifest.session_id != session_id || manifest.turn != turn {
            return Err(CheckpointError::CorruptManifest);
        }
        validate_session_id(&manifest.session_id)?;
        let mut canonical = BTreeSet::new();
        for (key, state) in &manifest.files {
            let normalized = normalize_relative(Path::new(key))?;
            if normalized != *key || !canonical.insert(normalized) {
                return Err(CheckpointError::CorruptManifest);
            }
            match state {
                CheckpointFileState::Present {
                    blob, unix_mode, ..
                } if blob.len() != 64
                    || !blob.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || blob.bytes().any(|byte| byte.is_ascii_uppercase())
                    || unix_mode.is_some_and(|mode| mode > 0o7777) =>
                {
                    return Err(CheckpointError::CorruptManifest);
                }
                CheckpointFileState::Unrestorable { reason }
                    if reason.is_empty()
                        || reason.len() > 1_024
                        || reason.chars().any(char::is_control) =>
                {
                    return Err(CheckpointError::CorruptManifest);
                }
                CheckpointFileState::Present { .. }
                | CheckpointFileState::Absent
                | CheckpointFileState::Unrestorable { .. } => {}
            }
        }
        Ok(())
    }

    fn manifest_path(&self, session_id: &str, turn: u64) -> PathBuf {
        self.root
            .join("manifests")
            .join(session_id)
            .join(format!("{turn:020}.json"))
    }

    fn pending_path(&self, session_id: &str, turn: u64) -> PathBuf {
        self.root
            .join("pending")
            .join(session_id)
            .join(format!("{turn:020}.json"))
    }

    fn rewind_path(&self, session_id: &str) -> PathBuf {
        self.root.join("rewinds").join(format!("{session_id}.json"))
    }

    fn review_path(&self, session_id: &str) -> PathBuf {
        self.root.join("reviews").join(format!("{session_id}.json"))
    }
}

fn baseline_matches_current(baseline: &CheckpointFileState, current: &ReviewCurrentState) -> bool {
    match (baseline, current) {
        (
            CheckpointFileState::Present {
                blob,
                bytes,
                unix_mode,
            },
            ReviewCurrentState::Present {
                content_blake3,
                bytes: current_bytes,
                unix_mode: current_mode,
            },
        ) => blob == content_blake3 && bytes == current_bytes && unix_mode == current_mode,
        (CheckpointFileState::Absent, ReviewCurrentState::Absent) => true,
        (
            CheckpointFileState::Present { .. }
            | CheckpointFileState::Absent
            | CheckpointFileState::Unrestorable { .. },
            ReviewCurrentState::Present { .. }
            | ReviewCurrentState::Absent
            | ReviewCurrentState::Unsupported { .. },
        ) => false,
    }
}

fn review_identity(value: &impl Serialize) -> Result<String, CheckpointError> {
    Ok(blake3::hash(&serde_json::to_vec(value)?)
        .to_hex()
        .to_string())
}

fn validate_review_current(current: &ReviewCurrentState) -> Result<(), CheckpointError> {
    match current {
        ReviewCurrentState::Present {
            content_blake3,
            unix_mode,
            ..
        } if !is_lower_blake3(content_blake3) || unix_mode.is_some_and(|mode| mode > 0o7777) => {
            Err(CheckpointError::CorruptReviewLedger)
        }
        ReviewCurrentState::Unsupported { reason }
            if reason.is_empty()
                || reason.len() > 1_024
                || reason.chars().any(char::is_control) =>
        {
            Err(CheckpointError::CorruptReviewLedger)
        }
        ReviewCurrentState::Present { .. }
        | ReviewCurrentState::Absent
        | ReviewCurrentState::Unsupported { .. } => Ok(()),
    }
}

fn render_whole_file_diff(
    path: &str,
    original: Option<&[u8]>,
    current: Option<&[u8]>,
    limit: usize,
) -> (String, bool) {
    let original_text = original.map(std::str::from_utf8).transpose();
    let current_text = current.map(std::str::from_utf8).transpose();
    let (Ok(original_text), Ok(current_text)) = (original_text, current_text) else {
        let (message, _) = bounded_diff_text("Binary files differ\n", limit);
        return (message, true);
    };
    let escaped_path = path.escape_default().to_string();
    let original_header = original.map_or("/dev/null".to_owned(), |_| format!("a/{escaped_path}"));
    let current_header = current.map_or("/dev/null".to_owned(), |_| format!("b/{escaped_path}"));
    let mut config = similar::TextDiff::configure();
    config.timeout(std::time::Duration::from_millis(50));
    let diff = config.diff_lines(original_text.unwrap_or(""), current_text.unwrap_or(""));
    let output = diff
        .unified_diff()
        .context_radius(3)
        .header(&original_header, &current_header)
        .to_string();
    bounded_diff_text(&output, limit)
}

fn bounded_diff_text(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut boundary = limit.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

fn normalize_relative(path: &Path) -> Result<String, CheckpointError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CheckpointError::UnsafePath);
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or(CheckpointError::UnsafePath)?;
                if value.is_empty() {
                    return Err(CheckpointError::UnsafePath);
                }
                parts.push(value);
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return Err(CheckpointError::UnsafePath),
        }
    }
    if parts.is_empty() {
        return Err(CheckpointError::UnsafePath);
    }
    Ok(parts.join("/"))
}

fn validate_session_id(value: &str) -> Result<(), CheckpointError> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CheckpointError::InvalidSessionId);
    }
    Ok(())
}

fn validate_operation_id(value: &str) -> Result<(), CheckpointError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CheckpointError::InvalidOperationId);
    }
    Ok(())
}

fn changed_inventory_paths(
    before: &BTreeMap<String, InventoryEntry>,
    after: &BTreeMap<String, InventoryEntry>,
) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

fn is_lower_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_rewind_report(report: &RewindReport) -> Result<(), CheckpointError> {
    for path in report
        .restored
        .iter()
        .chain(&report.removed)
        .chain(report.unrestorable.keys())
    {
        if normalize_relative(Path::new(path))? != *path {
            return Err(CheckpointError::CorruptRewindTransaction);
        }
    }
    if report.unrestorable.values().any(|reason| {
        reason.is_empty() || reason.len() > 1_024 || reason.chars().any(char::is_control)
    }) {
        return Err(CheckpointError::CorruptRewindTransaction);
    }
    Ok(())
}

fn parse_exact_turn_filename(filename: &std::ffi::OsStr) -> Option<u64> {
    let filename = filename.to_str()?;
    if filename.len() != 25 || filename.as_bytes().get(20..) != Some(b".json") {
        return None;
    }
    let digits = &filename[..20];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn is_private_temporary(filename: &std::ffi::OsStr) -> bool {
    let Some(filename) = filename.to_str() else {
        return false;
    };
    let Some(body) = filename
        .strip_prefix(".rw-")
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((pid, nonce)) = body.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !nonce.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && nonce.bytes().all(|byte| byte.is_ascii_digit())
}

fn cleanup_stale_temporaries_in(directory: &Path) -> Result<(), CheckpointError> {
    if !directory.exists() {
        return Ok(());
    }
    let mut removed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if is_private_temporary(&entry.file_name()) {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path())?;
                removed = true;
            } else if metadata.is_file() || metadata.file_type().is_symlink() {
                fs::remove_file(entry.path())?;
                removed = true;
            }
        }
    }
    if removed {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn remove_durable(path: &Path) -> Result<(), CheckpointError> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn digest_os_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    blake3::hash(path.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(not(unix))]
fn digest_os_path(path: &Path) -> String {
    blake3::hash(path.as_os_str().to_string_lossy().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(unix)]
fn open_workspace_root(workspace: &Path) -> Result<std::os::fd::OwnedFd, CheckpointError> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| std::io::Error::from(error).into())
}

#[cfg(unix)]
fn open_confined_parent(
    workspace: &Path,
    key: &str,
    create: bool,
) -> Result<Option<(std::os::fd::OwnedFd, String)>, CheckpointError> {
    use rustix::fs::{Mode, OFlags};
    let normalized = normalize_relative(Path::new(key))?;
    if normalized != key {
        return Err(CheckpointError::UnsafePath);
    }
    let mut parts = key.split('/').collect::<Vec<_>>();
    let name = parts.pop().ok_or(CheckpointError::UnsafePath)?.to_owned();
    let mut directory = open_workspace_root(workspace)?;
    for part in parts {
        match rustix::fs::openat(
            &directory,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => directory = next,
            Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
            Err(rustix::io::Errno::NOENT) => {
                rustix::fs::mkdirat(&directory, part, Mode::from_raw_mode(0o700))
                    .map_err(std::io::Error::from)?;
                rustix::fs::fsync(&directory).map_err(std::io::Error::from)?;
                directory = rustix::fs::openat(
                    &directory,
                    part,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?;
            }
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
            }
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
    Ok(Some((directory, name)))
}

#[cfg(unix)]
fn capture_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedRegular>, CheckpointError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(None);
    };
    let descriptor = match rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::ISDIR) => {
            return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
        }
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    #[cfg(target_os = "linux")]
    let mode = Some(Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777);
    #[cfg(not(target_os = "linux"))]
    let mode = Some(u32::from(
        Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777,
    ));
    let mut file = File::from(descriptor);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((bytes, mode)))
}

#[cfg(unix)]
fn capture_review_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedReview>, CheckpointError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(None);
    };
    let descriptor = match rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::ISDIR) => {
            return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
        }
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    #[cfg(target_os = "linux")]
    let mode = Some(Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777);
    #[cfg(not(target_os = "linux"))]
    let mode = Some(u32::from(
        Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777,
    ));
    capture_review_open_file(File::from(descriptor), mode).map(Some)
}

fn capture_review_open_file(
    mut file: File,
    unix_mode: Option<u32>,
) -> Result<CapturedReview, CheckpointError> {
    let before = file.metadata()?;
    if before.len() > MAX_REVIEW_IDENTITY_SCAN_BYTES {
        return Err(CheckpointError::ReviewIdentityLimit);
    }
    let mut hasher = blake3::Hasher::new();
    let mut retained = Vec::new();
    let mut retain_content = true;
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count).map_err(|_| CheckpointError::CorruptManifest)?)
            .ok_or(CheckpointError::CorruptManifest)?;
        if bytes > MAX_REVIEW_IDENTITY_SCAN_BYTES {
            return Err(CheckpointError::ReviewIdentityLimit);
        }
        hasher.update(&buffer[..count]);
        if retain_content
            && retained
                .len()
                .checked_add(count)
                .is_some_and(|length| length <= MAX_REVIEW_FILE_BYTES)
        {
            retained.extend_from_slice(&buffer[..count]);
        } else {
            retain_content = false;
            retained.clear();
        }
    }
    let after = file.metadata()?;
    if !same_open_file_identity(&before, &after) || bytes != after.len() {
        return Err(CheckpointError::ReviewPathChanged);
    }
    Ok((
        ReviewCurrentState::Present {
            content_blake3: hasher.finalize().to_hex().to_string(),
            bytes,
            unix_mode,
        },
        retain_content.then_some(retained),
    ))
}

#[cfg(unix)]
fn same_open_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.size() == after.size()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
fn same_open_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.is_file() == after.is_file()
}

#[cfg(unix)]
fn restore_regular_confined(
    workspace: &Path,
    key: &str,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let (parent, name) =
        open_confined_parent(workspace, key, true)?.ok_or(CheckpointError::UnsafePath)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = format!(".rw-{}-{nonce}.tmp", std::process::id());
    let descriptor = rustix::fs::openat(
        &parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?;
    let mut file = File::from(descriptor);
    let result = (|| -> Result<(), CheckpointError> {
        file.write_all(bytes)?;
        file.flush()?;
        if let Some(mode) = unix_mode {
            #[cfg(target_os = "linux")]
            rustix::fs::fchmod(&file, Mode::from_raw_mode(mode)).map_err(std::io::Error::from)?;
            #[cfg(not(target_os = "linux"))]
            {
                let mode = u16::try_from(mode).map_err(|_| CheckpointError::CorruptManifest)?;
                rustix::fs::fchmod(&file, Mode::from_raw_mode(mode))
                    .map_err(std::io::Error::from)?;
            }
        }
        file.sync_all()?;
        rustix::fs::renameat(&parent, temporary.as_str(), &parent, name.as_str())
            .map_err(std::io::Error::from)?;
        rustix::fs::fsync(&parent).map_err(std::io::Error::from)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(unix)]
fn remove_file_or_symlink_confined(workspace: &Path, key: &str) -> Result<(), CheckpointError> {
    use rustix::fs::{AtFlags, FileType};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(());
    };
    let stat = match rustix::fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if !file_type.is_file() && !file_type.is_symlink() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    rustix::fs::unlinkat(&parent, name.as_str(), AtFlags::empty()).map_err(std::io::Error::from)?;
    rustix::fs::fsync(&parent).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn inventory_confined(
    workspace: &Path,
    storage_relative: Option<&str>,
) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
    let root = open_workspace_root(workspace)?;
    let mut inventory = BTreeMap::new();
    inventory_directory_fd(&root, "", storage_relative, &mut inventory)?;
    Ok(inventory)
}

#[cfg(unix)]
fn inventory_directory_fd(
    directory: &std::os::fd::OwnedFd,
    prefix: &str,
    storage_relative: Option<&str>,
    inventory: &mut BTreeMap<String, InventoryEntry>,
) -> Result<(), CheckpointError> {
    use std::os::unix::ffi::OsStrExt as _;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    let mut entries = rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| CheckpointError::UnsafePath)?;
        if name == "." || name == ".." || (prefix.is_empty() && name == ".git") {
            continue;
        }
        let key = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if storage_relative
            .is_some_and(|storage| key == storage || key.starts_with(&format!("{storage}/")))
        {
            continue;
        }
        let stat = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        if file_type.is_symlink() {
            let target = rustix::fs::readlinkat(directory, name, Vec::new())
                .map_err(std::io::Error::from)?;
            let target = Path::new(std::ffi::OsStr::from_bytes(target.to_bytes()));
            inventory.insert(
                key,
                InventoryEntry::Symlink {
                    target: digest_os_path(target),
                },
            );
        } else if file_type.is_dir() {
            let child = rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            inventory.insert(key.clone(), InventoryEntry::Directory);
            inventory_directory_fd(&child, &key, storage_relative, inventory)?;
        } else if file_type.is_file() {
            let descriptor = rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let current = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
            if !FileType::from_raw_mode(current.st_mode).is_file() {
                return Err(CheckpointError::UnsupportedFileKind(key));
            }
            let mut file = File::from(descriptor);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            inventory.insert(
                key,
                InventoryEntry::Regular {
                    digest: blake3::hash(&bytes).to_hex().to_string(),
                },
            );
        } else {
            return Err(CheckpointError::UnsupportedFileKind(key));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn capture_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedRegular>, CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Some((fs::read(path)?, None))),
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn capture_review_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedReview>, CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match OpenOptions::new().read(true).open(path) {
        Ok(file) if file.metadata()?.is_file() => capture_review_open_file(file, None).map(Some),
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn restore_regular_confined(
    workspace: &Path,
    key: &str,
    bytes: &[u8],
    _unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    atomic_replace(&checked_workspace_path_fallback(workspace, key)?, bytes)
}

#[cfg(not(unix))]
fn remove_file_or_symlink_confined(workspace: &Path, key: &str) -> Result<(), CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn inventory_confined(
    workspace: &Path,
    storage_relative: Option<&str>,
) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
    fn scan(
        workspace: &Path,
        directory: &Path,
        prefix: &Path,
        storage_relative: Option<&str>,
        output: &mut BTreeMap<String, InventoryEntry>,
    ) -> Result<(), CheckpointError> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if prefix.as_os_str().is_empty() && entry.file_name() == ".git" {
                continue;
            }
            let relative = prefix.join(entry.file_name());
            let key = normalize_relative(&relative)?;
            if storage_relative
                .is_some_and(|storage| key == storage || key.starts_with(&format!("{storage}/")))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                output.insert(
                    key,
                    InventoryEntry::Symlink {
                        target: digest_os_path(&fs::read_link(entry.path())?),
                    },
                );
            } else if metadata.is_dir() {
                output.insert(key, InventoryEntry::Directory);
                scan(
                    workspace,
                    &entry.path(),
                    &relative,
                    storage_relative,
                    output,
                )?;
            } else if metadata.is_file() {
                let bytes = capture_regular_confined(workspace, &key)?
                    .ok_or(CheckpointError::UnsafePath)?
                    .0;
                output.insert(
                    key,
                    InventoryEntry::Regular {
                        digest: blake3::hash(&bytes).to_hex().to_string(),
                    },
                );
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    scan(
        workspace,
        workspace,
        Path::new(""),
        storage_relative,
        &mut output,
    )?;
    Ok(output)
}

#[cfg(not(unix))]
fn checked_workspace_path_fallback(
    workspace: &Path,
    key: &str,
) -> Result<PathBuf, CheckpointError> {
    let normalized = normalize_relative(Path::new(key))?;
    if normalized != key {
        return Err(CheckpointError::UnsafePath);
    }
    let path = workspace.join(key);
    let mut current = workspace.to_owned();
    for part in key.split('/') {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    let parent = path.parent().ok_or(CheckpointError::UnsafePath)?;
    fs::create_dir_all(parent)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".rw-{}-{nonce}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> Result<(), CheckpointError> {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Checkpoint failure with no captured file contents in diagnostics.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// Session id is not a safe path component.
    #[error("checkpoint session id is invalid")]
    InvalidSessionId,
    /// Rewind operation ids are durable path/event identities.
    #[error("checkpoint rewind operation id is invalid")]
    InvalidOperationId,
    /// Workspace root is not a directory.
    #[error("checkpoint workspace is not a directory")]
    WorkspaceNotDirectory,
    /// A workspace-relative path attempted traversal or used non-Unicode bytes.
    #[error("checkpoint path is not a safe workspace-relative path")]
    UnsafePath,
    /// Only regular files can be captured/restored in M2.
    #[error("checkpoint path has an unsupported file kind: {0}")]
    UnsupportedFileKind(String),
    /// Manifest fields or identity are inconsistent.
    #[error("checkpoint manifest is corrupt")]
    CorruptManifest,
    /// Manifest version is not supported.
    #[error("unsupported checkpoint manifest version {0}")]
    UnsupportedManifestVersion(u16),
    /// Blob content or digest is inconsistent.
    #[error("checkpoint blob is missing or corrupt")]
    CorruptBlob,
    /// An opaque command already has a durable unfinished baseline.
    #[error("an opaque checkpoint mutation is already pending")]
    OpaqueMutationPending,
    /// A durable opaque-command baseline failed validation.
    #[error("opaque checkpoint mutation marker is corrupt")]
    CorruptOpaqueMutation,
    /// Git returned malformed tracked-file baseline data.
    #[error("opaque checkpoint Git baseline is corrupt")]
    GitBaselineCorrupt,
    /// Another rewind for this session must be recovered or acknowledged.
    #[error("a checkpoint rewind is already pending")]
    RewindPending,
    /// Durable rewind identity did not match the caller's handle.
    #[error("checkpoint rewind identity does not match")]
    RewindIdentityMismatch,
    /// A rewind must finish applying before it can be acknowledged.
    #[error("checkpoint rewind workspace is not committed")]
    RewindNotCommitted,
    /// Prepared state may be removed only before the first workspace step.
    #[error("checkpoint rewind cannot be discarded after workspace application begins")]
    RewindCannotDiscard,
    /// A durable rewind transaction failed validation.
    #[error("checkpoint rewind transaction is corrupt")]
    CorruptRewindTransaction,
    /// A durable review decision ledger failed validation.
    #[error("checkpoint review ledger is corrupt")]
    CorruptReviewLedger,
    /// A session touched more files than one bounded review can represent.
    #[error("checkpoint review exceeds its file limit")]
    ReviewFileLimit,
    /// A requested review path was not changed by this session.
    #[error("checkpoint review path is not available")]
    ReviewPathNotFound,
    /// A truncated or unrestorable review entry cannot be safely reverted.
    #[error("checkpoint review path cannot be safely reverted")]
    ReviewPathNotRevertible,
    /// The path changed after the review snapshot displayed to the driver.
    #[error("checkpoint review path changed after it was displayed")]
    ReviewPathChanged,
    /// A current file is too large to fingerprint within the review work bound.
    #[error("checkpoint review identity scan limit exceeded")]
    ReviewIdentityLimit,
    /// Parent and child session identities must differ.
    #[error("checkpoint fork identities conflict")]
    ForkIdentityConflict,
    /// Source and target checkpoint stores must bind the same workspace root.
    #[error("checkpoint fork workspace roots do not match")]
    ForkWorkspaceMismatch,
    /// A child checkpoint namespace already exists.
    #[error("checkpoint fork target already exists")]
    ForkTargetExists,
    /// Filesystem failure.
    #[error("checkpoint storage I/O failed")]
    Io(#[from] std::io::Error),
    /// JSON failure without captured contents.
    #[error("checkpoint manifest JSON is invalid")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        path::{Path, PathBuf},
        process::Command,
    };

    use rw_types::{ReviewFileDecision, ReviewFileStatus};
    use tempfile::tempdir;

    use super::{CheckpointFileState, CheckpointStore, RewindReport, render_whole_file_diff};

    fn rewind(
        store: &CheckpointStore,
        session_id: &str,
        target_turn: u64,
        operation_id: &str,
    ) -> RewindReport {
        let handle = store
            .prepare_rewind(session_id, target_turn, operation_id)
            .unwrap_or_else(|error| panic!("rewind must prepare: {error}"));
        let commit = store
            .apply_rewind(&handle)
            .unwrap_or_else(|error| panic!("rewind must apply: {error}"));
        store
            .acknowledge_rewind(&handle)
            .unwrap_or_else(|error| panic!("rewind must acknowledge: {error}"));
        commit.report
    }

    fn git(workspace: &std::path::Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(arguments)
            .output()
            .unwrap_or_else(|error| panic!("git must run: {error}"));
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn ten_edits_rewind_to_turn_three_byte_identically() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        let storage = root.path().join("store");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let path = workspace.join("counter.txt");
        fs::write(&path, b"turn-0\n")
            .unwrap_or_else(|error| panic!("initial file must write: {error}"));
        let store = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
        for turn in 1_u64..=10 {
            store
                .checkpoint_known("session", turn, [PathBuf::from("counter.txt")])
                .unwrap_or_else(|error| panic!("turn {turn} must checkpoint: {error}"));
            fs::write(&path, format!("turn-{turn}\n"))
                .unwrap_or_else(|error| panic!("turn {turn} must write: {error}"));
        }
        let expected = b"turn-3\n".to_vec();
        let report = rewind(&store, "session", 3, "rewind-3");
        assert_eq!(
            fs::read(path).unwrap_or_else(|error| panic!("rewound file must read: {error}")),
            expected
        );
        assert_eq!(report.restored.len(), 7);
        assert!(report.unrestorable.is_empty());
    }

    #[test]
    fn new_files_are_removed_and_unknown_shell_outputs_are_honest() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
        let mut manifest = store
            .checkpoint_known("session", 1, [PathBuf::from("created.txt")])
            .unwrap_or_else(|error| panic!("missing file must checkpoint: {error}"));
        fs::write(workspace.join("created.txt"), b"new")
            .unwrap_or_else(|error| panic!("new file must write: {error}"));
        fs::write(workspace.join("opaque.txt"), b"unknown")
            .unwrap_or_else(|error| panic!("opaque file must write: {error}"));
        store
            .mark_unrestorable(
                &mut manifest,
                [PathBuf::from("opaque.txt")],
                "created by opaque shell execution before its prior state was captured",
            )
            .unwrap_or_else(|error| panic!("unrestorable path must persist: {error}"));
        assert!(matches!(
            manifest.files["created.txt"],
            CheckpointFileState::Absent
        ));
        let report = rewind(&store, "session", 0, "rewind-0");
        assert!(!workspace.join("created.txt").exists());
        assert_eq!(
            fs::read(workspace.join("opaque.txt"))
                .unwrap_or_else(|error| panic!("opaque output must remain: {error}")),
            b"unknown"
        );
        assert!(report.unrestorable.contains_key("opaque.txt"));
    }

    #[test]
    fn repeated_mutations_in_one_turn_preserve_the_earliest_pre_state() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let file = workspace.join("file.txt");
        fs::write(&file, b"original").unwrap_or_else(|error| panic!("fixture must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));

        store
            .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
            .unwrap_or_else(|error| panic!("first mutation must checkpoint: {error}"));
        fs::write(&file, b"intermediate")
            .unwrap_or_else(|error| panic!("intermediate fixture must write: {error}"));
        store
            .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
            .unwrap_or_else(|error| panic!("second mutation must checkpoint: {error}"));
        fs::write(&file, b"final")
            .unwrap_or_else(|error| panic!("final fixture must write: {error}"));

        rewind(&store, "session", 0, "rewind-original");
        assert_eq!(
            fs::read(file).unwrap_or_else(|error| panic!("rewound file must read: {error}")),
            b"original"
        );
    }

    #[test]
    fn traversal_and_symlink_capture_fail_closed() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
        assert!(
            store
                .checkpoint_known("session", 1, [PathBuf::from("../escape")])
                .is_err()
        );
        fs::write(workspace.join("safe.txt"), b"safe")
            .unwrap_or_else(|error| panic!("safe fixture must write: {error}"));
        let mut manifest = store
            .checkpoint_known("corrupt", 1, [PathBuf::from("safe.txt")])
            .unwrap_or_else(|error| panic!("safe fixture must checkpoint: {error}"));
        manifest.files.insert(
            "safe.txt".to_owned(),
            CheckpointFileState::Present {
                blob: "../../outside".to_owned(),
                bytes: 4,
                unix_mode: None,
            },
        );
        let bytes = serde_json::to_vec(&manifest)
            .unwrap_or_else(|error| panic!("corrupt fixture must encode: {error}"));
        fs::write(store.manifest_path("corrupt", 1), bytes)
            .unwrap_or_else(|error| panic!("corrupt fixture must write: {error}"));
        assert!(store.load_manifest("corrupt", 1).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../outside", workspace.join("link"))
                .unwrap_or_else(|error| panic!("fixture symlink must create: {error}"));
            assert!(
                store
                    .checkpoint_known("session", 2, [PathBuf::from("link")])
                    .is_err()
            );
            fs::create_dir_all(root.path().join("outside"))
                .unwrap_or_else(|error| panic!("outside fixture must create: {error}"));
            std::os::unix::fs::symlink(root.path().join("outside"), workspace.join("parent-link"))
                .unwrap_or_else(|error| panic!("parent symlink must create: {error}"));
            assert!(
                store
                    .checkpoint_known("session", 3, [PathBuf::from("parent-link/escape.txt")])
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rewind_replaces_final_symlinks_without_touching_their_targets_and_restores_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let outside = root.path().join("outside.txt");
        fs::write(&outside, b"outside")
            .unwrap_or_else(|error| panic!("outside fixture must write: {error}"));
        let present = workspace.join("present.txt");
        fs::write(&present, b"original")
            .unwrap_or_else(|error| panic!("present fixture must write: {error}"));
        fs::set_permissions(&present, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("fixture mode must set: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known(
                "session",
                1,
                [PathBuf::from("present.txt"), PathBuf::from("absent.txt")],
            )
            .unwrap_or_else(|error| panic!("paths must checkpoint: {error}"));
        fs::remove_file(&present)
            .unwrap_or_else(|error| panic!("present fixture must remove: {error}"));
        std::os::unix::fs::symlink(&outside, &present)
            .unwrap_or_else(|error| panic!("replacement symlink must create: {error}"));
        std::os::unix::fs::symlink(&outside, workspace.join("absent.txt"))
            .unwrap_or_else(|error| panic!("new symlink must create: {error}"));

        rewind(&store, "session", 0, "symlink-rewind");
        assert_eq!(
            fs::read(&present).unwrap_or_else(|error| panic!("restored file must read: {error}")),
            b"original"
        );
        assert!(
            !fs::symlink_metadata(&present)
                .unwrap_or_else(|error| panic!("restored metadata must read: {error}"))
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&present)
                .unwrap_or_else(|error| panic!("restored mode must read: {error}"))
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert!(!workspace.join("absent.txt").exists());
        assert_eq!(
            fs::read(outside).unwrap_or_else(|error| panic!("outside must read: {error}")),
            b"outside"
        );
    }

    #[test]
    fn stale_private_manifest_temp_is_recovered_but_unrecognized_entries_fail_closed() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        let storage = root.path().join("store");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("file.txt"), b"before")
            .unwrap_or_else(|error| panic!("fixture must write: {error}"));
        let store = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
            .unwrap_or_else(|error| panic!("fixture must checkpoint: {error}"));
        let manifest_directory = store.root.join("manifests/session");
        fs::write(manifest_directory.join(".rw-123-7.tmp"), b"partial")
            .unwrap_or_else(|error| panic!("stale temp must write: {error}"));
        drop(store);
        let reopened = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must recover stale temp: {error}"));
        assert!(!manifest_directory.join(".rw-123-7.tmp").exists());
        fs::write(workspace.join("file.txt"), b"after")
            .unwrap_or_else(|error| panic!("mutation must write: {error}"));
        rewind(&reopened, "session", 0, "temp-rewind");
        assert_eq!(
            fs::read(workspace.join("file.txt"))
                .unwrap_or_else(|error| panic!("restored file must read: {error}")),
            b"before"
        );

        fs::write(manifest_directory.join("unexpected.json.bak"), b"junk")
            .unwrap_or_else(|error| panic!("junk entry must write: {error}"));
        assert!(
            reopened
                .prepare_rewind("session", 0, "junk-rewind")
                .is_err()
        );
    }

    #[test]
    fn rewind_prevalidates_every_blob_before_mutating_workspace() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("a.txt"), b"a-before")
            .unwrap_or_else(|error| panic!("a fixture must write: {error}"));
        fs::write(workspace.join("b.txt"), b"b-before")
            .unwrap_or_else(|error| panic!("b fixture must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        let manifest = store
            .checkpoint_known(
                "session",
                1,
                [PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            )
            .unwrap_or_else(|error| panic!("fixtures must checkpoint: {error}"));
        fs::write(workspace.join("a.txt"), b"a-after")
            .unwrap_or_else(|error| panic!("a mutation must write: {error}"));
        fs::write(workspace.join("b.txt"), b"b-after")
            .unwrap_or_else(|error| panic!("b mutation must write: {error}"));
        let CheckpointFileState::Present { blob, .. } = &manifest.files["b.txt"] else {
            panic!("b must have a blob")
        };
        fs::write(
            store.root.join("blobs").join(&blob[..2]).join(blob),
            b"corrupt",
        )
        .unwrap_or_else(|error| panic!("blob corruption must write: {error}"));
        assert!(
            store
                .prepare_rewind("session", 0, "corrupt-rewind")
                .is_err()
        );
        assert_eq!(
            fs::read(workspace.join("a.txt"))
                .unwrap_or_else(|error| panic!("a current file must read: {error}")),
            b"a-after"
        );
        assert_eq!(
            fs::read(workspace.join("b.txt"))
                .unwrap_or_else(|error| panic!("b current file must read: {error}")),
            b"b-after"
        );
    }

    #[test]
    fn rewind_recovers_idempotently_after_apply_before_progress_persist() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        let storage = root.path().join("store");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("file.txt"), b"before")
            .unwrap_or_else(|error| panic!("fixture must write: {error}"));
        let store = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
            .unwrap_or_else(|error| panic!("fixture must checkpoint: {error}"));
        fs::write(workspace.join("file.txt"), b"after")
            .unwrap_or_else(|error| panic!("mutation must write: {error}"));
        let handle = store
            .prepare_rewind("session", 0, "crash-rewind")
            .unwrap_or_else(|error| panic!("rewind must prepare: {error}"));
        let transaction = store
            .load_rewind_transaction("session")
            .unwrap_or_else(|error| panic!("transaction must load: {error}"));
        let mut discarded_report = RewindReport::default();
        store
            .restore_state(
                &transaction.steps[0].path,
                &transaction.steps[0].state,
                &mut discarded_report,
            )
            .unwrap_or_else(|error| panic!("first unrecorded apply must work: {error}"));
        drop(store);

        let reopened = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must reopen: {error}"));
        let recovered = reopened
            .recover_rewinds()
            .unwrap_or_else(|error| panic!("rewind must recover: {error}"));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].handle, handle);
        assert_eq!(
            fs::read(workspace.join("file.txt"))
                .unwrap_or_else(|error| panic!("restored file must read: {error}")),
            b"before"
        );
        assert_eq!(
            reopened
                .recover_rewinds()
                .unwrap_or_else(|error| panic!("committed recovery must repeat: {error}")),
            recovered
        );
        reopened
            .acknowledge_rewind(&handle)
            .unwrap_or_else(|error| panic!("recovered rewind must ack: {error}"));
        assert!(reopened.recover_rewinds().unwrap_or_default().is_empty());
    }

    #[test]
    fn opaque_git_baseline_restores_tracked_marks_unknown_and_removes_new() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        git(&workspace, &["init", "-q"]);
        fs::write(workspace.join("tracked.txt"), b"tracked-before")
            .unwrap_or_else(|error| panic!("tracked fixture must write: {error}"));
        git(&workspace, &["add", "tracked.txt"]);
        git(
            &workspace,
            &[
                "-c",
                "user.name=Rottweiler Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        fs::write(workspace.join("unknown.txt"), b"unknown-before")
            .unwrap_or_else(|error| panic!("unknown fixture must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        let mutation = store
            .begin_opaque_mutation("session", 1)
            .unwrap_or_else(|error| panic!("opaque baseline must begin: {error}"));
        fs::write(workspace.join("tracked.txt"), b"tracked-after")
            .unwrap_or_else(|error| panic!("tracked mutation must write: {error}"));
        fs::write(workspace.join("unknown.txt"), b"unknown-after")
            .unwrap_or_else(|error| panic!("unknown mutation must write: {error}"));
        fs::write(workspace.join("created.txt"), b"created")
            .unwrap_or_else(|error| panic!("created fixture must write: {error}"));
        let manifest = store
            .finish_opaque_mutation(&mutation)
            .unwrap_or_else(|error| panic!("opaque post-scan must finish: {error}"));
        assert!(matches!(
            manifest.files["tracked.txt"],
            CheckpointFileState::Present { .. }
        ));
        assert!(matches!(
            manifest.files["unknown.txt"],
            CheckpointFileState::Unrestorable { .. }
        ));
        assert!(matches!(
            manifest.files["created.txt"],
            CheckpointFileState::Absent
        ));

        let report = rewind(&store, "session", 0, "opaque-rewind");
        assert_eq!(
            fs::read(workspace.join("tracked.txt"))
                .unwrap_or_else(|error| panic!("tracked restore must read: {error}")),
            b"tracked-before"
        );
        assert_eq!(
            fs::read(workspace.join("unknown.txt"))
                .unwrap_or_else(|error| panic!("unknown result must read: {error}")),
            b"unknown-after"
        );
        assert!(!workspace.join("created.txt").exists());
        assert!(report.unrestorable.contains_key("unknown.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_git_dirty_query_snapshots_all_tracked_worktree_preimages() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        git(&workspace, &["init", "-q"]);
        let tracked = workspace.join("tracked.txt");
        fs::write(&tracked, b"index-version")
            .unwrap_or_else(|error| panic!("tracked fixture must write: {error}"));
        git(&workspace, &["add", "tracked.txt"]);
        git(
            &workspace,
            &[
                "-c",
                "user.name=Rottweiler Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        fs::write(&tracked, b"dirty-worktree-preimage")
            .unwrap_or_else(|error| panic!("dirty preimage must write: {error}"));

        let fake_git = root.path().join("fake-git");
        fs::write(
            &fake_git,
            br#"#!/bin/sh
workspace="$2"
shift 2
if [ "$1" = "diff" ]; then
  exit 73
fi
exec git -C "$workspace" "$@"
"#,
        )
        .unwrap_or_else(|error| panic!("fake git must write: {error}"));
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("fake git must be executable: {error}"));

        let store = CheckpointStore::open(&root.path().join("store"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"))
            .with_git_program(fake_git);
        let mutation = store
            .begin_opaque_mutation("session", 1)
            .unwrap_or_else(|error| panic!("failed diff must use conservative baseline: {error}"));
        fs::write(&tracked, b"agent-after")
            .unwrap_or_else(|error| panic!("agent mutation must write: {error}"));
        let manifest = store
            .finish_opaque_mutation(&mutation)
            .unwrap_or_else(|error| panic!("opaque mutation must finish: {error}"));
        assert!(matches!(
            manifest.files["tracked.txt"],
            CheckpointFileState::Present { .. }
        ));

        rewind(&store, "session", 0, "failed-diff-rewind");
        assert_eq!(
            fs::read(tracked)
                .unwrap_or_else(|error| panic!("restored dirty preimage must read: {error}")),
            b"dirty-worktree-preimage"
        );
    }

    #[cfg(unix)]
    #[test]
    fn opaque_recovery_does_not_follow_workspace_symlinks_and_rejects_corrupt_marker() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        let storage = root.path().join("store");
        let outside = root.path().join("outside");
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::create_dir_all(&outside).unwrap_or_else(|error| panic!("outside must create: {error}"));
        fs::write(outside.join("secret.txt"), b"secret")
            .unwrap_or_else(|error| panic!("outside secret must write: {error}"));
        std::os::unix::fs::symlink(&outside, workspace.join("link"))
            .unwrap_or_else(|error| panic!("symlink must create: {error}"));
        let store = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        let mutation = store
            .begin_opaque_mutation("session", 1)
            .unwrap_or_else(|error| panic!("opaque baseline must begin: {error}"));
        let pending = store
            .load_pending("session", 1)
            .unwrap_or_else(|error| panic!("pending marker must load: {error}"));
        assert!(pending.before.contains_key("link"));
        assert!(!pending.before.contains_key("link/secret.txt"));
        drop(store);

        let reopened = CheckpointStore::open(&storage, &workspace)
            .unwrap_or_else(|error| panic!("store must reopen: {error}"));
        assert_eq!(
            reopened
                .recover_opaque_mutations()
                .unwrap_or_else(|error| panic!("pending mutation must recover: {error}"))
                .len(),
            1
        );
        assert!(!reopened.pending_path("session", 1).exists());

        let second = reopened
            .begin_opaque_mutation("session", 2)
            .unwrap_or_else(|error| panic!("second baseline must begin: {error}"));
        let path = reopened.pending_path("session", 2);
        let mut value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("pending bytes must read: {error}")),
        )
        .unwrap_or_else(|error| panic!("pending JSON must decode: {error}"));
        value["before"]["link"]["target"] = serde_json::Value::String("../../outside".to_owned());
        fs::write(
            &path,
            serde_json::to_vec(&value)
                .unwrap_or_else(|error| panic!("corrupt pending must encode: {error}")),
        )
        .unwrap_or_else(|error| panic!("corrupt pending must write: {error}"));
        assert!(reopened.finish_opaque_mutation(&second).is_err());
        assert!(path.exists());
        assert_eq!(mutation.session_id, "session");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn cumulative_review_ten_edits_reverts_one_file_and_preserves_accepted_peer() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("alpha.txt"), b"alpha original\n")
            .unwrap_or_else(|error| panic!("alpha baseline must write: {error}"));
        fs::write(workspace.join("beta.txt"), b"beta original\n")
            .unwrap_or_else(|error| panic!("beta baseline must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));

        for turn in 1..=10_u64 {
            let (path, content) = if turn.is_multiple_of(2) {
                ("beta.txt", format!("beta edit {turn}\n"))
            } else {
                ("alpha.txt", format!("alpha edit {turn}\n"))
            };
            store
                .checkpoint_known("session", turn, [PathBuf::from(path)])
                .unwrap_or_else(|error| panic!("turn {turn} must checkpoint: {error}"));
            fs::write(workspace.join(path), content)
                .unwrap_or_else(|error| panic!("turn {turn} must edit: {error}"));
        }

        let review = store
            .session_review("session")
            .unwrap_or_else(|error| panic!("review must load: {error}"));
        assert_eq!(
            review
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["alpha.txt", "beta.txt"]
        );
        assert!(review.files[0].unified_diff.contains("-alpha original"));
        assert!(review.files[0].unified_diff.contains("+alpha edit 9"));
        assert!(review.files[1].unified_diff.contains("-beta original"));
        assert!(review.files[1].unified_diff.contains("+beta edit 10"));
        let beta_hash = review.files[1].current_hash.clone();

        let accepted = store
            .resolve_review_file(
                "session",
                Path::new("beta.txt"),
                ReviewFileDecision::Accept,
                &beta_hash,
            )
            .unwrap_or_else(|error| panic!("beta must accept: {error}"));
        assert_eq!(
            accepted
                .files
                .iter()
                .find(|file| file.path == "beta.txt")
                .map(|file| file.status),
            Some(ReviewFileStatus::Accepted)
        );
        let alpha_hash = accepted
            .files
            .iter()
            .find(|file| file.path == "alpha.txt")
            .map_or_else(
                || panic!("alpha review entry must remain"),
                |file| file.current_hash.clone(),
            );

        let reverted = store
            .resolve_review_file(
                "session",
                Path::new("alpha.txt"),
                ReviewFileDecision::Revert,
                &alpha_hash,
            )
            .unwrap_or_else(|error| panic!("alpha must revert: {error}"));
        assert_eq!(
            fs::read(workspace.join("alpha.txt"))
                .unwrap_or_else(|error| panic!("alpha result must read: {error}")),
            b"alpha original\n"
        );
        assert_eq!(
            fs::read(workspace.join("beta.txt"))
                .unwrap_or_else(|error| panic!("beta result must read: {error}")),
            b"beta edit 10\n"
        );
        assert_eq!(
            reverted
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.status))
                .collect::<Vec<_>>(),
            [
                ("alpha.txt", ReviewFileStatus::Reverted),
                ("beta.txt", ReviewFileStatus::Accepted),
            ]
        );

        fs::write(
            workspace.join("beta.txt"),
            b"beta changed after acceptance\n",
        )
        .unwrap_or_else(|error| panic!("post-accept edit must write: {error}"));
        assert!(matches!(
            store.resolve_review_file(
                "session",
                Path::new("beta.txt"),
                ReviewFileDecision::Accept,
                &beta_hash,
            ),
            Err(super::CheckpointError::ReviewPathChanged)
        ));
        let changed = store
            .session_review("session")
            .unwrap_or_else(|error| panic!("changed review must load: {error}"));
        assert_eq!(
            changed
                .files
                .iter()
                .find(|file| file.path == "beta.txt")
                .map(|file| file.status),
            Some(ReviewFileStatus::Pending)
        );
    }

    #[test]
    fn review_diff_has_minimal_context_and_handles_file_edge_cases() {
        let original = b"one\ntwo\nthree\nfour\nfive\n";
        let current = b"one\ntwo\nTHREE\nfour\nfive\n";
        let (edited, truncated) =
            render_whole_file_diff("file.txt", Some(original), Some(current), 16 * 1024);
        assert!(!truncated);
        assert!(edited.contains(" two\n-three\n+THREE\n four\n"));
        assert!(!edited.contains("-one\n"));
        assert!(!edited.contains("+one\n"));

        let (deleted, truncated) =
            render_whole_file_diff("file.txt", Some(b"gone\n"), None, 16 * 1024);
        assert!(!truncated);
        assert!(deleted.contains("+++ /dev/null"));
        assert!(deleted.contains("-gone"));

        let (created, truncated) =
            render_whole_file_diff("new.txt", None, Some(b"new\n"), 16 * 1024);
        assert!(!truncated);
        assert!(created.contains("--- /dev/null"));
        assert!(created.contains("+new"));

        let (no_newline, truncated) =
            render_whole_file_diff("plain.txt", Some(b"before"), Some(b"after"), 16 * 1024);
        assert!(!truncated);
        assert!(no_newline.contains("\\ No newline at end of file"));

        let (binary, truncated) =
            render_whole_file_diff("binary.dat", Some(&[0xff, 0]), Some(&[0xfe, 0]), 16 * 1024);
        assert!(truncated);
        assert_eq!(binary, "Binary files differ\n");
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_symlink_target_swaps_cannot_be_accepted_or_reverted() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("review.txt"), b"baseline\n")
            .unwrap_or_else(|error| panic!("baseline must write: {error}"));
        fs::write(workspace.join("first.txt"), b"first\n")
            .unwrap_or_else(|error| panic!("first target must write: {error}"));
        fs::write(workspace.join("second.txt"), b"second\n")
            .unwrap_or_else(|error| panic!("second target must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known("symlink-session", 1, [PathBuf::from("review.txt")])
            .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
        fs::remove_file(workspace.join("review.txt"))
            .unwrap_or_else(|error| panic!("baseline must remove: {error}"));
        symlink("first.txt", workspace.join("review.txt"))
            .unwrap_or_else(|error| panic!("first symlink must create: {error}"));
        let first = store
            .session_review("symlink-session")
            .unwrap_or_else(|error| panic!("first review must load: {error}"));
        assert!(first.files[0].unrestorable_reason.is_some());
        let first_hash = first.files[0].current_hash.clone();
        assert!(matches!(
            store.resolve_review_file(
                "symlink-session",
                Path::new("review.txt"),
                ReviewFileDecision::Accept,
                &first_hash,
            ),
            Err(super::CheckpointError::ReviewPathNotRevertible)
        ));
        fs::remove_file(workspace.join("review.txt"))
            .unwrap_or_else(|error| panic!("first symlink must remove: {error}"));
        symlink("second.txt", workspace.join("review.txt"))
            .unwrap_or_else(|error| panic!("second symlink must create: {error}"));
        let second = store
            .session_review("symlink-session")
            .unwrap_or_else(|error| panic!("second review must load: {error}"));
        assert_eq!(second.files[0].status, ReviewFileStatus::Pending);
        assert!(matches!(
            store.resolve_review_file(
                "symlink-session",
                Path::new("review.txt"),
                ReviewFileDecision::Revert,
                &second.files[0].current_hash,
            ),
            Err(super::CheckpointError::ReviewPathNotRevertible)
        ));
    }

    #[test]
    fn oversized_review_streams_identity_and_remains_safely_revertible() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let path = workspace.join("large.bin");
        fs::write(&path, b"small baseline\n")
            .unwrap_or_else(|error| panic!("baseline must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known("large-session", 1, [PathBuf::from("large.bin")])
            .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("large fixture must open: {error}"));
        file.set_len(8 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("large fixture must resize: {error}"));

        let review = store
            .session_review("large-session")
            .unwrap_or_else(|error| panic!("large review must stream: {error}"));
        assert_eq!(review.files.len(), 1);
        assert!(review.files[0].truncated);
        assert!(review.files[0].unrestorable_reason.is_none());
        let current_hash = review.files[0].current_hash.clone();
        store
            .resolve_review_file(
                "large-session",
                Path::new("large.bin"),
                ReviewFileDecision::Revert,
                &current_hash,
            )
            .unwrap_or_else(|error| panic!("truncated review must revert: {error}"));
        assert_eq!(
            fs::read(path).unwrap_or_else(|error| panic!("reverted file must read: {error}")),
            b"small baseline\n"
        );
    }

    #[test]
    fn huge_sparse_review_is_bounded_and_marked_unreviewable() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
        let path = workspace.join("huge.bin");
        fs::write(&path, b"small baseline\n")
            .unwrap_or_else(|error| panic!("baseline must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        store
            .checkpoint_known("huge-session", 1, [PathBuf::from("huge.bin")])
            .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("sparse fixture must open: {error}"))
            .set_len(128 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("sparse fixture must resize: {error}"));
        let started = std::time::Instant::now();
        let review = store
            .session_review("huge-session")
            .unwrap_or_else(|error| panic!("bounded review must load: {error}"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(review.files.len(), 1);
        assert!(review.files[0].unrestorable_reason.is_some());
    }

    #[test]
    fn checkpoint_fork_rebinds_child_manifests_without_changing_parent() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
        fs::write(workspace.join("file.txt"), b"zero\n")
            .unwrap_or_else(|error| panic!("baseline must write: {error}"));
        let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
            .unwrap_or_else(|error| panic!("store must open: {error}"));
        for turn in 1..=3_u64 {
            store
                .checkpoint_known("parent", turn, [PathBuf::from("file.txt")])
                .unwrap_or_else(|error| panic!("parent checkpoint must write: {error}"));
            fs::write(workspace.join("file.txt"), format!("{turn}\n"))
                .unwrap_or_else(|error| panic!("parent edit must write: {error}"));
        }
        let parent_before = fs::read(store.manifest_path("parent", 1))
            .unwrap_or_else(|error| panic!("parent manifest must read: {error}"));

        let child_store = CheckpointStore::open(&root.path().join("child-storage"), &workspace)
            .unwrap_or_else(|error| panic!("child store must open: {error}"));
        store
            .fork_into(&child_store, "parent", "child", Some(2))
            .unwrap_or_else(|error| panic!("checkpoint fork must succeed: {error}"));
        assert_eq!(
            fs::read(store.manifest_path("parent", 1))
                .unwrap_or_else(|error| panic!("parent manifest must reread: {error}")),
            parent_before
        );
        assert_eq!(
            child_store
                .load_manifest("child", 1)
                .unwrap_or_else(|error| panic!("child manifest one must load: {error}"))
                .session_id,
            "child"
        );
        assert!(child_store.load_manifest("child", 2).is_ok());
        assert!(child_store.load_manifest("child", 3).is_err());
        assert!(matches!(
            store.fork_into(&child_store, "parent", "child", None),
            Err(super::CheckpointError::ForkTargetExists)
        ));
    }
}
