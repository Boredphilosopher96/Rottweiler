#![allow(clippy::expect_used)]
use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn permit() -> (Arc<Semaphore>, tokio::sync::OwnedSemaphorePermit) {
    let pool = Arc::new(Semaphore::new(1));
    let permit = Arc::clone(&pool).try_acquire_owned().expect("permit");
    (pool, permit)
}

#[tokio::test]
async fn blocked_pipe_read_and_write_wake_restore_flags_and_join_before_refund() {
    let (input, _keep_input_open) = rustix::pipe::pipe().expect("input pipe");
    let (_keep_output_open, output) = rustix::pipe::pipe().expect("output pipe");
    let inspect = rustix::io::dup(&output).expect("inspection descriptor");
    let flags = fcntl_getfl(&output).expect("original flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("fill nonblocking");
    loop {
        match rustix::io::write(&output, &[b'x'; 4096]) {
            Ok(_) => {}
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => panic!("fill: {error}"),
        }
    }
    fcntl_setfl(&output, flags).expect("restore fixture flags");
    // Darwin records a sticky kernel flag after the first write. Snapshot the
    // state handed to the native owner, after the fixture has filled the pipe.
    let flags = fcntl_getfl(&output).expect("flags before native ownership");
    let (pool, permit) = permit();
    let (stream, owner) = open(input, output, permit).expect("open");
    let (mut read, mut write) = tokio::io::split(stream);
    let reader = tokio::spawn(async move {
        let mut bytes = [0; 4];
        read.read_exact(&mut bytes).await
    });
    let writer = tokio::spawn(async move { write.write_all(b"response").await });
    tokio::task::yield_now().await;
    assert_eq!(pool.available_permits(), 0);
    assert!(!reader.is_finished());
    assert!(!writer.is_finished());
    tokio::time::timeout(std::time::Duration::from_secs(2), owner.close())
        .await
        .expect("physical deadline")
        .expect("physical close");
    assert_eq!(pool.available_permits(), 1);
    assert_eq!(fcntl_getfl(&inspect).expect("restored flags"), flags);
    assert!(reader.await.expect("reader join").is_err());
    assert!(writer.await.expect("writer join").is_err());
}

#[tokio::test]
async fn regular_file_stdio_preserves_bytes_and_physical_write_acknowledgement() {
    let directory = tempfile::tempdir().expect("directory");
    let input_path = directory.path().join("input");
    let output_path = directory.path().join("output");
    std::fs::write(&input_path, b"input\n").expect("input");
    let input: OwnedFd = std::fs::File::open(input_path).expect("file").into();
    let output: OwnedFd = std::fs::File::create(&output_path).expect("output").into();
    let (pool, permit) = permit();
    let (mut stream, owner) = open(input, output, permit).expect("native files");
    let mut input = [0; 6];
    stream.read_exact(&mut input).await.expect("read");
    assert_eq!(&input, b"input\n");
    stream.write_all(b"result\n").await.expect("physical write");
    assert_eq!(
        std::fs::read(output_path).expect("visible output"),
        b"result\n"
    );
    drop(stream);
    owner.close().await.expect("join");
    assert_eq!(pool.available_permits(), 1);
}

#[test]
fn dropping_unpolled_close_without_a_runtime_joins_the_native_owner() {
    let (input, _keep_input_open) = rustix::pipe::pipe().expect("input");
    let (_keep_output_open, output) = rustix::pipe::pipe().expect("output");
    let inspect = rustix::io::dup(&input).expect("inspect");
    let flags = fcntl_getfl(&inspect).expect("flags");
    let (pool, permit) = permit();
    let (stream, owner) = open(input, output, permit).expect("open");
    let close = owner.close();
    drop(close);
    assert_eq!(pool.available_permits(), 1);
    assert_eq!(fcntl_getfl(&inspect).expect("flags"), flags);
    drop(stream);
}

#[tokio::test]
async fn real_rmcp_client_crosses_native_stdio_for_handshake_tool_reply_and_close() {
    use rmcp::{ServiceExt as _, model::CallToolRequestParams};
    let (server, bridge) =
        super::super::ownership_tests::server(std::time::Duration::from_secs(30));
    bridge.release.add_permits(1);
    let (client, native) = UnixStream::pair().expect("native transport");
    client.set_nonblocking(true).expect("async client");
    let client = tokio::net::UnixStream::from_std(client).expect("client");
    let (pool, permit) = permit();
    let (stream, owner) = open(
        rustix::io::dup(&native).expect("input"),
        native.into(),
        permit,
    )
    .expect("stdio adapter");
    let server = tokio::spawn(async move {
        let result = dispatch::run(server, stream, CancellationToken::default()).await;
        result.and(owner.close().await)
    });
    let mut client = ().serve(client).await.expect("initialize");
    assert_eq!(client.peer().list_all_tools().await.expect("list").len(), 4);
    let request = CallToolRequestParams::new("rottweiler_tools_call").with_arguments(
        serde_json::json!({"name":"read","arguments":{"answer":42}})
            .as_object()
            .expect("object")
            .clone(),
    );
    let response = client.peer().call_tool(request).await.expect("tool call");
    assert_eq!(
        response.structured_content.expect("structured")["answer"],
        42
    );
    client.close().await.expect("close client");
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .expect("close deadline")
        .expect("server join")
        .expect("physical close");
    assert_eq!(pool.available_permits(), 1);
}
