//! Bounded, read-only historical usage and cost reporting.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
};

use miette::{Result, miette};
use rw_core::{AccountingAttribution, Cost, EngineEvent};
use rw_store::session::{AccountingLedger, TurnAccountingEntry, UtcDayKey, UtcTimestamp};
use serde::Serialize;

use rw_runtime::session_history;

const MAX_STATS_SESSIONS: usize = 10_000;
const MAX_STATS_HISTORY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STATS_HISTORY_EVENTS: usize = 1_000_000;
const MAX_STATS_ACCOUNTING_ENTRIES: usize = 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatsQuery {
    pub(crate) session: Option<String>,
    pub(crate) from_day: Option<String>,
    pub(crate) through_day: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_field_names)]
pub(crate) struct UsageTotals {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_tokens: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct CostTotals {
    /// USD subtotal for entries whose provider returned an ordinary API price.
    pub(crate) known_usd_micros: u64,
    pub(crate) ai_credit_micros: u64,
    pub(crate) subscription_quota_entries: u64,
    pub(crate) unavailable_entries: u64,
    pub(crate) non_usd_monetary_entries: u64,
    /// False when some entries cannot be represented by the known USD subtotal.
    pub(crate) usd_cost_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AttributionTotals {
    pub(crate) attribution: AccountingAttribution,
    pub(crate) accounting_entries: u64,
    pub(crate) usage: UsageTotals,
    pub(crate) cost: CostTotals,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolUseTotal {
    pub(crate) name: String,
    pub(crate) count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StatsReport {
    pub(crate) schema_version: u16,
    pub(crate) scope_session_id: Option<String>,
    pub(crate) from_utc: String,
    pub(crate) through_utc: String,
    pub(crate) sessions: u64,
    pub(crate) accounting_entries: u64,
    pub(crate) usage: UsageTotals,
    pub(crate) cost: CostTotals,
    pub(crate) attribution: Vec<AttributionTotals>,
    pub(crate) tool_uses: Vec<ToolUseTotal>,
}

#[derive(Debug)]
struct SessionFacts {
    id: String,
    children: BTreeSet<String>,
    tool_uses: BTreeMap<String, u64>,
    accounting: Vec<TurnAccountingEntry>,
}

pub(crate) fn collect(storage_root: &Path, query: &StatsQuery) -> Result<StatsReport> {
    let (start, end) = parse_range(query)?;
    let facts = load_session_facts(storage_root, &start, &end)?;
    let all_session_ids = facts
        .iter()
        .map(|fact| fact.id.clone())
        .collect::<BTreeSet<_>>();
    let child_parents = validate_session_graph(&facts)?;
    let selected_sessions = if let Some(root) = query.session.as_deref() {
        validate_session_id(root)?;
        if !all_session_ids.contains(root) {
            return Err(miette!("stats session {root:?} does not exist"));
        }
        descendants(root, &facts)?
    } else {
        all_session_ids.clone()
    };

    let entries = AccountingLedger::entries_read_only_bounded(
        storage_root,
        &start,
        &end,
        MAX_STATS_ACCOUNTING_ENTRIES,
    )
    .map_err(|error| miette!("historical accounting could not be read: {error}"))?;
    validate_accounting_projection(&facts, &selected_sessions, &entries)?;
    let mut by_attribution = [
        empty_attribution(AccountingAttribution::Main),
        empty_attribution(AccountingAttribution::Compaction),
        empty_attribution(AccountingAttribution::Subagent),
        empty_attribution(AccountingAttribution::Title),
    ];
    for entry in entries {
        if !selected_sessions.contains(&entry.session_id) {
            continue;
        }
        let attribution = if query
            .session
            .as_deref()
            .is_some_and(|root| entry.session_id != root)
            || (query.session.is_none() && child_parents.contains_key(&entry.session_id))
        {
            AccountingAttribution::Subagent
        } else {
            entry.attribution
        };
        add_entry(
            attribution_bucket(&mut by_attribution, &attribution),
            &entry.usage,
            &entry.cost,
        )?;
    }

    let mut tool_uses = BTreeMap::<String, u64>::new();
    for fact in &facts {
        if !selected_sessions.contains(&fact.id) {
            continue;
        }
        for (name, count) in &fact.tool_uses {
            checked_add(tool_uses.entry(name.clone()).or_default(), *count)?;
        }
    }
    let mut tool_uses = tool_uses
        .into_iter()
        .map(|(name, count)| ToolUseTotal { name, count })
        .collect::<Vec<_>>();
    tool_uses.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.name.cmp(&right.name))
    });

    let mut usage = UsageTotals::default();
    let mut cost = CostTotals {
        usd_cost_complete: true,
        ..CostTotals::default()
    };
    let mut accounting_entries = 0_u64;
    for bucket in &by_attribution {
        add_usage(&mut usage, &bucket.usage)?;
        add_cost_totals(&mut cost, &bucket.cost)?;
        checked_add(&mut accounting_entries, bucket.accounting_entries)?;
    }
    Ok(StatsReport {
        schema_version: 1,
        scope_session_id: query.session.clone(),
        from_utc: start.to_string(),
        through_utc: end.to_string(),
        sessions: u64::try_from(selected_sessions.len())
            .map_err(|_| miette!("stats session count overflow"))?,
        accounting_entries,
        usage,
        cost,
        attribution: by_attribution.into_iter().collect(),
        tool_uses,
    })
}

fn parse_range(query: &StatsQuery) -> Result<(UtcTimestamp, UtcTimestamp)> {
    let start_day = UtcDayKey::parse(query.from_day.as_deref().unwrap_or("0001-01-01"))
        .map_err(|error| miette!("--from must be a valid UTC YYYY-MM-DD date: {error}"))?;
    let end_day = UtcDayKey::parse(query.through_day.as_deref().unwrap_or("9999-12-31"))
        .map_err(|error| miette!("--to must be a valid UTC YYYY-MM-DD date: {error}"))?;
    if start_day > end_day {
        return Err(miette!("--from must not be later than --to"));
    }
    let start = UtcTimestamp::parse(format!("{start_day}T00:00:00.000Z"))
        .map_err(|error| miette!(error.to_string()))?;
    let end = UtcTimestamp::parse(format!("{end_day}T23:59:59.999Z"))
        .map_err(|error| miette!(error.to_string()))?;
    Ok((start, end))
}

fn load_session_facts(
    storage_root: &Path,
    start: &UtcTimestamp,
    end: &UtcTimestamp,
) -> Result<Vec<SessionFacts>> {
    let sessions_root = storage_root.join("sessions");
    let metadata = fs::symlink_metadata(&sessions_root)
        .map_err(|_| miette!("session storage could not be read"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(miette!("session storage is not a real directory"));
    }
    let mut ids = Vec::new();
    for entry in
        fs::read_dir(&sessions_root).map_err(|_| miette!("session storage could not be read"))?
    {
        let entry = entry.map_err(|_| miette!("session storage could not be read"))?;
        if !entry
            .file_type()
            .map_err(|_| miette!("session storage could not be read"))?
            .is_dir()
        {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        validate_session_id(&id)?;
        ids.push(id);
        if ids.len() > MAX_STATS_SESSIONS {
            return Err(miette!(
                "stats exceeds the {MAX_STATS_SESSIONS}-session read limit"
            ));
        }
    }
    ids.sort();
    let mut total_bytes = 0_u64;
    let mut total_events = 0_usize;
    let mut facts = Vec::with_capacity(ids.len());
    for id in ids {
        let remaining = MAX_STATS_HISTORY_BYTES.saturating_sub(total_bytes);
        let (inherited_through, metadata_bytes) =
            inherited_accounting_boundary(storage_root, &id, remaining)?;
        add_history_scan_totals(&mut total_bytes, &mut total_events, metadata_bytes, 0)?;
        let remaining = MAX_STATS_HISTORY_BYTES.saturating_sub(total_bytes);
        let (events, event_bytes) =
            session_history::load_events_with_size(storage_root, &id, remaining)?;
        add_history_scan_totals(
            &mut total_bytes,
            &mut total_events,
            event_bytes,
            events.len(),
        )?;
        facts.push(project_session_facts(
            id,
            events,
            start,
            end,
            inherited_through,
        )?);
    }
    Ok(facts)
}

fn project_session_facts(
    id: String,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
    start: &UtcTimestamp,
    end: &UtcTimestamp,
    inherited_through: Option<rw_core::SequenceId>,
) -> Result<SessionFacts> {
    let mut children = BTreeSet::new();
    let mut tool_uses = BTreeMap::<String, u64>::new();
    let mut accounting = Vec::new();
    for envelope in events {
        let meta = envelope
            .event
            .meta()
            .ok_or_else(|| miette!("session history contains a non-durable event"))?;
        let emitted_at = UtcTimestamp::parse(meta.emitted_at.clone()).map_err(|error| {
            miette!("session {id:?} contains an invalid UTC event timestamp: {error}")
        })?;
        let in_range = emitted_at >= *start && emitted_at <= *end;
        let is_owned = inherited_through.is_none_or(|boundary| meta.sequence_id > boundary);
        match envelope.event {
            EngineEvent::SubagentSpawned {
                child_session_id, ..
            } if is_owned => {
                children.insert(child_session_id.0);
            }
            EngineEvent::ToolCallStarted { name, .. } if in_range && is_owned => {
                checked_add(tool_uses.entry(name).or_default(), 1)?;
            }
            EngineEvent::TurnFinished {
                meta,
                turn_id,
                usage,
                cost,
                ..
            } if in_range && is_owned => {
                accounting.push(accounting_entry(&id, meta, turn_id, usage, cost)?);
            }
            EngineEvent::CompactionAttemptFinished {
                meta,
                summary_turn_id,
                usage,
                cost,
            } if in_range && is_owned => {
                let mut entry = accounting_entry(&id, meta, summary_turn_id, usage, cost)?;
                entry.attribution = AccountingAttribution::Compaction;
                accounting.push(entry);
            }
            EngineEvent::CompactionFinished {
                meta,
                summary_turn_id,
                usage: Some(usage),
                cost: Some(cost),
                ..
            } if in_range && is_owned => {
                let mut entry = accounting_entry(&id, meta, summary_turn_id, usage, cost)?;
                entry.attribution = AccountingAttribution::Compaction;
                accounting.push(entry);
            }
            _ => {}
        }
    }
    Ok(SessionFacts {
        id,
        children,
        tool_uses,
        accounting,
    })
}

fn inherited_accounting_boundary(
    storage_root: &Path,
    session_id: &str,
    max_bytes: u64,
) -> Result<(Option<rw_core::SequenceId>, u64)> {
    let path = storage_root
        .join("sessions")
        .join(session_id)
        .join("metadata.json");
    match fs::symlink_metadata(path) {
        Ok(_) => rw_runtime::session::load_inherited_accounting_boundary_bounded(
            storage_root,
            session_id,
            max_bytes,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((None, 0)),
        Err(_) => Err(miette!("session metadata could not be inspected")),
    }
}

fn accounting_entry(
    session_id: &str,
    meta: rw_core::EventMeta,
    turn_id: rw_core::TurnId,
    usage: rw_core::Usage,
    cost: Cost,
) -> Result<TurnAccountingEntry> {
    let emitted_at_utc = UtcTimestamp::parse(meta.emitted_at)
        .map_err(|error| miette!("session accounting timestamp is invalid: {error}"))?;
    Ok(TurnAccountingEntry {
        session_id: session_id.to_owned(),
        turn_id,
        sequence_id: meta.sequence_id,
        utc_day: emitted_at_utc.utc_day(),
        emitted_at_utc,
        attribution: AccountingAttribution::Main,
        usage,
        cost,
    })
}

fn validate_accounting_projection(
    facts: &[SessionFacts],
    selected_sessions: &BTreeSet<String>,
    entries: &[TurnAccountingEntry],
) -> Result<()> {
    let expected = facts
        .iter()
        .filter(|fact| selected_sessions.contains(&fact.id))
        .flat_map(|fact| fact.accounting.iter())
        .map(|entry| ((entry.session_id.as_str(), entry.sequence_id.0), entry))
        .collect::<BTreeMap<_, _>>();
    let actual = entries
        .iter()
        .filter(|entry| selected_sessions.contains(&entry.session_id))
        .map(|entry| ((entry.session_id.as_str(), entry.sequence_id.0), entry))
        .collect::<BTreeMap<_, _>>();
    if expected != actual {
        return Err(miette!(
            "historical accounting projection is stale or conflicts with its authoritative event logs"
        ));
    }
    Ok(())
}

fn add_history_scan_totals(
    total_bytes: &mut u64,
    total_events: &mut usize,
    bytes: u64,
    events: usize,
) -> Result<()> {
    *total_bytes = total_bytes
        .checked_add(bytes)
        .ok_or_else(|| miette!("stats history byte count overflow"))?;
    if *total_bytes > MAX_STATS_HISTORY_BYTES {
        return Err(miette!(
            "stats exceeds the {MAX_STATS_HISTORY_BYTES}-byte history read limit"
        ));
    }
    *total_events = total_events
        .checked_add(events)
        .ok_or_else(|| miette!("stats history event count overflow"))?;
    if *total_events > MAX_STATS_HISTORY_EVENTS {
        return Err(miette!(
            "stats exceeds the {MAX_STATS_HISTORY_EVENTS}-event history read limit"
        ));
    }
    Ok(())
}

fn validate_session_graph(facts: &[SessionFacts]) -> Result<HashMap<String, String>> {
    let mut parents = HashMap::new();
    for fact in facts {
        for child in &fact.children {
            if child == &fact.id {
                return Err(miette!("subagent session graph contains a self-cycle"));
            }
            if let Some(previous) = parents.insert(child.clone(), fact.id.clone())
                && previous != fact.id
            {
                return Err(miette!(
                    "subagent session {child:?} has more than one durable parent"
                ));
            }
        }
    }
    for id in parents.keys() {
        let mut seen = HashSet::new();
        let mut cursor = id.as_str();
        while let Some(parent) = parents.get(cursor) {
            if !seen.insert(cursor.to_owned()) {
                return Err(miette!("subagent session graph contains a cycle"));
            }
            cursor = parent;
        }
    }
    Ok(parents)
}

fn descendants(root: &str, facts: &[SessionFacts]) -> Result<BTreeSet<String>> {
    let children = facts
        .iter()
        .map(|fact| (fact.id.as_str(), &fact.children))
        .collect::<HashMap<_, _>>();
    let mut selected = BTreeSet::from([root.to_owned()]);
    let mut pending = vec![root.to_owned()];
    while let Some(parent) = pending.pop() {
        if let Some(nested) = children.get(parent.as_str()) {
            for child in *nested {
                if selected.insert(child.clone()) {
                    pending.push(child.clone());
                    if selected.len() > MAX_STATS_SESSIONS {
                        return Err(miette!("subagent session graph exceeds the session limit"));
                    }
                }
            }
        }
    }
    Ok(selected)
}

fn validate_session_id(value: &str) -> Result<()> {
    rw_core::SessionId::validate(value)
        .map_err(|_| miette!("stats session id is empty, too long, or unsafe"))
}

fn empty_attribution(attribution: AccountingAttribution) -> AttributionTotals {
    AttributionTotals {
        attribution,
        accounting_entries: 0,
        usage: UsageTotals::default(),
        cost: CostTotals {
            usd_cost_complete: true,
            ..CostTotals::default()
        },
    }
}

fn attribution_bucket<'a>(
    buckets: &'a mut [AttributionTotals; 4],
    attribution: &AccountingAttribution,
) -> &'a mut AttributionTotals {
    let index = match attribution {
        AccountingAttribution::Main => 0,
        AccountingAttribution::Compaction => 1,
        AccountingAttribution::Subagent => 2,
        AccountingAttribution::Title => 3,
    };
    &mut buckets[index]
}

fn add_entry(target: &mut AttributionTotals, usage: &rw_core::Usage, cost: &Cost) -> Result<()> {
    checked_add(&mut target.accounting_entries, 1)?;
    checked_add(&mut target.usage.input_tokens, usage.input_tokens)?;
    checked_add(&mut target.usage.output_tokens, usage.output_tokens)?;
    checked_add(&mut target.usage.cache_read_tokens, usage.cache_read_tokens)?;
    checked_add(
        &mut target.usage.cache_write_tokens,
        usage.cache_write_tokens,
    )?;
    checked_add(&mut target.usage.reasoning_tokens, usage.reasoning_tokens)?;
    match cost {
        Cost::Monetary {
            amount_micros,
            currency,
        } if currency.eq_ignore_ascii_case("USD") => {
            checked_add(&mut target.cost.known_usd_micros, *amount_micros)?;
        }
        Cost::Monetary { .. } => {
            checked_add(&mut target.cost.non_usd_monetary_entries, 1)?;
            target.cost.usd_cost_complete = false;
        }
        Cost::AiCredits { credits_micros, .. } => {
            checked_add(&mut target.cost.ai_credit_micros, *credits_micros)?;
            target.cost.usd_cost_complete = false;
        }
        Cost::SubscriptionQuota { .. } => {
            checked_add(&mut target.cost.subscription_quota_entries, 1)?;
            target.cost.usd_cost_complete = false;
        }
        Cost::Unavailable { .. } => {
            checked_add(&mut target.cost.unavailable_entries, 1)?;
            target.cost.usd_cost_complete = false;
        }
    }
    Ok(())
}

fn add_usage(target: &mut UsageTotals, source: &UsageTotals) -> Result<()> {
    checked_add(&mut target.input_tokens, source.input_tokens)?;
    checked_add(&mut target.output_tokens, source.output_tokens)?;
    checked_add(&mut target.cache_read_tokens, source.cache_read_tokens)?;
    checked_add(&mut target.cache_write_tokens, source.cache_write_tokens)?;
    checked_add(&mut target.reasoning_tokens, source.reasoning_tokens)
}

fn add_cost_totals(target: &mut CostTotals, source: &CostTotals) -> Result<()> {
    checked_add(&mut target.known_usd_micros, source.known_usd_micros)?;
    checked_add(&mut target.ai_credit_micros, source.ai_credit_micros)?;
    checked_add(
        &mut target.subscription_quota_entries,
        source.subscription_quota_entries,
    )?;
    checked_add(&mut target.unavailable_entries, source.unavailable_entries)?;
    checked_add(
        &mut target.non_usd_monetary_entries,
        source.non_usd_monetary_entries,
    )?;
    target.usd_cost_complete &= source.usd_cost_complete;
    Ok(())
}

fn checked_add(target: &mut u64, value: u64) -> Result<()> {
    *target = target
        .checked_add(value)
        .ok_or_else(|| miette!("stats total overflow"))?;
    Ok(())
}

pub(crate) fn render_text(report: &StatsReport) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let scope = report.scope_session_id.as_deref().unwrap_or("all sessions");
    let _ = writeln!(output, "Rottweiler stats");
    let _ = writeln!(output, "Scope: {scope}");
    let _ = writeln!(
        output,
        "UTC range: {} through {} (inclusive)",
        report.from_utc, report.through_utc
    );
    let _ = writeln!(output, "Sessions: {}", report.sessions);
    let _ = writeln!(
        output,
        "\nCATEGORY    ENTRIES       INPUT      OUTPUT  CACHE READ CACHE WRITE   REASONING     KNOWN USD   AI CREDITS   QUOTA  UNAVAIL NON-USD"
    );
    for bucket in &report.attribution {
        let _ = writeln!(
            output,
            "{:<11} {:>7} {:>11} {:>11} {:>11} {:>11} {:>11} {:>13} {:>12} {:>7} {:>8} {:>7}",
            attribution_name(&bucket.attribution),
            bucket.accounting_entries,
            bucket.usage.input_tokens,
            bucket.usage.output_tokens,
            bucket.usage.cache_read_tokens,
            bucket.usage.cache_write_tokens,
            bucket.usage.reasoning_tokens,
            format_usd(bucket.cost.known_usd_micros),
            bucket.cost.ai_credit_micros,
            bucket.cost.subscription_quota_entries,
            bucket.cost.unavailable_entries,
            bucket.cost.non_usd_monetary_entries,
        );
    }
    let _ = writeln!(
        output,
        "{:<11} {:>7} {:>11} {:>11} {:>11} {:>11} {:>11} {:>13} {:>12} {:>7} {:>8} {:>7}",
        "total",
        report.accounting_entries,
        report.usage.input_tokens,
        report.usage.output_tokens,
        report.usage.cache_read_tokens,
        report.usage.cache_write_tokens,
        report.usage.reasoning_tokens,
        format_usd(report.cost.known_usd_micros),
        report.cost.ai_credit_micros,
        report.cost.subscription_quota_entries,
        report.cost.unavailable_entries,
        report.cost.non_usd_monetary_entries,
    );
    let _ = writeln!(
        output,
        "\nCache savings: {} input tokens served from provider cache",
        report.usage.cache_read_tokens
    );
    let completeness = if report.cost.usd_cost_complete {
        "complete"
    } else {
        "partial; quota, credits, unavailable, or non-USD entries are not $0 API cost"
    };
    let _ = writeln!(output, "Known USD subtotal: {completeness}");
    let _ = writeln!(output, "\nTOOL USES   COUNT");
    if report.tool_uses.is_empty() {
        let _ = writeln!(output, "(none)          0");
    } else {
        for tool in &report.tool_uses {
            let _ = writeln!(
                output,
                "{:<12} {}",
                safe_terminal_text(&tool.name),
                tool.count
            );
        }
    }
    output
}

fn attribution_name(attribution: &AccountingAttribution) -> &'static str {
    match attribution {
        AccountingAttribution::Main => "main",
        AccountingAttribution::Compaction => "compaction",
        AccountingAttribution::Subagent => "subagents",
        AccountingAttribution::Title => "titles",
    }
}

fn format_usd(micros: u64) -> String {
    format!("${}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn safe_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use tempfile::tempdir;

    use rw_core::{Cost, EngineEvent, EventMeta, SequenceId, SessionId, TurnId, TurnStatus, Usage};
    use rw_store::session::{
        AccountingLedger, EventEnvelope, SessionEventLog, TurnAccountingEntry, UtcTimestamp,
    };
    use rw_types::{SubagentId, ToolCallId};

    use super::{
        CostTotals, MAX_STATS_HISTORY_BYTES, MAX_STATS_HISTORY_EVENTS, StatsQuery, UsageTotals,
        add_cost_totals, add_history_scan_totals, collect, parse_range, project_session_facts,
        render_text,
    };

    #[test]
    fn utc_ranges_are_inclusive_and_reversed_ranges_fail() {
        let (start, end) = parse_range(&StatsQuery {
            session: None,
            from_day: Some("2026-07-01".to_owned()),
            through_day: Some("2026-07-31".to_owned()),
        })
        .expect("valid range");
        assert_eq!(start.as_str(), "2026-07-01T00:00:00.000Z");
        assert_eq!(end.as_str(), "2026-07-31T23:59:59.999Z");
        assert!(
            parse_range(&StatsQuery {
                session: None,
                from_day: Some("2026-08-01".to_owned()),
                through_day: Some("2026-07-31".to_owned()),
            })
            .is_err()
        );
    }

    #[test]
    fn totals_fail_closed_on_overflow() {
        let mut target = CostTotals {
            known_usd_micros: u64::MAX,
            usd_cost_complete: true,
            ..CostTotals::default()
        };
        let source = CostTotals {
            known_usd_micros: 1,
            usd_cost_complete: true,
            ..CostTotals::default()
        };
        assert!(add_cost_totals(&mut target, &source).is_err());
    }

    #[test]
    fn aggregate_history_limits_fail_before_unbounded_scans() {
        let mut bytes = MAX_STATS_HISTORY_BYTES;
        let mut events = 0;
        assert!(add_history_scan_totals(&mut bytes, &mut events, 1, 0).is_err());
        let mut bytes = 0;
        let mut events = MAX_STATS_HISTORY_EVENTS;
        assert!(add_history_scan_totals(&mut bytes, &mut events, 0, 1).is_err());
        let mut bytes = u64::MAX;
        let mut events = 0;
        assert!(add_history_scan_totals(&mut bytes, &mut events, 1, 0).is_err());
    }

    #[test]
    fn fork_inherited_prefix_contributes_no_tools_or_subagent_edges() {
        let session = "fork-child";
        let meta = |sequence| EventMeta {
            protocol_version: 1,
            session_id: SessionId(session.to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: format!("2026-07-10T00:00:0{sequence}.000Z"),
            caused_by: None,
        };
        let tool = |sequence, name: &str| EngineEvent::ToolCallStarted {
            meta: meta(sequence),
            turn_id: TurnId("1".to_owned()),
            tool_call_id: ToolCallId(format!("tool-{sequence}")),
            name: name.to_owned(),
            args: serde_json::json!({}),
            call_index: 0,
        };
        let spawn = |sequence, child: &str| EngineEvent::SubagentSpawned {
            meta: meta(sequence),
            subagent_id: SubagentId(format!("agent-{sequence}")),
            child_session_id: SessionId(child.to_owned()),
            task: "fixture".to_owned(),
        };
        let events = vec![
            tool(0, "inherited_tool"),
            spawn(1, "inherited-child"),
            tool(2, "owned_tool"),
            spawn(3, "owned-child"),
        ]
        .into_iter()
        .enumerate()
        .map(|(sequence, event)| EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(u64::try_from(sequence).expect("sequence")),
            event,
        })
        .collect();
        let facts = project_session_facts(
            session.to_owned(),
            events,
            &UtcTimestamp::parse("2026-07-10T00:00:00.000Z").expect("start"),
            &UtcTimestamp::parse("2026-07-10T23:59:59.999Z").expect("end"),
            Some(SequenceId(1)),
        )
        .expect("fork facts");
        assert_eq!(facts.tool_uses.get("inherited_tool"), None);
        assert_eq!(facts.tool_uses.get("owned_tool"), Some(&1));
        assert_eq!(
            facts.children,
            ["owned-child".to_owned()].into_iter().collect()
        );
    }

    #[test]
    fn text_never_labels_incomplete_subscription_accounting_as_zero_cost() {
        let report = super::StatsReport {
            schema_version: 1,
            scope_session_id: None,
            from_utc: "2026-01-01T00:00:00.000Z".to_owned(),
            through_utc: "2026-01-01T23:59:59.999Z".to_owned(),
            sessions: 1,
            accounting_entries: 1,
            usage: UsageTotals::default(),
            cost: CostTotals {
                subscription_quota_entries: 1,
                usd_cost_complete: false,
                ..CostTotals::default()
            },
            attribution: vec![],
            tool_uses: vec![],
        };
        let rendered = render_text(&report);
        assert!(rendered.contains("not $0 API cost"));
        assert!(rendered.contains("partial"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn historical_stats_attribute_child_sessions_without_double_counting() {
        let root = tempdir().expect("stats root");
        let parent_id = "parent";
        let child_id = "child";
        let meta = |session: &str, sequence, emitted_at: &str| EventMeta {
            protocol_version: 1,
            session_id: SessionId(session.to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: emitted_at.to_owned(),
            caused_by: None,
        };
        let parent_usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 60,
            cache_write_tokens: 5,
            reasoning_tokens: 2,
        };
        let child_usage = Usage {
            input_tokens: 50,
            output_tokens: 10,
            cache_read_tokens: 30,
            cache_write_tokens: 0,
            reasoning_tokens: 1,
        };
        let mut parent = SessionEventLog::open(root.path(), parent_id).expect("parent log");
        parent
            .append(EngineEvent::ToolCallStarted {
                meta: meta(parent_id, 0, "2026-07-10T01:00:00.000Z"),
                turn_id: TurnId("1".to_owned()),
                tool_call_id: ToolCallId("read-1".to_owned()),
                name: "read".to_owned(),
                args: serde_json::json!({}),
                call_index: 0,
            })
            .expect("parent tool");
        parent
            .append(EngineEvent::TurnFinished {
                meta: meta(parent_id, 1, "2026-07-10T01:01:00.000Z"),
                turn_id: TurnId("1".to_owned()),
                status: TurnStatus::Completed,
                usage: parent_usage.clone(),
                cost: Cost::Monetary {
                    amount_micros: 250_000,
                    currency: "USD".to_owned(),
                },
            })
            .expect("parent turn");
        parent
            .append(EngineEvent::SubagentSpawned {
                meta: meta(parent_id, 2, "2026-07-10T01:02:00.000Z"),
                subagent_id: SubagentId("explorer".to_owned()),
                child_session_id: SessionId(child_id.to_owned()),
                task: "inspect".to_owned(),
            })
            .expect("child spawn");
        drop(parent);

        let mut child = SessionEventLog::open(root.path(), child_id).expect("child log");
        child
            .append(EngineEvent::ToolCallStarted {
                meta: meta(child_id, 0, "2026-07-10T02:00:00.000Z"),
                turn_id: TurnId("1".to_owned()),
                tool_call_id: ToolCallId("read-2".to_owned()),
                name: "read".to_owned(),
                args: serde_json::json!({}),
                call_index: 0,
            })
            .expect("child tool");
        child
            .append(EngineEvent::TurnFinished {
                meta: meta(child_id, 1, "2026-07-10T02:01:00.000Z"),
                turn_id: TurnId("1".to_owned()),
                status: TurnStatus::Completed,
                usage: child_usage.clone(),
                cost: Cost::SubscriptionQuota {
                    used: Some("1".to_owned()),
                    unit: Some("request".to_owned()),
                },
            })
            .expect("child turn");
        drop(child);

        let entry = |session: &str, sequence, timestamp: &str, usage: Usage, cost: Cost| {
            let emitted_at_utc = UtcTimestamp::parse(timestamp).expect("timestamp");
            TurnAccountingEntry {
                session_id: session.to_owned(),
                turn_id: TurnId("1".to_owned()),
                sequence_id: SequenceId(sequence),
                utc_day: emitted_at_utc.utc_day(),
                emitted_at_utc,
                attribution: rw_core::AccountingAttribution::Main,
                usage,
                cost,
            }
        };
        AccountingLedger::open(root.path())
            .and_then(|ledger| {
                ledger.reconcile(&[
                    entry(
                        parent_id,
                        1,
                        "2026-07-10T01:01:00.000Z",
                        parent_usage,
                        Cost::Monetary {
                            amount_micros: 250_000,
                            currency: "USD".to_owned(),
                        },
                    ),
                    entry(
                        child_id,
                        1,
                        "2026-07-10T02:01:00.000Z",
                        child_usage,
                        Cost::SubscriptionQuota {
                            used: Some("1".to_owned()),
                            unit: Some("request".to_owned()),
                        },
                    ),
                ])
            })
            .expect("ledger fixtures");

        let report = collect(
            root.path(),
            &StatsQuery {
                session: Some(parent_id.to_owned()),
                from_day: Some("2026-07-10".to_owned()),
                through_day: Some("2026-07-10".to_owned()),
            },
        )
        .expect("stats report");
        assert_eq!(report.sessions, 2);
        assert_eq!(report.accounting_entries, 2);
        assert_eq!(report.usage.input_tokens, 150);
        assert_eq!(report.usage.cache_read_tokens, 90);
        assert_eq!(report.cost.known_usd_micros, 250_000);
        assert_eq!(report.cost.subscription_quota_entries, 1);
        assert!(!report.cost.usd_cost_complete);
        assert_eq!(report.attribution[0].accounting_entries, 1);
        assert_eq!(report.attribution[2].accounting_entries, 1);
        assert_eq!(report.tool_uses[0].name, "read");
        assert_eq!(report.tool_uses[0].count, 2);
        let json = serde_json::to_string(&report).expect("stats JSON");
        assert_eq!(
            json,
            serde_json::to_string(&report).expect("repeat stats JSON")
        );
        assert!(json.contains("\"known_usd_micros\":250000"));
        assert!(json.contains("\"subscription_quota_entries\":1"));
        assert!(json.contains("\"attribution\":\"subagent\""));

        AccountingLedger::open(root.path())
            .and_then(|ledger| ledger.replace_all(&[]))
            .expect("make projection stale");
        assert!(
            collect(
                root.path(),
                &StatsQuery {
                    session: Some(parent_id.to_owned()),
                    from_day: Some("2026-07-10".to_owned()),
                    through_day: Some("2026-07-10".to_owned()),
                },
            )
            .is_err()
        );
    }
}
