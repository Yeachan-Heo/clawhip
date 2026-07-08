use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use tempfile::TempDir;

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn spawn_discord_channel_mock() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Discord API");
    let addr = listener.local_addr().expect("mock addr");
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept lookup");
        let mut request = [0_u8; 2048];
        let read = stream.read(&mut request).expect("read lookup");
        tx.send(String::from_utf8_lossy(&request[..read]).into_owned())
            .expect("send request");
        let body = r#"{"name":"dev"}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    });

    (format!("http://{addr}"), rx)
}

#[test]
fn legacy_setup_bind_without_checkout_writes_route_only() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("clawhip.toml");
    fs::write(
        &config_path,
        r#"
[providers.discord]
bot_token = "test-token"
"#,
    )
    .expect("write config");
    let (api_base, request_rx) = spawn_discord_channel_mock();

    let output = Command::new(clawhip_bin())
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("CLAWHIP_DISCORD_API_BASE", api_base)
        .arg("--config")
        .arg(&config_path)
        .arg("setup")
        .arg("--bind")
        .arg("owner/repo=123")
        .output()
        .expect("run clawhip setup");

    assert!(
        output.status.success(),
        "setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let request = request_rx.recv().expect("mock saw lookup");
    assert!(request.starts_with("GET /channels/123 "));

    let saved = fs::read_to_string(&config_path).expect("read saved config");
    assert!(saved.contains("repo = \"owner/repo\""));
    assert!(saved.contains("channel = \"123\""));
    assert!(saved.contains("channel_name = \"dev\""));
    assert!(!saved.contains("[[monitors.git.repos]]"));
}
