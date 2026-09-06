use super::{MAX_SESSION_METADATA_BYTES, SessionMetadata};
use miette::{IntoDiagnostic as _, Result};
use rw_types::json_encoding::JsonWriter;

pub(crate) fn encode(metadata: &SessionMetadata) -> Result<Vec<u8>> {
    let limit = usize::try_from(MAX_SESSION_METADATA_BYTES).into_diagnostic()?;
    let mut counter = JsonWriter::count(limit);
    counter
        .serialize(metadata)
        .map_err(|cause| {
            encoding_error(
                cause,
                counter.exceeded(),
                "session metadata exceeds its byte limit",
            )
        })
        .into_diagnostic()?;
    let measured = counter.written();
    let mut output = Vec::with_capacity(measured);
    let mut writer = JsonWriter::buffer(&mut output, measured, 0).into_diagnostic()?;
    writer
        .serialize(metadata)
        .map_err(|cause| {
            encoding_error(
                cause,
                writer.exceeded(),
                "session metadata exceeded its measured size",
            )
        })
        .into_diagnostic()?;
    Ok(output)
}

fn encoding_error(
    cause: serde_json::Error,
    exceeded: bool,
    message: &'static str,
) -> serde_json::Error {
    if exceeded {
        serde_json::Error::io(std::io::Error::other(message))
    } else {
        cause
    }
}
