use super::{
    CITATION_DATA_FIRST, CITATION_ENCODED_LIMIT, CITATION_INDEX_FIRST, TailState,
    TranscriptIndexMutation, TranscriptProjectionError, TranscriptRowLookup, chunks, index_cell,
};
use rw_types::citation_admission::{
    MAX_CITATION_TEXT_BYTES, MAX_TURN_CITATION_TEXT_BYTES, MAX_TURN_CITATIONS,
};
use serde::Serialize;

pub(super) fn append(
    state: &mut TailState,
    source: u64,
    uri: &str,
    title: &Option<String>,
    rows: &impl TranscriptRowLookup,
) -> Result<Vec<TranscriptIndexMutation>, TranscriptProjectionError> {
    let bytes = uri
        .len()
        .saturating_add(title.as_ref().map_or(0, String::len));
    if state.citation_count >= MAX_TURN_CITATIONS
        || bytes > MAX_CITATION_TEXT_BYTES
        || state.citation_utf8_bytes.saturating_add(bytes) > MAX_TURN_CITATION_TEXT_BYTES
    {
        return Err(TranscriptProjectionError::Invalid(
            "tail citation admission",
        ));
    }
    #[derive(Serialize)]
    struct Citation<'a> {
        uri: &'a str,
        title: &'a Option<String>,
    }
    let offset = state.citation_encoded_bytes;
    let mut writer =
        chunks::CellAppender::new(CITATION_DATA_FIRST, offset, CITATION_ENCODED_LIMIT, rows)?;
    serde_json::to_writer(&mut writer, &Citation { uri, title })?;
    let (next, mut mutations) = writer.finish()?;
    let key = CITATION_INDEX_FIRST
        + u16::try_from(state.citation_count / 128)
            .map_err(|_| TranscriptProjectionError::Invalid("citation index"))?;
    let mut cell = index_cell(rows, key, state.epoch, 128)?;
    let entry = 8 + state.citation_count % 128 * 16;
    cell[entry..entry + 8].copy_from_slice(&source.to_le_bytes());
    cell[entry + 8..entry + 12].copy_from_slice(
        &u32::try_from(offset)
            .map_err(|_| TranscriptProjectionError::Invalid("citation offset"))?
            .to_le_bytes(),
    );
    cell[entry + 12..entry + 16].copy_from_slice(
        &u32::try_from(next - offset)
            .map_err(|_| TranscriptProjectionError::Invalid("citation extent"))?
            .to_le_bytes(),
    );
    mutations.push(TranscriptIndexMutation::PutAuxiliary { key, payload: cell });
    state.citation_count += 1;
    state.citation_utf8_bytes += bytes;
    state.citation_encoded_bytes = next;
    Ok(mutations)
}
