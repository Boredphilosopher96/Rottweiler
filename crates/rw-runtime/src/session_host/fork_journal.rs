use super::*;

impl RuntimeSessionFactory {
    pub(super) fn fork_journal_directory(&self) -> PathBuf {
        self.options
            .storage_root
            .join("control")
            .join("fork-operations")
    }

    pub(super) fn fork_operation_id(key: &ForkOperationKey) -> String {
        let mut input = b"rw-fork-operation-v1\0".to_vec();
        input.extend_from_slice(&(key.operation_id.len() as u64).to_be_bytes());
        input.extend_from_slice(key.operation_id.as_bytes());
        blake3::hash(&input).to_hex().to_string()
    }

    pub(super) fn ensure_fork_journal_directory(&self) -> Result<PathBuf, HostError> {
        let control = self.options.storage_root.join("control");
        for directory in [&control, &self.fork_journal_directory()] {
            match fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unsafe".to_owned(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(directory) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            let metadata = fs::symlink_metadata(directory).map_err(|_| {
                                HostError::Persistence(
                                    "fork journal path is unavailable".to_owned(),
                                )
                            })?;
                            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                                return Err(HostError::Persistence(
                                    "fork journal path is unsafe".to_owned(),
                                ));
                            }
                        }
                        Err(_) => {
                            return Err(HostError::Persistence(
                                "fork journal could not initialize".to_owned(),
                            ));
                        }
                    }
                }
                Err(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unavailable".to_owned(),
                    ));
                }
            }
        }
        let directory = self.fork_journal_directory();
        #[cfg(unix)]
        fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .map_err(|_| HostError::Persistence("fork journal permissions failed".to_owned()))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|_| HostError::Persistence("fork journal is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HostError::Persistence(
                "fork journal path is unsafe".to_owned(),
            ));
        }
        Ok(directory)
    }

    pub(super) fn acquire_fork_journal_lock(&self) -> Result<ForkJournalLock, HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let control = directory.parent().ok_or_else(|| {
            HostError::Persistence("fork control directory is unavailable".to_owned())
        })?;
        #[cfg(unix)]
        {
            let descriptor = rustix::fs::open(
                control.join("fork-operations.lock"),
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|_| HostError::Persistence("fork journal lock is unsafe".to_owned()))?;
            let stat = rustix::fs::fstat(&descriptor).map_err(|_| {
                HostError::Persistence("fork journal lock is unavailable".to_owned())
            })?;
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
                || stat.st_nlink != 1
                || stat.st_mode & 0o077 != 0
            {
                return Err(HostError::Persistence(
                    "fork journal lock is not private and regular".to_owned(),
                ));
            }
            let file = fs::File::from(descriptor);
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
                .map_err(|_| HostError::Persistence("fork journal lock failed".to_owned()))?;
            Ok(ForkJournalLock { _file: file })
        }
        #[cfg(not(unix))]
        {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            Ok(ForkJournalLock {
                _guard: LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            })
        }
    }

    pub(super) fn is_lower_hex(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    pub(super) fn expected_fork_state(
        &self,
        request: &ForkSessionRequest,
        workspace: &Path,
    ) -> Result<ExpectedForkState, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &request.parent.session_id.0)
                .map_err(|_| {
                    HostError::Persistence("fork parent metadata is unavailable".to_owned())
                })?;
        if metadata.workspace != workspace {
            return Err(HostError::Persistence(
                "fork parent workspace does not match its operation".to_owned(),
            ));
        }
        let fork_turn = request
            .at_turn
            .0
            .parse::<u64>()
            .map_err(|_| HostError::Persistence("fork turn is invalid".into()))?;
        let route = crate::mode_recovery::fork_route(
            &self.journal_service,
            &request.parent.session_id.0,
            fork_turn,
            request.through_sequence,
            request.include_idle_tail,
        )
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        let selected = route
            .lease
            .view
            .prefix_through(route.through)
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let roots = crate::session_runtime::load_checkpoint_roots_exact(
            &crate::session_runtime::checkpoint_root(
                &self.options.storage_root,
                workspace,
                &request.parent.session_id.0,
            ),
            route.workspace_generation,
        )
        .map_err(|_| HostError::Persistence("fork root generation is unavailable".to_owned()))?
        .ok_or_else(|| {
            HostError::Persistence("fork root generation is not committed".to_owned())
        })?;
        let (user_home, user_rottweiler) =
            crate::session_runtime::extension_user_roots(&self.options.credentials_path);
        let catalog = crate::session_runtime::discover_runtime_extensions(
            &roots,
            &self.options.storage_root.join("trust.json"),
            &user_home,
            &user_rottweiler,
            self.options.dangerously_trust,
        )
        .map_err(|_| HostError::Persistence("fork mode registry is unavailable".to_owned()))?;
        let modes = rw_ext::compose_mode_registry(&catalog)
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let recovered = crate::mode_recovery::validate_fork(
            &selected,
            &modes,
            metadata
                .inherited_journal_through
                .filter(|inherited| route.through.is_some_and(|cut| *inherited <= cut)),
        )
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        if recovered.workspace_generation != route.workspace_generation {
            return Err(HostError::Persistence(
                "fork routing and canonical workspace disagree".into(),
            ));
        }
        let roots_digest =
            blake3::hash(&serde_json::to_vec(&roots).map_err(|_| {
                HostError::Persistence("fork roots could not serialize".to_owned())
            })?)
            .to_hex()
            .to_string();
        Ok(ExpectedForkState {
            model: ModelAlias(recovered.model_alias.unwrap_or(metadata.model_alias)),
            workspace_generation: recovered.workspace_generation,
            roots_digest,
            modes,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn validate_fork_journal(
        &self,
        journal: &ForkOperationJournal,
        path: &Path,
    ) -> Result<(), HostError> {
        let key = ForkOperationKey {
            operation_id: journal.stable_operation_id.clone(),
            client_id: journal.client_id.clone(),
            request_id: journal.request_id.clone(),
            payload_hash: journal.payload_hash.clone(),
        };
        let expected_id = Self::fork_operation_id(&key);
        let expected_filename = format!("{expected_id}.json");
        let canonical = self.authorize_workspace_path(&journal.canonical_workspace)?;
        let workspace_digest = blake3::hash(canonical.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let safe_text = |value: &str| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        let safe_session = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        let safe_operation = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        if journal.version != FORK_JOURNAL_VERSION
            || !Self::is_lower_hex(&journal.operation_id)
            || journal.operation_id != expected_id
            || path.file_name().and_then(|name| name.to_str()) != Some(&expected_filename)
            || !Self::is_lower_hex(&journal.payload_hash)
            || !Self::is_lower_hex(&journal.workspace_digest)
            || !Self::is_lower_hex(&journal.child_roots_digest)
            || journal.workspace_digest != workspace_digest
            || canonical != journal.canonical_workspace
            || !safe_text(&journal.client_id.0)
            || !safe_text(&journal.request_id.0)
            || !safe_operation(&journal.stable_operation_id)
            || !safe_text(&journal.child_model.0)
            || !safe_session(&journal.parent.session_id.0)
            || !safe_session(&journal.child_session_id.0)
            || journal.driver_client_id != journal.client_id
            || journal.at_turn.0.parse::<u64>().is_err()
            || workspace_name(&canonical) != journal.parent.workspace_name
        {
            return Err(HostError::Persistence(
                "fork journal validation failed".to_owned(),
            ));
        }
        if let ForkJournalState::Completed { result } = &journal.state {
            crate::session_runtime::validate_forked_session_commit(
                &self.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
                &journal.operation_id,
                &journal.parent.session_id.0,
            )
            .map_err(|_| {
                HostError::Persistence("completed fork storage validation failed".to_owned())
            })?;
            let child_metadata =
                load_session_metadata_any(&self.options.storage_root, &journal.child_session_id.0)
                    .map_err(|_| {
                        HostError::Persistence("completed fork metadata is unavailable".to_owned())
                    })?;
            let child_roots_digest = blake3::hash(
                &serde_json::to_vec(&child_metadata.workspace_roots).map_err(|_| {
                    HostError::Persistence("completed fork roots could not serialize".to_owned())
                })?,
            )
            .to_hex()
            .to_string();
            if result.protocol_version != rw_core::PROTOCOL_VERSION
                || UtcTimestamp::parse(result.command_ack_emitted_at.clone()).is_err()
                || UtcTimestamp::parse(result.fork_event_emitted_at.clone()).is_err()
                || !matches!(result.outcome, rw_core::CommandOutcome::Accepted {})
                || result.acknowledged_session_id != journal.parent.session_id
                || result.parent_session_id != journal.parent.session_id
                || result.child.session_id != journal.child_session_id
                || result.child.workspace_name != journal.parent.workspace_name
                || result.child.model != journal.child_model
                || child_metadata.workspace_generation != journal.child_workspace_generation
                || child_roots_digest != journal.child_roots_digest
                || result
                    .child
                    .driver_client_id
                    .as_ref()
                    .is_none_or(|driver| !safe_text(&driver.0))
                || result.child.shell_active
                || !safe_text(&result.child.workspace_name)
                || !safe_text(&result.child.model.0)
                || result.at_turn != journal.at_turn
            {
                return Err(HostError::Persistence(
                    "completed fork result does not match its prepared operation".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn fork_journal_path(&self, key: &ForkOperationKey) -> Result<PathBuf, HostError> {
        Ok(self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", Self::fork_operation_id(key))))
    }

    #[cfg(unix)]
    pub(super) fn read_fork_journal_file(
        &self,
        filename: &std::ffi::OsStr,
    ) -> Result<Vec<u8>, HostError> {
        let root = rustix::fs::open(
            &self.options.storage_root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("storage root is unsafe".to_owned()))?;
        let control = rustix::fs::openat(
            &root,
            "control",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork control directory is unsafe".to_owned()))?;
        let directory = rustix::fs::openat(
            &control,
            "fork-operations",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal directory is unsafe".to_owned()))?;
        let file = rustix::fs::openat(
            &directory,
            filename,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal file is unsafe".to_owned()))?;
        let stat = rustix::fs::fstat(&file)
            .map_err(|_| HostError::Persistence("fork journal metadata failed".to_owned()))?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
            || stat.st_nlink != 1
            || stat.st_mode & 0o077 != 0
            || usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is not private and regular".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(stat.st_size)
                .unwrap_or(MAX_FORK_JOURNAL_BYTES)
                .min(MAX_FORK_JOURNAL_BYTES),
        );
        fs::File::from(file)
            .take((MAX_FORK_JOURNAL_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        Ok(bytes)
    }

    #[cfg(not(unix))]
    pub(super) fn read_fork_journal_file(
        &self,
        filename: &std::ffi::OsStr,
    ) -> Result<Vec<u8>, HostError> {
        let path = self.ensure_fork_journal_directory()?.join(filename);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| HostError::Persistence("fork journal file is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is unsafe".to_owned(),
            ));
        }
        fs::read(path).map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))
    }

    pub(super) fn load_fork_journal_unlocked(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let path = self.fork_journal_path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        let filename = path.file_name().ok_or_else(|| {
            HostError::Persistence("fork journal filename is unavailable".to_owned())
        })?;
        let bytes = self.read_fork_journal_file(filename)?;
        let journal: ForkOperationJournal = serde_json::from_slice(&bytes)
            .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
        self.validate_fork_journal(&journal, &path)?;
        let operation_id = Self::fork_operation_id(key);
        if journal.version != FORK_JOURNAL_VERSION
            || journal.operation_id != operation_id
            || journal.stable_operation_id != key.operation_id
        {
            return Err(HostError::Persistence(
                "fork journal identity is corrupt".to_owned(),
            ));
        }
        if journal.payload_hash != key.payload_hash {
            return Err(HostError::RequestConflict);
        }
        Ok(Some(journal))
    }

    #[cfg(test)]
    pub(super) fn load_fork_journal(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.load_fork_journal_unlocked(key)
    }

    pub(super) fn persist_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
        replace: bool,
    ) -> Result<(), HostError> {
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|_| HostError::Persistence("fork journal could not serialize".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        let directory = path.parent().ok_or_else(|| {
            HostError::Persistence("fork journal directory is unavailable".to_owned())
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = directory.join(format!(".fork-{}-{nonce}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| HostError::Persistence("fork journal could not create".to_owned()))?;
        let result = (|| {
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| HostError::Persistence("fork journal could not sync".to_owned()))?;
            #[cfg(unix)]
            {
                let directory_file = fs::File::open(directory).map_err(|_| {
                    HostError::Persistence("fork journal directory is unavailable".to_owned())
                })?;
                let temporary_name = temporary.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal temporary name is unavailable".to_owned())
                })?;
                let final_name = path.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal filename is unavailable".to_owned())
                })?;
                let rename = if replace {
                    rustix::fs::renameat(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                    )
                } else {
                    rustix::fs::renameat_with(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                        rustix::fs::RenameFlags::NOREPLACE,
                    )
                };
                rename.map_err(|error| {
                    if !replace && error == rustix::io::Errno::EXIST {
                        HostError::RequestConflict
                    } else {
                        HostError::Persistence("fork journal update could not commit".to_owned())
                    }
                })?;
                rustix::fs::fsync(&directory_file).map_err(|_| {
                    HostError::Persistence("fork journal directory could not sync".to_owned())
                })?;
            }
            #[cfg(not(unix))]
            {
                if !replace && path.exists() {
                    return Err(HostError::RequestConflict);
                }
                fs::rename(&temporary, path).map_err(|_| {
                    HostError::Persistence("fork journal update could not commit".to_owned())
                })?;
                fs::File::open(directory)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| {
                        HostError::Persistence("fork journal directory could not sync".to_owned())
                    })?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(super) fn persist_new_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        Self::persist_fork_journal(path, journal, false)
    }

    pub(super) fn same_fork_operation(
        left: &ForkOperationJournal,
        right: &ForkOperationJournal,
    ) -> bool {
        left.version == right.version
            && left.operation_id == right.operation_id
            && left.child_model == right.child_model
            && left.child_workspace_generation == right.child_workspace_generation
            && left.child_roots_digest == right.child_roots_digest
            && Self::journal_operation(left) == Self::journal_operation(right)
            && left.canonical_workspace == right.canonical_workspace
            && left.workspace_digest == right.workspace_digest
    }

    pub(super) fn transition_fork_journal_unlocked(
        &self,
        candidate: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let key = ForkOperationKey {
            operation_id: candidate.stable_operation_id.clone(),
            client_id: candidate.client_id.clone(),
            request_id: candidate.request_id.clone(),
            payload_hash: candidate.payload_hash.clone(),
        };
        let current = self
            .load_fork_journal_unlocked(&key)?
            .ok_or_else(|| HostError::Persistence("fork operation was not prepared".to_owned()))?;
        if !Self::same_fork_operation(&current, candidate) {
            return Err(HostError::RequestConflict);
        }
        let current_rank = current.state.rank();
        let candidate_rank = candidate.state.rank();
        if current_rank >= candidate_rank {
            return Ok(current);
        }
        if candidate_rank != current_rank.saturating_add(1) {
            return Err(HostError::Persistence(
                "fork journal transition skipped a durable phase".to_owned(),
            ));
        }
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", candidate.operation_id));
        self.validate_fork_journal(candidate, &path)?;
        Self::persist_fork_journal(&path, candidate, true)?;
        Ok(candidate.clone())
    }

    #[cfg(test)]
    pub(super) fn force_replace_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", journal.operation_id));
        Self::persist_fork_journal(&path, journal, true)
    }

    #[cfg(test)]
    pub(super) fn transition_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.transition_fork_journal_unlocked(journal)
    }

    pub(super) fn journal_operation(journal: &ForkOperationJournal) -> PreparedForkOperation {
        PreparedForkOperation {
            key: ForkOperationKey {
                operation_id: journal.stable_operation_id.clone(),
                client_id: journal.client_id.clone(),
                request_id: journal.request_id.clone(),
                payload_hash: journal.payload_hash.clone(),
            },
            request: ForkSessionRequest {
                operation_key: ForkOperationKey {
                    operation_id: journal.stable_operation_id.clone(),
                    client_id: journal.client_id.clone(),
                    request_id: journal.request_id.clone(),
                    payload_hash: journal.payload_hash.clone(),
                },
                parent: journal.parent.clone(),
                child_session_id: journal.child_session_id.clone(),
                at_turn: journal.at_turn.clone(),
                through_sequence: journal.through_sequence,
                include_idle_tail: journal.include_idle_tail,
                driver_client_id: journal.driver_client_id.clone(),
            },
        }
    }

    pub(super) fn completed_fork_result(result: &ForkJournalResult) -> CompletedForkOperation {
        CompletedForkOperation {
            protocol_version: result.protocol_version,
            command_ack_emitted_at: result.command_ack_emitted_at.clone(),
            fork_event_emitted_at: result.fork_event_emitted_at.clone(),
            acknowledged_session_id: result.acknowledged_session_id.clone(),
            outcome: result.outcome.clone(),
            parent_session_id: result.parent_session_id.clone(),
            child: result.child.clone(),
            at_turn: result.at_turn.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn recover_fork_operations(&self) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let directory = self.ensure_fork_journal_directory()?;
        let mut completed = Vec::new();
        let mut pending = 0_usize;
        let mut seen = 0_usize;
        let mut directory_changed = false;
        let entries = fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            let path = entry.path();
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                HostError::Persistence("fork journal has a non-Unicode entry".to_owned())
            })?;
            if name.starts_with(".fork-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
            {
                let metadata = fs::symlink_metadata(&path).map_err(|_| {
                    HostError::Persistence("fork journal temporary file is unsafe".to_owned())
                })?;
                #[cfg(unix)]
                let private_single_link = {
                    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                    metadata.nlink() == 1 && metadata.permissions().mode().trailing_zeros() >= 6
                };
                #[cfg(not(unix))]
                let private_single_link = true;
                if metadata.file_type().is_symlink() || !metadata.is_file() || !private_single_link
                {
                    return Err(HostError::Persistence(
                        "fork journal temporary file is unsafe".to_owned(),
                    ));
                }
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal temporary cleanup failed".to_owned())
                })?;
                directory_changed = true;
                continue;
            }
            let stem = name.strip_suffix(".json").ok_or_else(|| {
                HostError::Persistence("fork journal has an unexpected entry".to_owned())
            })?;
            if !Self::is_lower_hex(stem) {
                return Err(HostError::Persistence(
                    "fork journal filename is invalid".to_owned(),
                ));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                HostError::Persistence("fork journal entry is unavailable".to_owned())
            })?;
            #[cfg(unix)]
            let single_link = {
                use std::os::unix::fs::MetadataExt as _;
                metadata.nlink() == 1
            };
            #[cfg(not(unix))]
            let single_link = true;
            if metadata.file_type().is_symlink() || !metadata.is_file() || !single_link {
                return Err(HostError::Persistence(
                    "fork journal entry is unsafe".to_owned(),
                ));
            }
            let bytes = self.read_fork_journal_file(&entry.file_name())?;
            let mut journal: ForkOperationJournal = serde_json::from_slice(&bytes)
                .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Prepared => {
                    let metadata = self
                        .options
                        .storage_root
                        .join("sessions")
                        .join(&journal.child_session_id.0)
                        .join("metadata.json");
                    if metadata.is_file() {
                        crate::session_runtime::validate_forked_session_commit(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                            &journal.operation_id,
                            &journal.parent.session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence(
                                "committed fork storage failed recovery validation".to_owned(),
                            )
                        })?;
                        journal.state = ForkJournalState::StorageCommitted;
                        journal.updated_unix_ms = unix_millis();
                        self.transition_fork_journal_unlocked(&journal)?;
                        pending = pending.saturating_add(1);
                    } else {
                        remove_forked_session_storage(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence("partial fork cleanup failed".to_owned())
                        })?;
                        // The durable operation remains authoritative even before
                        // child metadata exists, so retry reuses the same child id.
                        pending = pending.saturating_add(1);
                    }
                }
                ForkJournalState::StorageCommitted => {
                    crate::session_runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "committed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    pending = pending.saturating_add(1);
                }
                ForkJournalState::Completed { .. } => {
                    crate::session_runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "completed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
            }
        }
        if pending > MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::Persistence(
                "too many unfinished fork operations require recovery".to_owned(),
            ));
        }
        completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        let remove = completed
            .len()
            .saturating_sub(MAX_COMPLETED_FORK_OPERATIONS);
        for (_, _, path) in completed.into_iter().take(remove) {
            fs::remove_file(path)
                .map_err(|_| HostError::Persistence("fork journal retention failed".to_owned()))?;
            directory_changed = true;
        }
        if directory_changed {
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal cleanup could not sync".to_owned())
                })?;
        }
        Ok(())
    }

    pub(super) fn enforce_live_fork_limits_unlocked(
        &self,
        prune_completed: bool,
    ) -> Result<(), HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let mut pending = 0_usize;
        let mut completed = Vec::new();
        let mut seen = 0_usize;
        for entry in fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?
        {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let journal: ForkOperationJournal =
                serde_json::from_slice(&self.read_fork_journal_file(&entry.file_name())?)
                    .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Completed { .. } => {
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => {
                    pending = pending.saturating_add(1);
                }
            }
        }
        if !prune_completed && pending >= MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::SessionCapacity);
        }
        if prune_completed && completed.len() > MAX_COMPLETED_FORK_OPERATIONS {
            completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
            let remove = completed.len() - MAX_COMPLETED_FORK_OPERATIONS;
            for (_, _, path) in completed.into_iter().take(remove) {
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal retention failed".to_owned())
                })?;
            }
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal retention could not sync".to_owned())
                })?;
        }
        Ok(())
    }
}
