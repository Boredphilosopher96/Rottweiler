#![allow(clippy::expect_used)]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::tempdir;

const VALID_CATALOG: &str = r#"{
  "fixture": {
    "id": "fixture",
    "name": "Fixture Provider",
    "models": {
      "fast": {
        "id": "fast",
        "name": "Fixture Fast",
        "reasoning": true,
        "tool_call": true,
        "limit": {"context": 12345, "output": 678},
        "cost": {
          "input": 0.25,
          "output": 2,
          "cache_read": 0.025,
          "cache_write": 0.3,
          "reasoning": 2.5
        }
      }
    }
  }
}"#;

#[test]
fn refresh_routes_through_global_proxy_and_installs_converted_table() {
    let root = tempdir().expect("temporary directory should be created");
    let destination = root.path().join("user/models.toml");
    let server = spawn_http_server(VALID_CATALOG);
    let source = "http://127.0.0.1:1/api.json";
    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .env_clear()
        .current_dir(root.path())
        .env("ROTTWEILER_HOME", root.path().join("user"))
        .env("RW_NETWORK_PROXY", format!("http://{}", server.address))
        .env("NO_PROXY", "")
        .args([
            "models",
            "refresh",
            "--source",
            source,
            "--output",
            destination.to_str().expect("destination should be UTF-8"),
        ])
        .output()
        .expect("rw models refresh should run");

    assert!(
        output.status.success(),
        "refresh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server
        .request
        .recv_timeout(Duration::from_secs(5))
        .expect("global proxy should receive refresh request");
    assert!(request.starts_with(&format!("GET {source} HTTP/1.1")));

    let contents = fs::read_to_string(&destination).expect("pricing table should be installed");
    let table: toml::Value =
        toml::from_str(&contents).expect("installed table should be valid TOML");
    assert_eq!(table["source_url"].as_str(), Some(source));
    assert_eq!(table["snapshot_date"].as_str(), Some("2026-07-10"));
    assert_eq!(table["revision"].as_str(), Some("\"fixture-revision\""));
    let model = &table["models"]["fixture/fast"];
    assert_eq!(
        model["input_per_million_micros_usd"].as_integer(),
        Some(250_000)
    );
    assert_eq!(
        model["output_per_million_micros_usd"].as_integer(),
        Some(2_000_000)
    );
    assert_eq!(model["max_context_tokens"].as_integer(), Some(12_345));
    assert_eq!(model["supports_tools"].as_bool(), Some(true));
    assert_eq!(model["supports_thinking"].as_bool(), Some(true));

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("refreshed 1 models"));
    assert!(stdout.contains(source));
    assert!(stdout.contains(destination.to_str().expect("destination should be UTF-8")));
}

#[test]
fn invalid_payload_leaves_existing_models_file_unchanged() {
    let root = tempdir().expect("temporary directory should be created");
    let user_root = root.path().join("user");
    fs::create_dir_all(&user_root).expect("user directory should be created");
    let destination = user_root.join("models.toml");
    fs::write(&destination, "existing-pricing-table\n").expect("sentinel should be written");
    let server = spawn_http_server(r#"{"fixture":{"models":{"bad":{"cost":{"input":1}}}}}"#);
    let source = format!("http://{}/api.json", server.address);

    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .env_clear()
        .current_dir(root.path())
        .env("ROTTWEILER_HOME", &user_root)
        .args([
            "models",
            "refresh",
            "--source",
            &source,
            "--output",
            destination.to_str().expect("destination should be UTF-8"),
        ])
        .output()
        .expect("rw models refresh should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing required cost.output"));
    assert_eq!(
        fs::read_to_string(destination).expect("sentinel should remain readable"),
        "existing-pricing-table\n"
    );
}

#[cfg(unix)]
#[test]
fn refresh_resolves_global_proxy_password_from_user_credential_store() {
    let root = tempdir().expect("temporary directory should be created");
    let user_root = root.path().join("user");
    fs::create_dir_all(&user_root).expect("user directory should be created");
    let server = spawn_http_server(VALID_CATALOG);
    fs::write(
        user_root.join("config.toml"),
        format!(
            "[network]\nproxy = \"http://{}\"\nproxy_username = \"catalog-user\"\nproxy_password_credential = \"catalog-proxy-password\"\n",
            server.address
        ),
    )
    .expect("proxy config should be written");
    let credentials = user_root.join("credentials.toml");
    fs::write(
        &credentials,
        "version = 1\n[credentials]\ncatalog-proxy-password = \"catalog-secret\"\n",
    )
    .expect("credential fallback should be written");
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600))
        .expect("credential fallback permissions should be private");
    let destination = user_root.join("models.toml");
    let source = "http://127.0.0.1:1/api.json";

    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .env_clear()
        .current_dir(root.path())
        .env("ROTTWEILER_HOME", &user_root)
        .args([
            "models",
            "refresh",
            "--source",
            source,
            "--output",
            destination.to_str().expect("destination should be UTF-8"),
        ])
        .output()
        .expect("rw models refresh should run");

    assert!(
        output.status.success(),
        "authenticated refresh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server
        .request
        .recv_timeout(Duration::from_secs(5))
        .expect("authenticated proxy should receive refresh request");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("proxy-authorization: basic y2f0ywxvzy11c2vyomnhdgfsb2ctc2vjcmv0")
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("catalog-secret"));
}

struct TestServer {
    address: std::net::SocketAddr,
    request: Receiver<String>,
}

fn spawn_http_server(body: &'static str) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture server should bind");
    listener
        .set_nonblocking(true)
        .expect("fixture listener should become nonblocking");
    let address = listener
        .local_addr()
        .expect("fixture address should resolve");
    let (sender, request) = mpsc::channel();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("fixture stream should become blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("fixture stream timeout should be set");
                    let mut received = Vec::new();
                    let mut chunk = [0_u8; 2048];
                    while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                        let read = stream.read(&mut chunk).expect("request should be readable");
                        if read == 0 {
                            break;
                        }
                        received.extend_from_slice(&chunk[..read]);
                    }
                    let request_text = String::from_utf8_lossy(&received).into_owned();
                    sender
                        .send(request_text)
                        .expect("request should be observed");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nDate: Fri, 10 Jul 2026 06:00:00 GMT\r\nETag: \"fixture-revision\"\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .expect("response should be writable");
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    });
    TestServer { address, request }
}
