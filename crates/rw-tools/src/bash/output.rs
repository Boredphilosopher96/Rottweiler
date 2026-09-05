use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_types::ToolOutputStream;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{Duration, Instant, sleep, sleep_until};

use crate::registry::{ToolError, ToolOutputChunk, ToolOutputSink};

pub(super) async fn copy_stream(
    mut reader: impl AsyncRead + Unpin,
    stream: ToolOutputStream,
    output: Arc<dyn ToolOutputSink>,
) -> Result<(), ToolError> {
    const FLUSH_INTERVAL: Duration = Duration::from_millis(16);
    const MAX_BATCH_BYTES: usize = 64 * 1024;

    let mut buffer = [0_u8; 8 * 1024];
    let mut pending = Vec::with_capacity(buffer.len() + 4);
    let mut content = String::with_capacity(MAX_BATCH_BYTES);
    let mut deadline = None;
    loop {
        if content.len() >= MAX_BATCH_BYTES {
            emit_output_batch(&output, &stream, &mut content).await?;
            deadline = None;
        }
        let read = if let Some(flush_at) = deadline {
            tokio::select! {
                biased;
                () = sleep_until(flush_at) => None,
                read = reader.read(&mut buffer) => Some(read),
            }
        } else {
            Some(reader.read(&mut buffer).await)
        };
        let Some(read) = read else {
            emit_output_batch(&output, &stream, &mut content).await?;
            deadline = None;
            continue;
        };
        let read = read.map_err(|error| ToolError::Output(error.to_string()))?;
        if read == 0 {
            if !pending.is_empty() {
                content.push_str(String::from_utf8_lossy(&pending).as_ref());
                pending.clear();
            }
            emit_output_batch(&output, &stream, &mut content).await?;
            return Ok(());
        }
        pending.extend_from_slice(&buffer[..read]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(complete) => {
                    content.push_str(complete);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    content.push_str(String::from_utf8_lossy(&pending[..valid]).as_ref());
                    let Some(invalid) = error.error_len() else {
                        pending.drain(..valid);
                        break;
                    };
                    content.push('\u{fffd}');
                    pending.drain(..valid + invalid);
                }
            }
        }
        if deadline.is_none() && !content.is_empty() {
            deadline = Some(Instant::now() + FLUSH_INTERVAL);
        }
    }
}

pub(super) async fn emit_output_batch(
    output: &Arc<dyn ToolOutputSink>,
    stream: &ToolOutputStream,
    content: &mut String,
) -> Result<(), ToolError> {
    if content.is_empty() {
        return Ok(());
    }
    output
        .emit(ToolOutputChunk {
            stream: stream.clone(),
            content: std::mem::take(content),
        })
        .await
}

pub(super) async fn finish_command_output(
    stdout: tokio::task::JoinHandle<Result<(), ToolError>>,
    stderr: tokio::task::JoinHandle<Result<(), ToolError>>,
) -> Result<(), ToolError> {
    let (stdout, stderr) = tokio::join!(finish_output_task(stdout), finish_output_task(stderr));
    stdout.and(stderr)
}

pub(super) async fn finish_output_task(
    mut task: tokio::task::JoinHandle<Result<(), ToolError>>,
) -> Result<(), ToolError> {
    tokio::select! {
        result = &mut task => result.map_err(|error| ToolError::Output(error.to_string()))?,
        () = sleep(Duration::from_secs(2)) => {
            task.abort();
            match task.await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Ok(()),
                Err(error) => Err(ToolError::Output(error.to_string())),
            }
        }
    }
}

pub(super) struct CapturingSink {
    upstream: Arc<dyn ToolOutputSink>,
    state: Mutex<CapturedState>,
}

pub(super) struct CapturedState {
    stdout: TailBuffer,
    stderr: TailBuffer,
    limit: usize,
}

impl CapturingSink {
    pub(super) fn new(upstream: Arc<dyn ToolOutputSink>, limit: usize) -> Self {
        Self {
            upstream,
            state: Mutex::new(CapturedState {
                stdout: TailBuffer::new(limit),
                stderr: TailBuffer::new(limit),
                limit,
            }),
        }
    }

    pub(super) fn finish(&self) -> Result<CapturedOutput, ToolError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ToolError::Output("capture lock was poisoned".to_owned()))?;
        let stdout_seen = state.stdout.total_seen;
        let stderr_seen = state.stderr.total_seen;
        let (stdout_limit, stderr_limit) =
            allocate_stream_limits(state.limit, stdout_seen, stderr_seen);
        let (stdout, stdout_truncated) = state.stdout.render(stdout_limit);
        let (stderr, stderr_truncated) = state.stderr.render(stderr_limit);
        Ok(CapturedOutput {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

#[async_trait]
impl ToolOutputSink for CapturingSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ToolError::Output("capture lock was poisoned".to_owned()))?;
            match chunk.stream {
                ToolOutputStream::Stdout => state.stdout.push(chunk.content.as_bytes()),
                ToolOutputStream::Stderr => state.stderr.push(chunk.content.as_bytes()),
            }
        }
        self.upstream.emit(chunk).await
    }
}

pub(super) struct CapturedOutput {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) stdout_truncated: bool,
    pub(super) stderr_truncated: bool,
}

pub(super) struct TailBuffer {
    bytes: Vec<u8>,
    cap: usize,
    total_seen: usize,
}

impl TailBuffer {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
            total_seen: 0,
        }
    }

    fn push(&mut self, incoming: &[u8]) {
        self.total_seen = self.total_seen.saturating_add(incoming.len());
        if self.cap == 0 {
            return;
        }
        if incoming.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&incoming[incoming.len() - self.cap..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(incoming.len())
            .saturating_sub(self.cap);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend_from_slice(incoming);
    }

    fn render(&self, limit: usize) -> (String, bool) {
        if self.total_seen <= limit {
            return (String::from_utf8_lossy(&self.bytes).into_owned(), false);
        }
        if limit == 0 {
            return (String::new(), true);
        }
        let provisional_dropped = self.total_seen.saturating_sub(limit);
        let provisional_marker = format!("[truncated {provisional_dropped} bytes; showing tail]\n");
        if provisional_marker.len() >= limit {
            let start = self.bytes.len().saturating_sub(limit);
            return (
                String::from_utf8_lossy(&self.bytes[start..]).into_owned(),
                true,
            );
        }
        let tail_limit = limit - provisional_marker.len();
        let retained = self.bytes.len().min(tail_limit);
        let dropped = self.total_seen.saturating_sub(retained);
        let marker = format!("[truncated {dropped} bytes; showing tail]\n");
        if marker.len() >= limit {
            let start = self.bytes.len().saturating_sub(limit);
            return (
                String::from_utf8_lossy(&self.bytes[start..]).into_owned(),
                true,
            );
        }
        let adjusted_tail_limit = limit.saturating_sub(marker.len());
        let start = self.bytes.len().saturating_sub(adjusted_tail_limit);
        (
            format!("{marker}{}", String::from_utf8_lossy(&self.bytes[start..])),
            true,
        )
    }
}

pub(super) fn allocate_stream_limits(total: usize, stdout: usize, stderr: usize) -> (usize, usize) {
    if stdout == 0 {
        return (0, total);
    }
    if stderr == 0 {
        return (total, 0);
    }
    let combined = stdout.saturating_add(stderr).max(1);
    let stdout_limit = total.saturating_mul(stdout) / combined;
    (stdout_limit, total.saturating_sub(stdout_limit))
}
