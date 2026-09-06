//! Private supervised WASM runtime. It is never a public application entrypoint.

use std::{io::Write as _, process::ExitCode};

use rw_ext::{
    MAX_WASM_HOST_HEADER_BYTES, MAX_WASM_HOST_RESPONSE_BYTES, WasmHookHost, WasmHostRequest,
    WasmHostResponse,
};
use tokio::io::AsyncReadExt;

const ABSOLUTE_MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;

// One worker executes one request at a time. Compiled code is reused; stores are not.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = write_response(&WasmHostResponse::Error { message });
            ExitCode::SUCCESS
        }
    }
}

async fn run() -> Result<(), String> {
    let mut stdin = tokio::io::stdin();
    let mut host: Option<WasmHookHost> = None;
    loop {
        // Clean EOF between requests is normal owner shutdown. Partial headers fail.
        let mut first = [0; 1];
        if stdin.read(&mut first).await.map_err(io_message)? == 0 {
            return Ok(());
        }
        let mut rest = [0; 3];
        stdin.read_exact(&mut rest).await.map_err(io_message)?;
        let header_len = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
        let component_len = stdin.read_u32().await.map_err(io_message)? as usize;
        if header_len > MAX_WASM_HOST_HEADER_BYTES || component_len > ABSOLUTE_MAX_COMPONENT_BYTES {
            return Err("WASM helper request exceeds its wire limits".to_owned());
        }
        let mut header = vec![0; header_len];
        stdin.read_exact(&mut header).await.map_err(io_message)?;
        let request: WasmHostRequest = serde_json::from_slice(&header)
            .map_err(|_| "WASM helper request is malformed".to_owned())?;
        let mut component = vec![0; component_len];
        stdin.read_exact(&mut component).await.map_err(io_message)?;
        let response = match request {
            WasmHostRequest::Load { manifest, limits } => {
                // Failed replacement must never retain the previous generation.
                host = None;
                match WasmHookHost::from_bytes(*manifest, &component, limits) {
                    Ok(compiled) => {
                        host = Some(compiled);
                        WasmHostResponse::Valid {}
                    }
                    Err(error) => WasmHostResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            WasmHostRequest::Invoke { event, input } => {
                if component_len != 0 {
                    return Err("WASM invocation cannot replace component bytes".to_owned());
                }
                let Some(compiled) = host.as_ref() else {
                    return Err("WASM worker has no compiled generation".to_owned());
                };
                match compiled.invoke_json(&event, &input).await {
                    Ok(directive) => WasmHostResponse::Directive { directive },
                    Err(error) => WasmHostResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
        };
        write_response(&response)?;
    }
}

fn write_response(response: &WasmHostResponse) -> Result<(), String> {
    let bytes = serde_json::to_vec(response)
        .map_err(|_| "WASM helper response could not encode".to_owned())?;
    if bytes.len() > MAX_WASM_HOST_RESPONSE_BYTES {
        return Err("WASM helper response exceeds its wire limit".to_owned());
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| "WASM helper response exceeds its wire limit".to_owned())?;
    let mut frame = Vec::with_capacity(bytes.len() + size_of::<u32>());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&bytes);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&frame).map_err(io_message)?;
    stdout.flush().map_err(io_message)
}

fn io_message(_: std::io::Error) -> String {
    "WASM helper communication failed".to_owned()
}
