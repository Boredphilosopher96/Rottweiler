use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rw_types::{ReviewFileDecision, ReviewFileStatus, SessionId, SessionReview, SessionReviewFile};

use super::{
    CheckpointError, CheckpointFileState, CheckpointOperation, CheckpointStore,
    MAX_REVIEW_FILE_BYTES, MAX_REVIEW_FILES, MAX_REVIEW_TOTAL_DIFF_BYTES, REVIEW_LEDGER_VERSION,
    ReviewCurrentState, ReviewDecisionRecord, ReviewLedger, RewindReport, atomic_replace,
    baseline_matches_current, capture_review_regular_confined, normalize_relative,
    render_whole_file_diff, review_identity, validate_review_current, validate_session_id,
};

impl CheckpointStore {
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
        self.session_review_in(session_id, &mut CheckpointOperation::default())
    }

    fn session_review_in(
        &self,
        session_id: &str,
        operation: &mut CheckpointOperation,
    ) -> Result<SessionReview, CheckpointError> {
        validate_session_id(session_id)?;
        let baselines = self.cumulative_baselines(session_id, operation)?;
        let ledger = self.load_review_ledger(session_id, operation)?;
        let mut remaining_diff_bytes = MAX_REVIEW_TOTAL_DIFF_BYTES;
        for _ in 0..baselines.len() {
            operation.retain_read::<SessionReviewFile>(128)?;
        }
        let mut files = Vec::with_capacity(baselines.len());
        for (path, baseline) in baselines {
            operation.path(&path)?;
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
            operation.retain_read::<String>(unified_diff.capacity())?;
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
        let mut operation = CheckpointOperation::default();
        validate_session_id(session_id)?;
        let path = normalize_relative(relative_path)?;
        let before = self.session_review_in(session_id, &mut operation)?;
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
        let baselines = self.cumulative_baselines(session_id, &mut operation)?;
        let baseline = baselines
            .get(&path)
            .ok_or(CheckpointError::ReviewPathNotFound)?;
        let (current_before_decision, _) = self.capture_review_current(&path)?;
        if review_identity(&current_before_decision)? != expected_current_hash {
            return Err(CheckpointError::ReviewPathChanged);
        }
        let mut ledger = self.load_review_ledger(session_id, &mut operation)?;
        if !ledger.files.contains_key(&path) && ledger.files.len() >= MAX_REVIEW_FILES {
            return Err(CheckpointError::ReviewFileLimit);
        }
        operation
            .retain_read::<(String, ReviewDecisionRecord)>(path.capacity().saturating_add(1024))?;
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
        ledger
            .files
            .insert(path, ReviewDecisionRecord { decision, current });
        self.write_review_ledger(&ledger)?;
        self.session_review_in(session_id, &mut operation)
    }

    pub(super) fn cumulative_baselines(
        &self,
        session_id: &str,
        operation: &mut CheckpointOperation,
    ) -> Result<BTreeMap<String, CheckpointFileState>, CheckpointError> {
        let mut turns = self.manifest_turns(session_id, operation)?;
        turns.sort_unstable();
        let mut baselines = BTreeMap::new();
        for turn in turns {
            for (path, state) in self.load_manifest_in(session_id, turn, operation)?.files {
                if path.chars().any(char::is_control) {
                    return Err(CheckpointError::UnsafePath);
                }
                operation.path(&path)?;
                if baselines.contains_key(&path) {
                    continue;
                }
                if baselines.len() >= MAX_REVIEW_FILES {
                    return Err(CheckpointError::ReviewFileLimit);
                }
                operation.retain_state::<(String, CheckpointFileState)>(path.capacity(), &state)?;
                baselines.insert(path, state);
            }
        }
        Ok(baselines)
    }

    pub(super) fn capture_review_current(
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

    pub(super) fn render_review_diff(
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

    pub(super) fn load_review_ledger(
        &self,
        session_id: &str,
        operation: &mut CheckpointOperation,
    ) -> Result<ReviewLedger, CheckpointError> {
        let path = self.review_path(session_id);
        let bytes = match operation.read_metadata(&path) {
            Ok(bytes) => bytes,
            Err(CheckpointError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReviewLedger {
                    version: REVIEW_LEDGER_VERSION,
                    session_id: session_id.to_owned(),
                    files: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let ledger: ReviewLedger = serde_json::from_slice(&bytes)?;
        if ledger.version != REVIEW_LEDGER_VERSION || ledger.session_id != session_id {
            return Err(CheckpointError::CorruptReviewLedger);
        }
        if ledger.files.len() > MAX_REVIEW_FILES {
            return Err(CheckpointError::ReviewFileLimit);
        }
        for (path, record) in &ledger.files {
            operation.path(path)?;
            let heap = match &record.current {
                ReviewCurrentState::Present { content_blake3, .. } => content_blake3.capacity(),
                ReviewCurrentState::Unsupported { reason } => reason.capacity(),
                ReviewCurrentState::Absent => 0,
            };
            operation.retain_read::<(String, ReviewDecisionRecord)>(
                path.capacity().saturating_add(heap),
            )?;
            if normalize_relative(Path::new(path))? != *path {
                return Err(CheckpointError::CorruptReviewLedger);
            }
            validate_review_current(&record.current)?;
        }
        Ok(ledger)
    }

    pub(super) fn write_review_ledger(&self, ledger: &ReviewLedger) -> Result<(), CheckpointError> {
        atomic_replace(
            &self.review_path(&ledger.session_id),
            &super::operation::serialize_metadata(ledger, false)?,
        )
    }

    pub(super) fn review_path(&self, session_id: &str) -> PathBuf {
        self.root.join("reviews").join(format!("{session_id}.json"))
    }
}
