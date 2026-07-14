//! Private one-shot WASM runtime. It is never a public application entrypoint.

use std::process::ExitCode;

use rw_ext::{
    HookDirective, MAX_WASM_HOST_HEADER_BYTES, MAX_WASM_HOST_RESPONSE_BYTES, WasmHookHost,
    WasmHostRequest, WasmHostResponse,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ABSOLUTE_MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = write_response(&WasmHostResponse::Error { message }).await;
            ExitCode::SUCCESS
        }
    }
}

async fn run() -> Result<(), String> {
    let mut stdin = tokio::io::stdin();
    let header_len = stdin.read_u32().await.map_err(io_message)? as usize;
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
        WasmHostRequest::Validate { manifest, limits } => {
            WasmHookHost::from_bytes(manifest, &component, limits).map_or_else(
                |error| WasmHostResponse::Error {
                    message: error.to_string(),
                },
                |_| WasmHostResponse::Valid,
            )
        }
        WasmHostRequest::Invoke {
            manifest,
            limits,
            event,
            input,
        } => match WasmHookHost::from_bytes(manifest, &component, limits) {
            Ok(host) => match host.invoke_json(&event, &input).await {
                Ok(HookDirective::Continue) => WasmHostResponse::Continue,
                Ok(HookDirective::Replace(payload)) => WasmHostResponse::Replace { payload },
                Ok(HookDirective::Block { message }) => WasmHostResponse::Block { message },
                Err(error) => WasmHostResponse::Error {
                    message: error.to_string(),
                },
            },
            Err(error) => WasmHostResponse::Error {
                message: error.to_string(),
            },
        },
    };
    write_response(&response).await
}

async fn write_response(response: &WasmHostResponse) -> Result<(), String> {
    let bytes = serde_json::to_vec(response)
        .map_err(|_| "WASM helper response could not encode".to_owned())?;
    if bytes.len() > MAX_WASM_HOST_RESPONSE_BYTES {
        return Err("WASM helper response exceeds its wire limit".to_owned());
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| "WASM helper response exceeds its wire limit".to_owned())?;
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(&len.to_be_bytes())
        .await
        .map_err(io_message)?;
    stdout.write_all(&bytes).await.map_err(io_message)?;
    stdout.shutdown().await.map_err(io_message)
}

fn io_message(_: std::io::Error) -> String {
    "WASM helper communication failed".to_owned()
}
