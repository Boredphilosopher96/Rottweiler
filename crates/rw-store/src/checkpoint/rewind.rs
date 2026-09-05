use std::{
    fs::{self},
    path::{Path, PathBuf},
};

use super::{
    CheckpointError, CheckpointFileState, CheckpointStore, REWIND_TRANSACTION_VERSION,
    RewindCommit, RewindHandle, RewindPhase, RewindReport, RewindStep, RewindTransaction,
    atomic_replace, cleanup_stale_temporaries_in, is_lower_blake3, normalize_relative,
    remove_durable, validate_operation_id, validate_rewind_report, validate_session_id,
};

impl CheckpointStore {
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

    pub(super) fn build_rewind_steps(
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

    pub(super) fn validate_rewind_steps(
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

    pub(super) fn load_rewind_transaction(
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

    pub(super) fn write_rewind_transaction(
        &self,
        transaction: &RewindTransaction,
    ) -> Result<(), CheckpointError> {
        atomic_replace(
            &self.rewind_path(&transaction.handle.session_id),
            &serde_json::to_vec(transaction)?,
        )
    }

    pub(super) fn enumerate_rewinds(&self) -> Result<Vec<RewindHandle>, CheckpointError> {
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

    pub(super) fn rewind_path(&self, session_id: &str) -> PathBuf {
        self.root.join("rewinds").join(format!("{session_id}.json"))
    }
}
