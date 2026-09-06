use super::{
    CheckpointOperation, TEMP_COUNTER,
    operation::{read_metadata, serialize_metadata},
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
};

use super::{
    CheckpointError, CheckpointFileState, CheckpointManifest, CheckpointStore, GitDirtyPaths,
    GitTrackedBaseline, GitTrackedEntry, InventoryEntry, MANIFEST_VERSION, MAX_CAPTURE_FILE_BYTES,
    OPAQUE_PENDING_VERSION, OpaqueMutation, OpaquePending, RewindReport, atomic_replace,
    capture_regular_confined, changed_inventory_paths, inventory_confined, is_lower_blake3,
    is_private_temporary, normalize_relative, parse_exact_turn_filename, remove_durable,
    remove_file_or_symlink_confined, restore_regular_confined, same_open_file_identity,
    validate_session_id,
};

impl CheckpointStore {
    /// Canonical workspace root whose paths this store snapshots and restores.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace
    }

    /// Creates a checkpoint store under `namespace_root/checkpoints`.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace cannot be canonicalized or storage
    /// directories cannot be created.
    pub fn open(
        namespace_root: &Path,
        workspace: &Path,
        blobs: std::sync::Arc<super::CheckpointBlobStore>,
    ) -> Result<Self, CheckpointError> {
        let workspace = fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(CheckpointError::WorkspaceNotDirectory);
        }
        let root = std::path::absolute(namespace_root)?.join("checkpoints");
        blobs.validate_workspace(&workspace)?;
        if root.join("blobs").exists() && fs::read_dir(root.join("blobs"))?.next().is_some() {
            return Err(CheckpointError::UnexpectedBlobDirectory);
        }
        super::create_directory_durable(&root.join("manifests"))?;
        super::create_directory_durable(&root.join("pending"))?;
        super::create_directory_durable(&root.join("rewinds"))?;
        super::create_directory_durable(&root.join("reviews"))?;
        let root = fs::canonicalize(root)?;
        let mut storage_relative = Vec::new();
        for path in [root.as_path(), blobs.storage_path()] {
            if let Ok(relative) = path.strip_prefix(&workspace)
                && !relative.as_os_str().is_empty()
            {
                storage_relative.push(normalize_relative(relative)?);
            }
        }
        Ok(Self {
            root,
            workspace,
            storage_relative,
            git_program: PathBuf::from("git"),
            blobs,
        })
    }

    /// Captures the known pre-mutation state of the supplied relative paths.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe path, unreadable file, unsupported file
    /// kind, or durable blob/manifest write failure.
    #[tracing::instrument(
        target = "rw_performance",
        level = "trace",
        name = "checkpoint.capture",
        skip_all,
        fields(session_id, turn)
    )]
    pub fn checkpoint_known(
        &self,
        session_id: &str,
        turn: u64,
        relative_paths: impl IntoIterator<Item = PathBuf>,
        operation: &mut CheckpointOperation,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(session_id)?;
        let mut blobs = self.blobs.begin(&self.root, operation)?;
        let path = self.manifest_path(session_id, turn);
        let mut files = if path.exists() {
            self.load_manifest_in(session_id, turn, operation)?.files
        } else {
            BTreeMap::new()
        };
        for relative in relative_paths {
            let key = normalize_relative(&relative)?;
            operation.path(&key)?;
            if let Entry::Vacant(entry) = files.entry(key) {
                let key = entry.key().clone();
                let state = self.capture(&key, operation, &mut blobs)?;
                entry.insert(state);
            }
        }
        let manifest = self.persist_manifest(session_id, turn, files)?;
        blobs.finish()?;
        Ok(manifest)
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
        let mut operation = CheckpointOperation::default();
        let blobs = self.blobs.begin(&self.root, &mut operation)?;
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
        self.write_manifest(manifest)?;
        blobs.finish()
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
        self.load_manifest_in(session_id, turn, &mut CheckpointOperation::default())
    }

    pub(super) fn load_manifest_in(
        &self,
        session_id: &str,
        turn: u64,
        operation: &mut CheckpointOperation,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(session_id)?;
        let bytes = operation.read_metadata(&self.manifest_path(session_id, turn))?;
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
    #[tracing::instrument(
        target = "rw_performance",
        level = "trace",
        name = "checkpoint.begin_opaque",
        skip_all,
        fields(session_id, turn)
    )]
    pub fn begin_opaque_mutation(
        &self,
        session_id: &str,
        turn: u64,
        operation: &mut CheckpointOperation,
    ) -> Result<OpaqueMutation, CheckpointError> {
        validate_session_id(session_id)?;
        let pending_path = self.pending_path(session_id, turn);
        if pending_path.exists() {
            return Err(CheckpointError::OpaqueMutationPending);
        }
        let mut blobs = self.blobs.begin(&self.root, operation)?;
        let before = self.workspace_inventory(operation)?;
        let tracked = self.git_tracked_baseline(operation)?;
        let dirty = self.git_dirty_tracked_paths(operation)?;
        let dirty = if tracked.complete && !dirty.complete {
            tracked.paths.clone()
        } else {
            dirty.paths
        };
        let mut files = self
            .load_manifest_if_exists(session_id, turn)?
            .map_or_else(BTreeMap::new, |manifest| manifest.files);
        for path in dirty {
            operation.path(&path)?;
            if let Entry::Vacant(entry) = files.entry(path.clone()) {
                let state = match self.capture(&path, operation, &mut blobs) {
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
        let bytes = serialize_metadata(&pending, false)?;
        operation.check()?;
        atomic_replace(&pending_path, &bytes)?;
        blobs.finish()?;
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
    #[tracing::instrument(target = "rw_performance", level = "trace", name = "checkpoint.finish_opaque", skip_all, fields(session_id = mutation.session_id.as_str(), turn = mutation.turn))]
    pub fn finish_opaque_mutation(
        &self,
        mutation: &OpaqueMutation,
        operation: &mut CheckpointOperation,
    ) -> Result<CheckpointManifest, CheckpointError> {
        validate_session_id(&mutation.session_id)?;
        let mut blobs = self.blobs.begin(&self.root, operation)?;
        let pending = self.load_pending(&mutation.session_id, mutation.turn)?;
        let after = self.workspace_inventory(operation)?;
        let mut manifest = self
            .load_manifest_if_exists(&mutation.session_id, mutation.turn)?
            .ok_or(CheckpointError::CorruptManifest)?;
        let changed = changed_inventory_paths(&pending.before, &after);
        for path in changed {
            operation.path(&path)?;
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
                self.capture_git_preimage(tracked, operation, &mut blobs)?.unwrap_or_else(|| {
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
        blobs.finish()?;
        Ok(manifest)
    }

    /// Completes every durable opaque post-scan left by a killed process.
    ///
    /// # Errors
    ///
    /// Returns an error rather than silently discarding an invalid marker.
    pub fn recover_opaque_mutations(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<usize, CheckpointError> {
        let mut pending = self.enumerate_pending(operation)?;
        pending.sort_by(|left, right| {
            (&left.session_id, left.turn).cmp(&(&right.session_id, right.turn))
        });
        let count = pending.len();
        for mutation in pending {
            self.finish_opaque_mutation(&mutation, operation)?;
        }
        Ok(count)
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
        let mut operation = CheckpointOperation::default();
        let mut blobs = target.blobs.begin(&target.root, &mut operation)?;
        let manifests_directory = target.root.join("manifests");
        let child_directory = manifests_directory.join(child_session_id);
        let mut turns = self.manifest_turns(parent_session_id, &mut operation)?;
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
        let result: Result<(), CheckpointError> = (|| {
            for turn in turns {
                let mut manifest =
                    self.load_manifest_in(parent_session_id, turn, &mut operation)?;
                for state in manifest.files.values() {
                    if let CheckpointFileState::Present { blob, bytes, .. } = state {
                        let content = self.read_valid_blob(blob, *bytes)?;
                        let captured =
                            blobs.capture(&mut content.as_slice(), None, &mut operation)?;
                        if !matches!(captured, CheckpointFileState::Present { blob: ref actual, .. } if actual == blob)
                        {
                            return Err(CheckpointError::CorruptBlob);
                        }
                    }
                }
                child_session_id.clone_into(&mut manifest.session_id);
                Self::validate_manifest(&manifest, child_session_id, turn)?;
                atomic_replace(
                    &staging.join(format!("{turn:020}.json")),
                    &serialize_metadata(&manifest, true)?,
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
        result?;
        blobs.finish()
    }

    pub(super) fn capture(
        &self,
        key: &str,
        operation: &mut CheckpointOperation,
        blobs: &mut super::blob_store::BlobWriteGuard<'_>,
    ) -> Result<CheckpointFileState, CheckpointError> {
        let Some((mut file, unix_mode)) = capture_regular_confined(&self.workspace, key)? else {
            return Ok(CheckpointFileState::Absent);
        };
        let before = file.metadata()?;
        if before.len() > MAX_CAPTURE_FILE_BYTES {
            return Err(CheckpointError::CaptureFileLimit);
        }
        let state = blobs.capture(&mut file, unix_mode, operation)?;
        if !same_open_file_identity(&before, &file.metadata()?) {
            return Err(CheckpointError::CaptureChanged);
        }
        Ok(state)
    }

    pub(super) fn capture_git_preimage(
        &self,
        tracked: &GitTrackedEntry,
        operation: &mut CheckpointOperation,
        blobs: &mut super::blob_store::BlobWriteGuard<'_>,
    ) -> Result<Option<CheckpointFileState>, CheckpointError> {
        let Some(unix_mode) = tracked.unix_mode else {
            return Ok(None);
        };
        operation.check()?;
        let mut pipe = match super::git::GitPipe::spawn(
            self.git_command().arg("-C").arg(&self.workspace).args([
                "cat-file",
                "blob",
                &tracked.object_id,
            ]),
            operation,
        ) {
            Ok(pipe) => pipe,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let result = blobs.capture(&mut pipe, Some(unix_mode), operation);
        operation.check()?;
        let state = result?;
        Ok(pipe.finish()?.then_some(state))
    }

    pub(super) fn persist_manifest(
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

    pub(super) fn write_manifest(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<(), CheckpointError> {
        Self::validate_manifest(manifest, &manifest.session_id, manifest.turn)?;
        let bytes = serialize_metadata(manifest, true)?;
        atomic_replace(
            &self.manifest_path(&manifest.session_id, manifest.turn),
            &bytes,
        )
    }

    pub(super) fn restore_state(
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

    pub(super) fn validate_blob(
        &self,
        blob: &str,
        bytes: u64,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        if !is_lower_blake3(blob) || bytes > MAX_CAPTURE_FILE_BYTES {
            return Err(CheckpointError::CorruptBlob);
        }
        let file = File::open(self.blobs.directory().join(&blob[..2]).join(blob))?;
        if file.metadata()?.len() != bytes
            || operation.hash(file.take(bytes + 1))?.to_hex().as_str() != blob
        {
            return Err(CheckpointError::CorruptBlob);
        }
        Ok(())
    }

    pub(super) fn read_valid_blob(
        &self,
        blob: &str,
        bytes: u64,
    ) -> Result<Vec<u8>, CheckpointError> {
        if !is_lower_blake3(blob) || bytes > MAX_CAPTURE_FILE_BYTES {
            return Err(CheckpointError::CorruptBlob);
        }
        let prefix = blob.get(..2).ok_or(CheckpointError::CorruptBlob)?;
        let file = File::open(self.blobs.directory().join(prefix).join(blob))?;
        if file.metadata()?.len() != bytes {
            return Err(CheckpointError::CorruptBlob);
        }
        let mut content = Vec::new();
        file.take(bytes + 1).read_to_end(&mut content)?;
        if u64::try_from(content.len()).map_err(|_| CheckpointError::CorruptBlob)? != bytes
            || blake3::hash(&content).to_hex().as_str() != blob
        {
            return Err(CheckpointError::CorruptBlob);
        }
        Ok(content)
    }

    pub(super) fn load_manifest_if_exists(
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

    pub(super) fn workspace_inventory(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
        inventory_confined(&self.workspace, &self.storage_relative, operation)
    }

    pub(super) fn git_tracked_baseline(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<GitTrackedBaseline, CheckpointError> {
        let Some(output) = super::git::query(
            self.git_command()
                .arg("-C")
                .arg(&self.workspace)
                .args(["ls-files", "--cached", "--stage", "-z", "--", "."]),
            operation,
        )?
        else {
            return Ok(GitTrackedBaseline::default());
        };
        let mut baseline = GitTrackedBaseline {
            complete: true,
            ..GitTrackedBaseline::default()
        };
        for record in output.split(|byte| *byte == 0) {
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
            operation.path(&key)?;
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

    pub(super) fn git_dirty_tracked_paths(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<GitDirtyPaths, CheckpointError> {
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
            let Some(output) = super::git::query(
                self.git_command().arg("-C").arg(&self.workspace).args(args),
                operation,
            )?
            else {
                query.complete = false;
                continue;
            };
            for path in output.split(|byte| *byte == 0) {
                if path.is_empty() {
                    continue;
                }
                let path = std::str::from_utf8(path).map_err(|_| CheckpointError::UnsafePath)?;
                operation.path(path)?;
                query.paths.insert(normalize_relative(Path::new(path))?);
            }
        }
        Ok(query)
    }

    pub(super) fn git_command(&self) -> Command {
        Command::new(&self.git_program)
    }

    #[cfg(test)]
    pub(super) fn with_git_program(mut self, program: PathBuf) -> Self {
        self.git_program = program;
        self
    }

    pub(super) fn load_pending(
        &self,
        session_id: &str,
        turn: u64,
    ) -> Result<OpaquePending, CheckpointError> {
        let pending: OpaquePending =
            serde_json::from_slice(&read_metadata(&self.pending_path(session_id, turn))?)?;
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

    pub(super) fn enumerate_pending(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<Vec<OpaqueMutation>, CheckpointError> {
        let root = self.root.join("pending");
        let mut mutations = Vec::new();
        for session in fs::read_dir(&root)? {
            operation.check()?;
            let session = session?;
            if is_private_temporary(&session.file_name()) {
                continue;
            }
            let metadata = fs::symlink_metadata(session.path())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(CheckpointError::CorruptOpaqueMutation);
            }
            let session_id = session
                .file_name()
                .into_string()
                .map_err(|_| CheckpointError::CorruptOpaqueMutation)?;
            validate_session_id(&session_id)?;
            for entry in fs::read_dir(session.path())? {
                let entry = entry?;
                if is_private_temporary(&entry.file_name()) {
                    continue;
                }
                let turn = parse_exact_turn_filename(&entry.file_name())
                    .ok_or(CheckpointError::CorruptOpaqueMutation)?;
                if !fs::symlink_metadata(entry.path())?.is_file() {
                    return Err(CheckpointError::CorruptOpaqueMutation);
                }
                operation.path(&format!("{session_id}/{turn}"))?;
                mutations.push(OpaqueMutation {
                    session_id: session_id.clone(),
                    turn,
                });
            }
        }
        Ok(mutations)
    }

    pub(super) fn manifest_turns(
        &self,
        session_id: &str,
        operation: &mut CheckpointOperation,
    ) -> Result<Vec<u64>, CheckpointError> {
        let directory = self.root.join("manifests").join(session_id);
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let mut turns = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if is_private_temporary(&entry.file_name()) {
                continue;
            }
            if !fs::symlink_metadata(entry.path())?.is_file() {
                return Err(CheckpointError::CorruptManifest);
            }
            operation.path(&entry.path().to_string_lossy())?;
            operation.retain_read::<u64>(0)?;
            turns.push(
                parse_exact_turn_filename(&entry.file_name())
                    .ok_or(CheckpointError::CorruptManifest)?,
            );
        }
        Ok(turns)
    }

    pub(super) fn validate_manifest(
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
        let mut allowance = CheckpointOperation::default();
        for (key, state) in &manifest.files {
            allowance.path(key)?;
            let normalized = normalize_relative(Path::new(key))?;
            if normalized != *key {
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

    pub(super) fn manifest_path(&self, session_id: &str, turn: u64) -> PathBuf {
        self.root
            .join("manifests")
            .join(session_id)
            .join(format!("{turn:020}.json"))
    }

    pub(super) fn pending_path(&self, session_id: &str, turn: u64) -> PathBuf {
        self.root
            .join("pending")
            .join(session_id)
            .join(format!("{turn:020}.json"))
    }
}
