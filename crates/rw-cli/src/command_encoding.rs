//! Compact command bodies share the engine protocol's encoded byte ceiling.
use rw_types::{ClientCommand, MAX_COMMAND_BODY_BYTES, json_encoding::JsonWriter};

pub(crate) fn encode(command: &ClientCommand) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    JsonWriter::buffer(&mut bytes, MAX_COMMAND_BODY_BYTES, 1024)
        .map_err(serde_json::Error::io)?
        .serialize(command)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use rw_types::{CommandMeta, PROTOCOL_VERSION, RequestId};

    #[test]
    fn shutdown_command_encoding_preserves_exact_compact_bytes() {
        let command = ClientCommand::ShutdownHost {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: rw_types::ClientId("client\n\"\\🦀".into()),
                request_id: RequestId("remote-supervisor-shutdown".into()),
            },
        };
        let expected = serde_json::to_vec(&command).expect("reference wire bytes");
        let encoded = encode(&command).expect("bounded command");
        assert_eq!(encoded, expected);
        assert_eq!(
            serde_json::from_slice::<ClientCommand>(&encoded).expect("wire command"),
            command
        );
    }

    #[test]
    fn control_command_rejects_encoded_overflow_as_an_io_error() {
        let command = ClientCommand::ShutdownHost {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: rw_types::ClientId("\0".repeat(MAX_COMMAND_BODY_BYTES / 6)),
                request_id: RequestId("remote-supervisor-shutdown".into()),
            },
        };
        let error = encode(&command).expect_err("escaped bytes exceed wire cap");
        assert!(error.is_io());
        assert_eq!(error.io_error_kind(), Some(std::io::ErrorKind::Other));
    }

    #[test]
    fn development_and_shell_commands_preserve_compact_wire_semantics() {
        let meta = CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: rw_types::ClientId("client".into()),
            request_id: RequestId("request".into()),
        };
        let session_id = rw_types::SessionId("session".into());
        let commands = [
            ClientCommand::AttachDevelopmentPlugin {
                meta: meta.clone(),
                session_id: session_id.clone(),
                source: "/workspace/escaped\"🦀.ts".into(),
            },
            ClientCommand::DetachDevelopmentPlugin {
                meta: meta.clone(),
                session_id: session_id.clone(),
            },
            ClientCommand::UserShellEnded {
                meta,
                session_id,
                shell_id: rw_types::ShellId("shell".into()),
                status: 127,
                captured_output: Some("\0\n\r\t\\\"🦀".into()),
            },
        ];
        for command in commands {
            let expected = serde_json::to_vec(&command).expect("reference command");
            let actual = encode(&command).expect("bounded command");
            assert_eq!(actual, expected);
            assert_eq!(
                serde_json::from_slice::<ClientCommand>(&actual).expect("wire command"),
                command
            );
        }
    }
}
