#![allow(clippy::expect_used)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Command, Stdio},
    time::Duration,
};

use tempfile::tempdir;
use url::Url;

#[test]
fn auth_login_prints_pkce_url_and_rejects_wrong_callback_state_without_leaks() {
    const ATTACKER_STATE: &str = "attacker-state-cli-canary";
    const CODE: &str = "authorization-code-cli-canary";
    let root = tempdir().expect("temporary directory should be created");
    let user_root = root.path().join("user");
    fs::create_dir_all(&user_root).expect("user config directory should be created");
    fs::write(
        user_root.join("config.toml"),
        r#"
[providers.subscription]
kind = "openai_compatible"
oauth_authorization_endpoint = "https://login.example/authorize"
oauth_token_endpoint = "http://127.0.0.1:1/oauth/token"
oauth_client_id = "fixture-native-client"
oauth_scopes = ["models", "offline_access"]
"#,
    )
    .expect("OAuth config should be written");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rw"))
        .env_clear()
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("ROTTWEILER_HOME", &user_root)
        .args(["auth", "login", "subscription"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rw auth login should start");
    let stdout = child
        .stdout
        .take()
        .expect("child stdout should be captured");
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("authorization URL should be readable");
    let authorization_url = Url::parse(line.trim()).expect("authorization URL should parse");
    assert_eq!(query_value(&authorization_url, "response_type"), "code");
    assert_eq!(
        query_value(&authorization_url, "code_challenge_method"),
        "S256"
    );
    assert_eq!(
        query_value(&authorization_url, "scope"),
        "models offline_access"
    );
    let expected_state = query_value(&authorization_url, "state");
    let redirect = Url::parse(&query_value(&authorization_url, "redirect_uri"))
        .expect("redirect URI should parse");

    let address = format!(
        "{}:{}",
        redirect.host_str().expect("redirect host should exist"),
        redirect.port().expect("redirect port should exist")
    );
    let mut callback = redirect.clone();
    callback
        .query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", ATTACKER_STATE);
    let target = format!(
        "{}?{}",
        callback.path(),
        callback.query().expect("callback query should exist")
    );
    let mut stream = TcpStream::connect(&address).expect("callback should connect to CLI");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("callback read timeout should set");
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .expect("callback should write");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("callback response should read");
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
    drop(stdout);

    let output = child
        .wait_with_output()
        .expect("rw auth login should exit after bad state");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("state validation failed"));
    for canary in [ATTACKER_STATE, CODE, &expected_state] {
        assert!(!stderr.contains(canary));
    }
}

#[test]
fn copilot_login_rejects_api_key_mixing_before_credentials_or_network() {
    let root = tempdir().expect("temporary directory should be created");
    let user_root = root.path().join("user");
    fs::create_dir_all(&user_root).expect("user config directory should be created");
    fs::write(
        user_root.join("config.toml"),
        r#"
[providers.github-copilot]
kind = "github_copilot"
api_key_env = "COPILOT_TOKEN_MUST_NOT_BE_READ"
"#,
    )
    .expect("Copilot config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .env_clear()
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("ROTTWEILER_HOME", &user_root)
        .args(["auth", "login", "github-copilot"])
        .output()
        .expect("rw auth login should exit");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("fixed github_copilot profile"));
    assert!(!stderr.contains("COPILOT_TOKEN_MUST_NOT_BE_READ"));
    assert!(!user_root.join("credentials.toml").exists());
}

fn query_value(url: &Url, name: &str) -> String {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        .unwrap_or_else(|| panic!("query parameter {name} should exist"))
}
