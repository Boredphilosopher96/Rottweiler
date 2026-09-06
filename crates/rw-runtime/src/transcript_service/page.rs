//! Current-view row paging; ordinal seeks never reduce arbitrary raw event pages.

use rw_core::HostError;
use rw_store::session::{
    journal::{JournalPrefixIdentity, JournalReadView},
    transcript_index::{MAX_PAGE_ROWS, TranscriptIndex, TranscriptIndexHead, TranscriptIndexRow},
};
use rw_types::{
    SequenceId, SessionId, TurnId,
    transcript::{
        TranscriptAnchor, TranscriptGeneration, TranscriptInvalidation, TranscriptItem,
        TranscriptItemId, TranscriptOrdinal, TranscriptPage, TranscriptPosition, TranscriptRead,
        TranscriptReadResult, TranscriptView,
    },
};

pub(super) fn view(session: &SessionId, head: &TranscriptIndexHead) -> TranscriptView {
    TranscriptView {
        session_id: session.clone(),
        projection_version: head.version,
        generation: TranscriptGeneration(head.generation),
        through: head.prefix.next_sequence.checked_sub(1).map(SequenceId),
        digest: head.prefix.digest,
    }
}

pub(super) fn prefix(view: &TranscriptView) -> Result<JournalPrefixIdentity, HostError> {
    Ok(JournalPrefixIdentity {
        next_sequence: match view.through {
            None => 0,
            Some(sequence) => sequence
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("view sequence overflow"))?,
        },
        digest: view.digest,
    })
}

pub(super) fn limits(request: &TranscriptRead) -> Result<(usize, usize), HostError> {
    let maximum = usize::try_from(request.max_items).map_err(|_| invalid("item limit"))?;
    let max_bytes = usize::try_from(request.max_bytes).map_err(|_| invalid("byte limit"))?;
    if !(1..=MAX_PAGE_ROWS).contains(&maximum) || !(4096..=1024 * 1024).contains(&max_bytes) {
        return Err(invalid("transcript page limits are invalid"));
    }
    Ok((maximum, max_bytes))
}

pub(super) fn read(
    index: &TranscriptIndex,
    journal: &JournalReadView,
    session: &SessionId,
    request: &TranscriptRead,
) -> Result<TranscriptReadResult, HostError> {
    let (maximum, max_bytes) = limits(request)?;
    let head = index.head().map_err(storage)?;
    let current = view(session, &head);
    let invalidation = invalidation(index, journal, request.known_view.as_ref(), &current)?;
    let (window, anchor) = position(index, &head, &request.position, maximum)?;
    if matches!(request.position, TranscriptPosition::AtOrdinal { generation, .. } if generation != current.generation)
    {
        return Ok(TranscriptReadResult::OrderingChanged { view: current });
    }
    let page = match window {
        Window::From(first) => index.page(first, maximum, max_bytes),
        Window::Before(end) => index.page_ending_before(end, maximum, max_bytes),
    }
    .map_err(storage)?;
    bounded_page(
        page.rows,
        current,
        head.total_rows,
        anchor,
        invalidation,
        window,
        max_bytes,
    )
    .map(|page| TranscriptReadResult::Ready { page })
}

fn bounded_page(
    mut rows: Vec<TranscriptIndexRow>,
    view: TranscriptView,
    total: u64,
    anchor: TranscriptAnchor,
    invalidation: TranscriptInvalidation,
    window: Window,
    maximum: usize,
) -> Result<TranscriptPage, HostError> {
    let mut page = TranscriptPage {
        view,
        // Reserve the longest ordinal while admission may still choose a different tail start.
        first_ordinal: TranscriptOrdinal(u64::MAX),
        total_items: TranscriptOrdinal(total),
        items: Vec::new(),
        anchor,
        invalidation,
    };
    let mut bytes = encoded_size(&page)?;
    if bytes >= maximum {
        page.invalidation = TranscriptInvalidation::All {};
        bytes = encoded_size(&page)?;
    }
    if matches!(window, Window::Before(_)) {
        rows.reverse();
    }
    for row in rows {
        let item = item(&row)?;
        let next = bytes
            .saturating_add(encoded_size(&item)?)
            .saturating_add(usize::from(!page.items.is_empty()));
        if next > maximum {
            if page.items.is_empty() {
                return Err(invalid("transcript page cannot fit one item"));
            }
            break;
        }
        bytes = next;
        page.items.push(item);
    }
    if matches!(window, Window::Before(_)) {
        page.items.reverse();
    }
    page.first_ordinal = page.items.first().map_or_else(
        || match window {
            Window::From(first) => TranscriptOrdinal(first),
            Window::Before(end) => TranscriptOrdinal(end),
        },
        |item| item.ordinal,
    );
    Ok(page)
}

fn encoded_size(value: &impl serde::Serialize) -> Result<usize, HostError> {
    let mut writer = rw_types::json_encoding::JsonWriter::count(usize::MAX);
    writer.serialize(value).map_err(storage)?;
    Ok(writer.written())
}

fn invalidation(
    index: &TranscriptIndex,
    journal: &JournalReadView,
    known: Option<&TranscriptView>,
    current: &TranscriptView,
) -> Result<TranscriptInvalidation, HostError> {
    let Some(known) = known else {
        return Ok(TranscriptInvalidation::All {});
    };
    if known.session_id != current.session_id {
        return Err(invalid("transcript view belongs to another session"));
    }
    if known == current {
        return Ok(TranscriptInvalidation::None {});
    }
    journal.at_prefix(prefix(known)?).map_err(storage)?;
    if known.generation != current.generation
        || known.projection_version != current.projection_version
    {
        return Ok(TranscriptInvalidation::All {});
    }
    let Some(through) = known.through else {
        return Ok(TranscriptInvalidation::All {});
    };
    let Some(keys) = index
        .changed_keys(through, MAX_PAGE_ROWS)
        .map_err(storage)?
    else {
        return Ok(TranscriptInvalidation::All {});
    };
    let items = keys
        .into_iter()
        .map(|key| {
            key.strip_prefix("item:")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|value| TranscriptItemId(SequenceId(value)))
                .ok_or_else(|| invalid("invalid stored transcript identity"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TranscriptInvalidation::Items { items })
}

#[derive(Clone, Copy)]
enum Window {
    From(u64),
    Before(u64),
}

fn position(
    index: &TranscriptIndex,
    head: &TranscriptIndexHead,
    position: &TranscriptPosition,
    maximum: usize,
) -> Result<(Window, TranscriptAnchor), HostError> {
    let count = u64::try_from(maximum).map_err(|_| invalid("item limit"))?;
    match position {
        TranscriptPosition::First {} => Ok((Window::From(0), TranscriptAnchor::Unspecified {})),
        TranscriptPosition::Latest {} => Ok((
            Window::Before(head.total_rows),
            TranscriptAnchor::Unspecified {},
        )),
        TranscriptPosition::AtOrdinal { ordinal, .. } => Ok((
            Window::From(ordinal.0.min(head.total_rows.saturating_sub(1))),
            TranscriptAnchor::Unspecified {},
        )),
        TranscriptPosition::Before { item }
        | TranscriptPosition::After { item }
        | TranscriptPosition::Around { item } => {
            let key = format!("item:{}", item.0.0);
            let exact = index.row(&key).map_err(storage)?;
            let (row, anchor) = if let Some(row) = exact {
                (Some(row), TranscriptAnchor::Exact { item: *item })
            } else {
                let replacement = index.at_or_before_source(item.0).map_err(storage)?;
                let anchor = TranscriptAnchor::Replaced {
                    requested: *item,
                    replacement: replacement.as_ref().map(|row| TranscriptItemId(row.source)),
                };
                (replacement, anchor)
            };
            let first = match (position, row) {
                (TranscriptPosition::Before { .. }, Some(row)) => Window::Before(row.ordinal),
                (TranscriptPosition::After { .. }, Some(row)) => {
                    Window::From(row.ordinal.saturating_add(1).min(head.total_rows))
                }
                (_, Some(row)) => Window::From(row.ordinal.saturating_sub(count / 2)),
                _ => Window::From(0),
            };
            Ok((first, anchor))
        }
    }
}

fn item(row: &TranscriptIndexRow) -> Result<TranscriptItem, HostError> {
    Ok(TranscriptItem {
        id: TranscriptItemId(row.source),
        ordinal: TranscriptOrdinal(row.ordinal),
        revision: row.revision,
        agent_turn: row.agent_turn.map(|turn| TurnId(turn.to_string())),
        content: serde_json::from_slice(&row.payload).map_err(storage)?,
    })
}

pub(super) fn storage(error: impl std::fmt::Display) -> HostError {
    HostError::Query(format!("transcript storage: {error}"))
}
fn invalid(message: &str) -> HostError {
    HostError::Protocol(message.to_owned())
}
