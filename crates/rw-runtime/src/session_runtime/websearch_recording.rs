use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_providers::FixtureRedactor;
use rw_tools::CancellationToken;
use rw_tools::ToolError;
use rw_tools::WebSearchRequest;
use rw_tools::WebSearchResponse;
use rw_tools::WebSearcher;
use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

pub(super) const WEBSEARCH_REPLAY_FILE: &str = "websearch.json";

pub(super) const WEBSEARCH_REPLAY_TEMP_PREFIX: &str = ".websearch.json.tmp-";

pub(super) struct WebSearchFixtureDirectory {
    pub(super) path: PathBuf,
    #[cfg(unix)]
    pub(super) descriptor: std::os::fd::OwnedFd,
}

impl WebSearchFixtureDirectory {
    pub(super) fn open(directory: &Path, create: bool) -> Result<Self> {
        if create {
            std::fs::create_dir_all(directory).map_err(|error| {
                miette!("web-search fixture directory could not create: {error}")
            })?;
        }
        let supplied = std::fs::symlink_metadata(directory)
            .map_err(|error| miette!("web-search fixture directory could not inspect: {error}"))?;
        if supplied.file_type().is_symlink() || !supplied.is_dir() {
            return Err(miette!(
                "web-search fixture directory must be a real directory, never a symlink"
            ));
        }
        let path = std::fs::canonicalize(directory).map_err(|error| {
            miette!("web-search fixture directory could not canonicalize: {error}")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let descriptor = rustix::fs::open(
                &path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)
            .map_err(|error| miette!("web-search fixture directory could not open: {error}"))?;
            let stat = rustix::fs::fstat(&descriptor)
                .map_err(std::io::Error::from)
                .map_err(|error| {
                    miette!("web-search fixture directory could not validate: {error}")
                })?;
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
                || crate::rustix_device_id(stat.st_dev) != Some(supplied.dev())
                || stat.st_ino != supplied.ino()
                || stat.st_uid != rustix::process::geteuid().as_raw()
                || stat.st_mode & 0o022 != 0
            {
                return Err(miette!(
                    "web-search fixture directory must be owner-controlled and not group/other writable"
                ));
            }
            Ok(Self { path, descriptor })
        }
        #[cfg(not(unix))]
        {
            Ok(Self { path })
        }
    }

    pub(super) fn fixture_path(&self) -> PathBuf {
        self.path.join(WEBSEARCH_REPLAY_FILE)
    }

    pub(super) fn open_fixture(&self) -> Result<Option<std::fs::File>> {
        #[cfg(unix)]
        let descriptor = match rustix::fs::openat(
            &self.descriptor,
            WEBSEARCH_REPLAY_FILE,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => {
                return Err(miette!(
                    "web-search fixture could not open safely: {}",
                    std::io::Error::from(error)
                ));
            }
        };
        #[cfg(unix)]
        let file = std::fs::File::from(descriptor);

        #[cfg(not(unix))]
        let file = {
            let path = self.fixture_path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(miette!("web-search fixture could not inspect: {error}")),
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(miette!("web-search fixture must be a regular file"));
            }
            std::fs::File::open(&path)
                .map_err(|error| miette!("web-search fixture could not open: {error}"))?
        };

        let metadata = file
            .metadata()
            .map_err(|error| miette!("web-search fixture could not validate: {error}"))?;
        if !metadata.is_file() {
            return Err(miette!(
                "web-search fixture must be a regular file, never a symlink or special file"
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                return Err(miette!(
                    "web-search fixture must be owner-controlled and private"
                ));
            }
        }
        Ok(Some(file))
    }

    pub(super) fn read_fixture(&self) -> Result<Option<Vec<u8>>> {
        let Some(mut file) = self.open_fixture()? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| miette!("web-search fixture could not read: {error}"))?;
        Ok(Some(bytes))
    }

    pub(super) fn persist(&self, bytes: &[u8]) -> std::result::Result<(), ToolError> {
        #[cfg(unix)]
        {
            self.persist_unix(bytes)
        }
        #[cfg(not(unix))]
        {
            self.persist_portable(bytes)
        }
    }

    #[cfg(unix)]
    pub(super) fn persist_unix(&self, bytes: &[u8]) -> std::result::Result<(), ToolError> {
        self.open_fixture()
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            ToolError::Network(format!("web-search fixture entropy failed: {error}"))
        })?;
        let suffix = blake3::hash(&random).to_hex();
        let temporary_name = format!("{WEBSEARCH_REPLAY_TEMP_PREFIX}{suffix}");
        let temporary_path = self.path.join(&temporary_name);
        let descriptor = rustix::fs::openat(
            &self.descriptor,
            temporary_name.as_str(),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "create private web-search fixture temporary",
            path: temporary_path.clone(),
            source,
        })?;
        let mut file = std::fs::File::from(descriptor);
        let installed = (|| -> std::result::Result<(), ToolError> {
            file.write_all(bytes).map_err(|source| ToolError::Io {
                operation: "write web-search fixture temporary",
                path: temporary_path.clone(),
                source,
            })?;
            file.flush().map_err(|source| ToolError::Io {
                operation: "flush web-search fixture temporary",
                path: temporary_path.clone(),
                source,
            })?;
            rustix::fs::fsync(&file)
                .map_err(std::io::Error::from)
                .map_err(|source| ToolError::Io {
                    operation: "synchronize web-search fixture temporary",
                    path: temporary_path.clone(),
                    source,
                })?;
            rustix::fs::renameat(
                &self.descriptor,
                temporary_name.as_str(),
                &self.descriptor,
                WEBSEARCH_REPLAY_FILE,
            )
            .map_err(std::io::Error::from)
            .map_err(|source| ToolError::Io {
                operation: "install web-search fixture",
                path: self.fixture_path(),
                source,
            })?;
            rustix::fs::fsync(&self.descriptor)
                .map_err(std::io::Error::from)
                .map_err(|source| ToolError::Io {
                    operation: "synchronize web-search fixture directory",
                    path: self.path.clone(),
                    source,
                })
        })();
        if installed.is_err() {
            let _ = rustix::fs::unlinkat(
                &self.descriptor,
                temporary_name.as_str(),
                rustix::fs::AtFlags::empty(),
            );
        }
        installed
    }

    #[cfg(not(unix))]
    pub(super) fn persist_portable(&self, bytes: &[u8]) -> std::result::Result<(), ToolError> {
        let temporary = self.path.join(format!(
            "{WEBSEARCH_REPLAY_TEMP_PREFIX}{}",
            std::process::id()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ToolError::Io {
                operation: "create web-search fixture temporary",
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes).map_err(|source| ToolError::Io {
            operation: "write web-search fixture temporary",
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| ToolError::Io {
            operation: "synchronize web-search fixture temporary",
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, self.fixture_path()).map_err(|source| ToolError::Io {
            operation: "install web-search fixture",
            path: self.fixture_path(),
            source,
        })
    }
}

pub(super) fn canonical_websearch_key(request: &WebSearchRequest) -> Result<String> {
    let canonical = serde_json::to_vec(&serde_json::json!({
        "query": request.query,
        "max_results": request.max_results,
        "recency_days": request.recency_days,
        "allowed_domains": request.allowed_domains,
    }))
    .map_err(|error| miette!("web-search request could not canonicalize: {error}"))?;
    Ok(blake3::hash(&canonical).to_hex().to_string())
}

pub(super) fn redact_websearch_response(
    mut response: WebSearchResponse,
    redactor: &FixtureRedactor,
) -> WebSearchResponse {
    for result in &mut response.results {
        result.title = redactor.redact_text(&result.title);
        result.url = redactor.redact_text(&result.url);
        result.snippet = redactor.redact_text(&result.snippet);
    }
    response
}

pub(super) struct RecordingConfiguredWebSearcher {
    pub(super) inner: Arc<dyn WebSearcher>,
    pub(super) directory: WebSearchFixtureDirectory,
    pub(super) redactor: FixtureRedactor,
    pub(super) fixtures: Mutex<BTreeMap<String, Vec<WebSearchResponse>>>,
}

impl RecordingConfiguredWebSearcher {
    pub(super) fn new(
        inner: Arc<dyn WebSearcher>,
        directory: &Path,
        redactor: FixtureRedactor,
    ) -> Result<Self> {
        let directory = WebSearchFixtureDirectory::open(directory, true)?;
        let fixtures = ReplayingConfiguredWebSearcher::load_from(&directory)?
            .map(|replay| replay.fixtures)
            .unwrap_or_default();
        Ok(Self {
            inner,
            directory,
            redactor,
            fixtures: Mutex::new(fixtures),
        })
    }

    pub(super) fn persist(&self) -> std::result::Result<(), ToolError> {
        let fixtures = self
            .fixtures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = serde_json::to_vec(&*fixtures).map_err(|error| {
            ToolError::Network(format!("web-search fixture encode failed: {error}"))
        })?;
        self.directory.persist(&bytes)
    }
}

#[async_trait]
impl WebSearcher for RecordingConfiguredWebSearcher {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<WebSearchResponse, ToolError> {
        let key = canonical_websearch_key(&request)
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let response = self.inner.search(request, cancellation).await?;
        let response = redact_websearch_response(response, &self.redactor);
        self.fixtures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .push(response.clone());
        self.persist()?;
        Ok(response)
    }
}

pub(super) struct ReplayingConfiguredWebSearcher {
    pub(super) fixtures: BTreeMap<String, Vec<WebSearchResponse>>,
    pub(super) occurrences: Mutex<BTreeMap<String, usize>>,
}

impl ReplayingConfiguredWebSearcher {
    pub(super) fn load(directory: &Path) -> Result<Option<Self>> {
        let directory = WebSearchFixtureDirectory::open(directory, false)?;
        Self::load_from(&directory)
    }

    pub(super) fn load_from(directory: &WebSearchFixtureDirectory) -> Result<Option<Self>> {
        let Some(bytes) = directory.read_fixture()? else {
            return Ok(None);
        };
        let encoded: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&bytes)
            .map_err(|error| miette!("web-search fixture could not parse: {error}"))?;
        let fixtures = encoded
            .into_iter()
            .map(|(key, value)| {
                let responses = if value.is_array() {
                    serde_json::from_value(value)
                } else {
                    serde_json::from_value(value).map(|response| vec![response])
                }
                .map_err(|error| miette!("web-search fixture response could not parse: {error}"))?;
                Ok((key, responses))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Some(Self {
            fixtures,
            occurrences: Mutex::new(BTreeMap::new()),
        }))
    }
}

#[async_trait]
impl WebSearcher for ReplayingConfiguredWebSearcher {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<WebSearchResponse, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let key = canonical_websearch_key(&request)
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let occurrence = {
            let mut occurrences = self
                .occurrences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let occurrence = *occurrences.get(&key).unwrap_or(&0);
            occurrences.insert(key.clone(), occurrence.saturating_add(1));
            occurrence
        };
        self.fixtures
            .get(&key)
            .and_then(|responses| responses.get(occurrence))
            .cloned()
            .ok_or_else(|| {
                ToolError::Network(format!(
                    "configured web-search replay sequence is exhausted at occurrence {occurrence}"
                ))
            })
    }
}
