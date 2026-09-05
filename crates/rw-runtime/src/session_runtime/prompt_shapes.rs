use super::session_metadata::{ensure_real_directory, validate_session_id};
use miette::{IntoDiagnostic, Result, miette};
use rw_providers::{
    CacheBreakpointSupport, CacheHint, ProviderRequest, ToolChoice, ToolDefinition,
};
use rw_types::Turn;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const PROMPT_SHAPE_VERSION: u16 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptShapeProfile {
    pub(super) model_alias: String,
    pub(super) tools: Vec<ToolDefinition>,
    pub(super) cache_support: CacheBreakpointSupport,
    #[serde(default)]
    pub(super) cache_hint: Option<CacheHint>,
    #[serde(default)]
    pub(super) cache_breakpoints: Vec<PromptCacheBreakpoint>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptCacheBreakpoint {
    pub(super) after_item_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptShapeRecord {
    pub(super) profile_id: String,
    pub(super) request_fingerprint: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptShapeState {
    pub(super) version: u16,
    #[serde(default)]
    pub(super) profiles: BTreeMap<String, PromptShapeProfile>,
    #[serde(default)]
    pub(super) records: BTreeMap<String, PromptShapeRecord>,
}

impl Default for PromptShapeState {
    fn default() -> Self {
        Self {
            version: PROMPT_SHAPE_VERSION,
            profiles: BTreeMap::new(),
            records: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct PromptShapeJournal {
    pub(super) path: PathBuf,
    pub(super) state: Mutex<PromptShapeState>,
    pub(super) active_turn: Mutex<Option<rw_core::TurnId>>,
}

impl PromptShapeJournal {
    pub(super) fn open(storage_root: &Path, session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        let directory = storage_root.join("sessions").join(session_id);
        ensure_real_directory(&directory, false)?;
        let path = directory.join("prompt-shapes.json");
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(miette!("prompt-shape metadata is not a regular file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(miette!(
                            "prompt-shape metadata permissions grant group or other access"
                        ));
                    }
                }
                let bytes = std::fs::read(&path).into_diagnostic()?;
                let state: PromptShapeState = serde_json::from_slice(&bytes).into_diagnostic()?;
                if state.version != PROMPT_SHAPE_VERSION {
                    return Err(miette!("unsupported prompt-shape metadata version"));
                }
                validate_prompt_shape_state(&state)?;
                state
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => PromptShapeState::default(),
            Err(error) => return Err(error).into_diagnostic(),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
            active_turn: Mutex::new(None),
        })
    }

    pub(super) fn set_active_turn(&self, turn_id: rw_core::TurnId) {
        *self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn_id);
    }

    pub(super) fn clear_active_turn(&self, turn_id: &rw_core::TurnId) {
        let mut active = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_ref() == Some(turn_id) {
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
        let Some(turn_id) = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return Ok(());
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.records.contains_key(&turn_id.0) {
            return Ok(());
        }
        let profile = PromptShapeProfile {
            model_alias: model_alias.to_owned(),
            tools: request.tools.clone(),
            cache_support,
            cache_hint: request.cache_hint,
            cache_breakpoints: cache_breakpoints_for_hint(request.cache_hint, cache_support),
        };
        let profile_id = hash_serialized(&profile)?;
        let request_fingerprint = prompt_request_fingerprint(
            model_alias,
            &request.turns,
            &request.tools,
            request.cache_hint,
            cache_support,
            &profile.cache_breakpoints,
        )?;
        state.profiles.entry(profile_id.clone()).or_insert(profile);
        state.records.insert(
            turn_id.0,
            PromptShapeRecord {
                profile_id,
                request_fingerprint,
            },
        );
        persist_prompt_shape_state(&self.path, &state)
    }

    pub(super) fn shape_for_turn(
        &self,
        turn: u64,
    ) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = state.records.get(&turn.to_string()) else {
            return Ok(None);
        };
        let profile = state
            .profiles
            .get(&record.profile_id)
            .ok_or_else(|| miette!("prompt-shape record references a missing profile"))?;
        Ok(Some((profile.clone(), record.clone())))
    }

    pub(super) fn latest_shape(&self) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((_, record)) = state
            .records
            .iter()
            .filter_map(|(turn, record)| turn.parse::<u64>().ok().map(|turn| (turn, record)))
            .max_by_key(|(turn, _)| *turn)
        else {
            return Ok(None);
        };
        let profile = state
            .profiles
            .get(&record.profile_id)
            .ok_or_else(|| miette!("prompt-shape record references a missing profile"))?;
        Ok(Some((profile.clone(), record.clone())))
    }
}

pub(super) fn hash_serialized(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).into_diagnostic()?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
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

pub(super) fn is_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn validate_prompt_shape_state(state: &PromptShapeState) -> Result<()> {
    for (profile_id, profile) in &state.profiles {
        if !is_blake3_hex(profile_id) || hash_serialized(profile)? != *profile_id {
            return Err(miette!(
                "prompt-shape profile id does not match its serialized content"
            ));
        }
        if profile
            .cache_hint
            .is_some_and(|hint| hint.tools_in_prefix == profile.tools.is_empty())
            || profile.cache_breakpoints
                != cache_breakpoints_for_hint(profile.cache_hint, profile.cache_support)
        {
            return Err(miette!(
                "prompt-shape profile contains inconsistent cache metadata"
            ));
        }
    }
    for (turn, record) in &state.records {
        if turn.parse::<u64>().is_err() {
            return Err(miette!("prompt-shape record has an invalid turn id"));
        }
        if !is_blake3_hex(&record.request_fingerprint) {
            return Err(miette!(
                "prompt-shape record has an invalid request fingerprint"
            ));
        }
        if !state.profiles.contains_key(&record.profile_id) {
            return Err(miette!("prompt-shape record references a missing profile"));
        }
    }
    Ok(())
}

pub(super) fn prompt_request_fingerprint(
    model_alias: &str,
    turns: &[Turn],
    tools: &[ToolDefinition],
    cache_hint: Option<CacheHint>,
    cache_support: CacheBreakpointSupport,
    cache_breakpoints: &[PromptCacheBreakpoint],
) -> Result<String> {
    hash_serialized(&serde_json::json!({
        "model_alias": model_alias,
        "turns": turns,
        "tools": tools,
        "cache_hint": cache_hint,
        "cache_support": cache_support,
        "cache_breakpoints": cache_breakpoints,
    }))
}

pub(super) fn validate_historical_prompt_shape(
    dump: &rw_core::PromptDump,
    tools: &[ToolDefinition],
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

pub(super) fn persist_prompt_shape_state(path: &Path, state: &PromptShapeState) -> Result<()> {
    let bytes = serde_json::to_vec(state).into_diagnostic()?;
    let parent = path
        .parent()
        .ok_or_else(|| miette!("prompt-shape path has no parent"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".prompt-shapes-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).into_diagnostic()?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        std::fs::rename(&temporary, path).into_diagnostic()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
