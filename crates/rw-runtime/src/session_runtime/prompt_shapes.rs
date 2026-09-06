use super::session_metadata::{ensure_real_directory, validate_session_id};
use miette::{IntoDiagnostic, Result, miette};
use rw_providers::{
    CacheBreakpointSupport, CacheHint, ProviderRequest, ToolChoice, ToolDefinition,
};
use rw_types::Turn;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::Path,
    sync::Mutex,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptShapeProfile {
    pub(super) model_alias: String,
    pub(super) tools: Vec<ToolDefinition>,
    pub(super) cache_support: CacheBreakpointSupport,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) cache_hint: Option<CacheHint>,
    pub(super) cache_breakpoints: Vec<PromptCacheBreakpoint>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptCacheBreakpoint {
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) after_item_id: Option<String>,
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptShapeRecord {
    pub(super) profile_id: String,
    pub(super) request_fingerprint: String,
    pub(super) source: u64,
}

#[derive(Serialize)]
struct Profile<'a> {
    model_alias: &'a str,
    tools: &'a [ToolDefinition],
    cache_support: CacheBreakpointSupport,
    cache_hint: Option<CacheHint>,
    cache_breakpoints: &'a [PromptCacheBreakpoint],
}
#[derive(Debug)]
struct ActivePrompt {
    turn: rw_core::TurnId,
    source: Option<u64>,
    recorded: bool,
}

#[derive(Debug)]
pub(super) struct PromptShapeJournal {
    store: Mutex<rw_store::prompt_shapes::PromptShapeStore>,
    active_turn: Mutex<Option<ActivePrompt>>,
    records: std::sync::Arc<tokio::sync::Semaphore>,
}

impl PromptShapeJournal {
    pub(super) fn open(storage_root: &Path, session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        let directory = storage_root.join("sessions").join(session_id);
        ensure_real_directory(&directory, false)?;
        Ok(Self {
            store: Mutex::new(
                rw_store::prompt_shapes::PromptShapeStore::open(
                    &directory.join("prompt-shapes.sqlite3"),
                )
                .into_diagnostic()?,
            ),
            active_turn: Mutex::new(None),
            records: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub(super) async fn record_owned(
        self: std::sync::Arc<Self>,
        alias: String,
        request: ProviderRequest,
        cache: CacheBreakpointSupport,
    ) -> Result<ProviderRequest> {
        if request.tool_choice == (ToolChoice::None {})
            || self
                .active_turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_none_or(|active| active.recorded)
        {
            return Ok(request);
        }
        let permit = std::sync::Arc::clone(&self.records)
            .try_acquire_owned()
            .into_diagnostic()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            self.record_request(&alias, &request, cache)?;
            Ok(request)
        })
        .await
        .into_diagnostic()?
    }
    pub(super) async fn settle_records(&self) -> Result<()> {
        let permit = self.records.acquire().await.into_diagnostic()?;
        drop(permit);
        Ok(())
    }
    pub(super) fn set_active_turn(&self, turn: rw_core::TurnId) {
        *self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActivePrompt {
            turn,
            source: None,
            recorded: false,
        });
    }
    pub(super) fn set_prompt_source(&self, turn: &rw_core::TurnId, source: rw_types::SequenceId) {
        let mut active = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = active.as_mut().filter(|active| &active.turn == turn) {
            active.source.get_or_insert(source.0);
        }
    }
    pub(super) fn clear_active_turn(&self, turn: &rw_core::TurnId) {
        let mut active = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_ref().is_some_and(|active| &active.turn == turn) {
            *active = None;
        }
    }
    pub(super) fn record_request(
        &self,
        model_alias: &str,
        request: &ProviderRequest,
        cache_support: CacheBreakpointSupport,
    ) -> Result<()> {
        if request.tool_choice == (ToolChoice::None {}) {
            return Ok(());
        }
        let mut active = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = active.as_mut() else {
            return Ok(());
        };
        if active.recorded {
            return Ok(());
        }
        let source = active
            .source
            .ok_or_else(|| miette!("provider request lacks its committed context source"))?;
        let turn = active.turn.0.parse::<u64>().into_diagnostic()?;
        let breakpoints = cache_breakpoints_for_hint(request.cache_hint, cache_support);
        let profile = Profile {
            model_alias,
            tools: &request.tools,
            cache_support,
            cache_hint: request.cache_hint,
            cache_breakpoints: &breakpoints,
        };
        let mut bytes = BoundedWriter(Vec::new());
        serde_json::to_writer(&mut bytes, &profile).into_diagnostic()?;
        admit_profile(&bytes.0)?;
        let fingerprint = prompt_request_fingerprint(
            model_alias,
            &request.turns,
            &request.tools,
            request.cache_hint,
            cache_support,
            &breakpoints,
        )?;
        let fingerprint = *blake3::Hash::from_hex(&fingerprint)
            .into_diagnostic()?
            .as_bytes();
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(turn, source, &bytes.0, fingerprint)
            .into_diagnostic()?;
        active.recorded = true;
        Ok(())
    }
    pub(super) fn shape_for_turn(
        &self,
        turn: u64,
    ) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        self.read(Some(turn), None)
    }
    pub(super) fn shape_at_source(
        &self,
        turn: u64,
        source: u64,
    ) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        self.read(Some(turn), Some(source))
    }
    pub(super) fn latest_shape(&self) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        self.read(None, None)
    }
    fn read(
        &self,
        turn: Option<u64>,
        source: Option<u64>,
    ) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        let stored = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read(turn, source)
            .into_diagnostic()?;
        stored
            .map(|stored| {
                admit_profile(&stored.profile)?;
                let profile: PromptShapeProfile =
                    serde_json::from_slice(&stored.profile).into_diagnostic()?;
                if profile
                    .cache_hint
                    .is_some_and(|hint| hint.tools_in_prefix == profile.tools.is_empty())
                    || profile.cache_breakpoints
                        != cache_breakpoints_for_hint(profile.cache_hint, profile.cache_support)
                {
                    return Err(miette!("prompt shape has inconsistent cache metadata"));
                }
                let record = PromptShapeRecord {
                    profile_id: blake3::hash(&stored.profile).to_hex().to_string(),
                    request_fingerprint: blake3::Hash::from_bytes(stored.fingerprint)
                        .to_hex()
                        .to_string(),
                    source: stored.source,
                };
                Ok((profile, record))
            })
            .transpose()
    }
}
fn admit_profile(bytes: &[u8]) -> Result<()> {
    let shape = rw_types::json_structure::preflight_json(
        bytes,
        rw_types::json_structure::JsonStructureLimits {
            max_encoded_bytes: rw_store::prompt_shapes::MAX_PROFILE_BYTES,
            max_nodes: 32_768,
            max_string_bytes: rw_store::prompt_shapes::MAX_PROFILE_BYTES,
            max_depth: 32,
        },
    )
    .into_diagnostic()?;
    // The profile and ToolDefinition are directly decoded structs; their only
    // recursive value is JSON schema. No internally tagged Content tree exists.
    // Charge typed envelope slots in addition to the direct JSON container bound.
    let slot = std::mem::size_of::<PromptShapeProfile>()
        .max(std::mem::size_of::<ToolDefinition>())
        .max(std::mem::size_of::<PromptCacheBreakpoint>());
    if shape
        .direct_value_decode_bytes()
        .and_then(|bytes| bytes.checked_add(shape.nodes.checked_mul(slot)?.checked_mul(2)?))
        .is_none_or(|size| size > 16 * 1024 * 1024)
    {
        return Err(miette!("prompt shape exceeds decoded allocation admission"));
    }
    Ok(())
}
struct BoundedWriter(Vec<u8>);
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > rw_store::prompt_shapes::MAX_PROFILE_BYTES {
            return Err(io::Error::other("prompt shape exceeds encoded admission"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn hash_serialized(value: &impl Serialize) -> Result<String> {
    struct HashWriter(blake3::Hasher);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    serde_json::to_writer(&mut writer, value).into_diagnostic()?;
    Ok(writer.0.finalize().to_hex().to_string())
}

pub(super) fn cache_breakpoints_for_hint(
    cache_hint: Option<CacheHint>,
    cache_support: CacheBreakpointSupport,
) -> Vec<PromptCacheBreakpoint> {
    if cache_support == CacheBreakpointSupport::None {
        return Vec::new();
    }
    let after_item_id = cache_hint
        .and_then(|hint| hint.stable_prefix_turns.checked_sub(1))
        .map(|index| format!("system:{index}"));
    vec![PromptCacheBreakpoint { after_item_id }]
}

pub(super) fn prompt_dump_cache_breakpoints(
    dump: &rw_core::PromptDump,
) -> Vec<PromptCacheBreakpoint> {
    dump.cache_breakpoints
        .iter()
        .map(|breakpoint| PromptCacheBreakpoint {
            after_item_id: breakpoint
                .after_item_id
                .as_ref()
                .map(|item_id| item_id.0.clone()),
        })
        .collect()
}

pub(super) fn prompt_request_fingerprint(
    model_alias: &str,
    turns: &[Turn],
    tools: &(impl Serialize + ?Sized),
    cache_hint: Option<CacheHint>,
    cache_support: CacheBreakpointSupport,
    cache_breakpoints: &[PromptCacheBreakpoint],
) -> Result<String> {
    #[derive(Serialize)]
    struct RequestShape<'a, T: ?Sized> {
        model_alias: &'a str,
        turns: &'a [Turn],
        tools: &'a T,
        cache_hint: Option<CacheHint>,
        cache_support: CacheBreakpointSupport,
        cache_breakpoints: &'a [PromptCacheBreakpoint],
    }
    hash_serialized(&RequestShape {
        model_alias,
        turns,
        tools,
        cache_hint,
        cache_support,
        cache_breakpoints,
    })
}

pub(super) fn validate_historical_prompt_shape(
    dump: &rw_core::PromptDump,
    tools: &(impl Serialize + ?Sized),
    profile: &PromptShapeProfile,
    record: &PromptShapeRecord,
) -> Result<()> {
    let fingerprint = prompt_request_fingerprint(
        &dump.model_alias.0,
        &dump.turns,
        tools,
        profile.cache_hint,
        profile.cache_support,
        &profile.cache_breakpoints,
    )?;
    if fingerprint != record.request_fingerprint {
        return Err(miette!(
            "historical prompt reconstruction did not match its recorded request shape"
        ));
    }
    if prompt_dump_cache_breakpoints(dump) != profile.cache_breakpoints {
        return Err(miette!(
            "historical prompt reconstruction did not match its recorded cache behavior"
        ));
    }
    Ok(())
}
