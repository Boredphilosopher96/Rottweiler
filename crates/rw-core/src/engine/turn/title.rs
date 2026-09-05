use crate::engine::SESSION_TITLE_MAX_CHARS;
use crate::engine::SESSION_TITLE_OUTPUT_CHARS;
use crate::engine::SESSION_TITLE_PROMPT_CHARS;
use crate::engine::SESSION_TITLE_TIMEOUT;
use crate::engine::SessionUsage;
use crate::engine::model::ModelDriver;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::hooks::mark_unsettled;
use crate::engine::turn::provider_calls;
use crate::engine::turn::signals::TurnSignal;
use futures_util::StreamExt;
use rw_providers::ProviderEvent;
use rw_providers::ProviderRequest;
use rw_providers::ToolChoice;
use rw_tools::CancellationToken;
use rw_types::AccountingAttribution;
use rw_types::Block;
use rw_types::Cost;
use rw_types::Role;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::ThinkingLevel;
use std::sync::Arc;
use tokio::sync::mpsc;

fn first_meaningful_user_prompt(conversation: &[Turn]) -> Option<String> {
    conversation.iter().find_map(|turn| {
        if turn.role != Role::User || turn.meta.synthetic {
            return None;
        }
        let text = turn
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!collapsed.is_empty()).then_some(collapsed)
    })
}

fn has_successful_assistant_text(conversation: &[Turn]) -> bool {
    conversation.iter().rev().any(|turn| {
        turn.role == Role::Assistant
            && turn
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if !text.trim().is_empty()))
    })
}

fn deterministic_session_title(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = collapsed
        .chars()
        .take(SESSION_TITLE_MAX_CHARS)
        .collect::<String>();
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
}

pub(super) fn normalize_generated_session_title(raw: &str) -> Option<String> {
    let first = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let unquoted = first
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '#' | '*' | '_'))
        .trim()
        .trim_end_matches(['.', ':', ';']);
    if unquoted.is_empty()
        || unquoted.chars().count() > SESSION_TITLE_MAX_CHARS
        || unquoted.chars().any(char::is_control)
    {
        return None;
    }
    Some(unquoted.to_owned())
}

pub(in crate::engine) fn normalize_manual_session_title(raw: &str) -> Option<String> {
    if raw.chars().any(char::is_control) {
        return None;
    }
    let title = raw.trim();
    if title.is_empty() || title.chars().count() > SESSION_TITLE_MAX_CHARS {
        return None;
    }
    Some(title.to_owned())
}

fn unavailable_session_title() -> (Option<String>, SessionUsage, Cost) {
    (
        None,
        SessionUsage::default(),
        Cost::Unavailable {
            reason: "session title generation was unavailable".to_owned(),
        },
    )
}

async fn generate_session_title(
    model: Arc<dyn ModelDriver>,
    alias: String,
    prompt: String,
    config: Arc<SessionActorConfig>,
    signals: mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    cancellation: CancellationToken,
) -> (Option<String>, SessionUsage, Cost) {
    if model.prepare_model(&alias).await.is_err() {
        return unavailable_session_title();
    }
    let prompt = prompt
        .chars()
        .take(SESSION_TITLE_PROMPT_CHARS)
        .collect::<String>();
    let request = title_request(&alias, prompt);
    let Ok(invocation) = provider_calls::invocation(
        &config,
        &signals,
        turn,
        AccountingAttribution::Title,
        &request,
    ) else {
        return unavailable_session_title();
    };
    let Ok(mut stream) = model.stream(&alias, request, invocation) else {
        return unavailable_session_title();
    };
    let collect = async {
        let mut title = String::new();
        let mut usage = SessionUsage::default();
        let mut reported_model = None;
        let mut selected_route = None;
        while let Some(event) = stream.next().await {
            let Ok(event) = event else { return None };
            match event {
                ProviderEvent::RouteSelected { route } => selected_route = Some(route),
                ProviderEvent::MessageStart { model } => reported_model = Some(model),
                ProviderEvent::TextDelta { text } => {
                    if title.chars().count().saturating_add(text.chars().count())
                        > SESSION_TITLE_OUTPUT_CHARS
                    {
                        return None;
                    }
                    title.push_str(&text);
                }
                ProviderEvent::ToolCallStart { .. }
                | ProviderEvent::ToolCallArgumentsDelta { .. }
                | ProviderEvent::ToolCallEnd { .. } => return None,
                ProviderEvent::Usage { usage: latest } => usage.update(latest),
                _ => {}
            }
        }
        let title = normalize_generated_session_title(&title)?;
        let cost = model.cost_for_route(
            &alias,
            selected_route.as_deref(),
            reported_model.as_deref(),
            usage.into(),
        );
        Some((title, usage, cost))
    };
    let result = tokio::select! {
        result = tokio::time::timeout(SESSION_TITLE_TIMEOUT, collect) => Some(result),
        () = cancellation.cancelled() => None,
    };
    drop(stream);
    if let Err(error) = model.settle_effects().await {
        mark_unsettled(&signals, &cancellation, error.to_string());
        return unavailable_session_title();
    }
    let Some(result) = result else {
        return unavailable_session_title();
    };
    match result {
        Ok(Some((title, usage, cost))) => (Some(title), usage, cost),
        Ok(None) => (
            None,
            SessionUsage::default(),
            Cost::Unavailable {
                reason: "session title generation failed".to_owned(),
            },
        ),
        Err(_) => (
            None,
            SessionUsage::default(),
            Cost::Unavailable {
                reason: "session title generation timed out".to_owned(),
            },
        ),
    }
}

pub(super) fn start_session_title_generation(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) {
    if state.session_title.is_some() || state.title_generation_started {
        return;
    }
    let Some(prompt) = first_meaningful_user_prompt(&state.conversation) else {
        return;
    };
    if !has_successful_assistant_text(&state.conversation) {
        return;
    }
    state.title_generation_started = true;
    let fallback = deterministic_session_title(&prompt);
    let model = Arc::clone(&config.model);
    let budget = model.budget_config();
    let hard_cap_configured = budget.session_cost_cap_micros_usd.is_some()
        || budget.daily_cost_cap_micros_usd.is_some()
        || budget.session_ai_credit_cap_micros.is_some()
        || budget.daily_ai_credit_cap_micros.is_some()
        || budget.session_token_cap.is_some()
        || budget.daily_token_cap.is_some();
    // Background metadata must never race an ordinary turn past a hard cap.
    // Use the deterministic title in capped sessions; uncapped calls are
    // durably accounted when their result is persisted.
    let alias = (!hard_cap_configured)
        .then(|| model.title_model_alias())
        .flatten();
    let signals = signals.clone();
    let config = Arc::clone(config);
    let turn = state.next_turn.saturating_sub(1);
    let cancellation = CancellationToken::default();
    let errors = signals.clone();
    if let Err(error) = state
        .tasks
        .spawn(Arc::clone(&config), cancellation.clone(), async move {
            let (title, usage, cost) = match alias {
                Some(alias) => {
                    let (title, usage, cost) = generate_session_title(
                        model,
                        alias,
                        prompt,
                        config,
                        signals.clone(),
                        turn,
                        cancellation,
                    )
                    .await;
                    (title.unwrap_or(fallback), Some(usage), Some(cost))
                }
                None => (fallback, None, None),
            };
            let _ = signals.send(TurnSignal::SessionTitleGenerated { title, usage, cost });
        })
    {
        let _ = errors.send(TurnSignal::EffectsUnsettled {
            message: error.to_string(),
        });
    }
}

fn title_request(alias: &str, prompt: String) -> ProviderRequest {
    ProviderRequest {
        model: alias.to_owned(),
        turns: vec![
            Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: "Name this coding session in 3 to 7 plain words. Return only the title, with no quotes, punctuation, markdown, or explanation.".to_owned(),
                }],
                meta: TurnMeta {
                    synthetic: true,
                    ..TurnMeta::default()
                },
            },
            Turn {
                role: Role::User,
                blocks: vec![Block::Text { text: prompt }],
                meta: TurnMeta {
                    synthetic: true,
                    ..TurnMeta::default()
                },
            },
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None {},
        max_output_tokens: 32,
        temperature: Some(0.0),
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}
