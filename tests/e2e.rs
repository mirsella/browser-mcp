use std::{
    fs::OpenOptions,
    os::unix::ffi::OsStrExt,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use xxhash_rust::xxh3::xxh3_64;

const SESSION_KEY: &str = "__opencode_session_id";
type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

fn request(name: &str, session_id: &str, arguments: Value) -> CallToolRequestParams {
    let mut arguments = arguments.as_object().cloned().unwrap_or_else(Map::new);
    arguments.insert(SESSION_KEY.to_owned(), Value::from(session_id));
    CallToolRequestParams::new(name.to_owned()).with_arguments(arguments)
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call_ok(
    client: &Client,
    name: &str,
    session_id: &str,
    arguments: Value,
) -> CallToolResult {
    let result = client
        .call_tool(request(name, session_id, arguments))
        .await
        .expect("tool call transport");
    assert!(
        !result.is_error.unwrap_or(false),
        "{name} for {session_id} returned a tool error: {}",
        text(&result)
    );
    result
}

fn runtime_command(runtime: &Path, profile: &Path, port: u16, layout: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-browser-mcp"));
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("OPENCODE_BROWSER_PROFILE", profile)
        .env("OPENCODE_BROWSER_CDP_PORT", port.to_string())
        .env("OPENCODE_BROWSER_DISABLE_CRASH_REPORTER", "true")
        .env("OPENCODE_BROWSER_SESSION_LAYOUT", layout)
        .env("OZONE_PLATFORM", "x11")
        .env("WAYLAND_DISPLAY", "")
        .env("XDG_SESSION_TYPE", "x11")
        .stderr(Stdio::inherit());
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        command.env("OPENCODE_BROWSER_HEADLESS", "true");
    }
    command
}

fn adapter_command(
    runtime: &Path,
    direct_tools: bool,
    profile: &Path,
    port: u16,
    layout: &str,
) -> Command {
    let mut command = runtime_command(runtime, profile, port, layout);
    if direct_tools {
        command.arg("--direct-tools");
    }
    command
}

fn broker_socket(profile: &Path) -> std::path::PathBuf {
    let profile_id = xxh3_64(profile.as_os_str().as_bytes());
    let uid = unsafe { libc::geteuid() };
    let runtime = std::path::PathBuf::from(format!("/run/user/{uid}"));
    let directory = if runtime.exists() {
        runtime.join("opencode-browser-mcp")
    } else {
        std::path::PathBuf::from(format!("/tmp/opencode-browser-mcp-{uid}"))
    };
    directory.join(format!("broker-{profile_id:016x}.sock"))
}

async fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "broker socket did not appear");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_broker_exit(socket: &Path, profile: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let lock = profile.join("opencode-browser-mcp.lock");
    loop {
        let lease_available = match OpenOptions::new().read(true).open(&lock) {
            Ok(file) => file.try_lock().is_ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => panic!("failed to inspect broker lease: {error}"),
        };
        if !socket.exists() && lease_available {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "managed broker remained after every adapter disconnected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn release_session(
    runtime: &Path,
    profile: &Path,
    port: u16,
    layout: &str,
    session_id: &str,
) {
    let status = runtime_command(runtime, profile, port, layout)
        .arg("release-session")
        .arg(session_id)
        .status()
        .await
        .expect("request browser session release");
    assert!(status.success(), "broker rejected session release request");
}

async fn client(
    runtime: &Path,
    direct_tools: bool,
    profile: &Path,
    port: u16,
    layout: &str,
) -> Client {
    let transport = TokioChildProcess::new(adapter_command(
        runtime,
        direct_tools,
        profile,
        port,
        layout,
    ))
    .expect("start MCP adapter");
    ().serve(transport).await.expect("initialize MCP adapter")
}

async fn start_test_site() -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind cookie-sharing test site");
    let address = listener.local_addr().expect("test site address");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).await;
                let body = b"<!doctype html><title>Shared broker origin</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{address}"), task)
}

async fn browser_json(port: u16, path: &str) -> Value {
    let body = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build browser test client")
        .get(format!("http://127.0.0.1:{port}/json/{path}"))
        .send()
        .await
        .expect("request browser JSON")
        .error_for_status()
        .expect("browser JSON response status")
        .bytes()
        .await
        .expect("read browser JSON");
    serde_json::from_slice(&body).expect("parse browser JSON")
}

async fn browser_tabs(port: u16) -> Vec<Value> {
    browser_json(port, "list")
        .await
        .as_array()
        .expect("browser target list")
        .clone()
}

fn tabs_contain_url(tabs: &[Value], fragment: &str) -> bool {
    tabs.iter().any(|tab| {
        tab.get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.contains(fragment))
    })
}

async fn window_for_url(port: u16, url_fragment: &str) -> i64 {
    let tabs = browser_tabs(port).await;
    let target = tabs
        .iter()
        .find(|tab| {
            tab.get("url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.contains(url_fragment))
        })
        .and_then(|tab| tab.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no browser target URL contains {url_fragment}"));
    let version = browser_json(port, "version").await;
    let (mut socket, _) = connect_async(
        version["webSocketDebuggerUrl"]
            .as_str()
            .expect("browser websocket URL"),
    )
    .await
    .expect("connect browser websocket");
    socket
        .send(Message::Text(
            json!({"id": 1, "method": "Browser.getWindowForTarget", "params": {"targetId": target}})
                .to_string()
                .into(),
        ))
        .await
        .expect("request browser window");
    while let Some(message) = socket.next().await {
        let Message::Text(text) = message.expect("read browser window response") else {
            continue;
        };
        let response: Value = serde_json::from_str(&text).expect("parse browser window response");
        if response.get("id").and_then(Value::as_u64) == Some(1) {
            assert!(response.get("error").is_none(), "CDP failed: {response}");
            return response["result"]["windowId"]
                .as_i64()
                .expect("browser window ID");
        }
    }
    panic!("browser websocket closed before the window response");
}

async fn create_unowned_tab(port: u16) {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build browser test client")
        .put(format!(
            "http://127.0.0.1:{port}/json/new?https%3A%2F%2Fexample.edu%2F%23user-tab"
        ))
        .send()
        .await
        .expect("create independent browser tab")
        .error_for_status()
        .expect("independent browser tab response status");
}

async fn wait_for_browser_exit(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("browser remained reachable after the MCP process exited");
}

fn isolated_view(result: &CallToolResult, own_url: &str, other_url: &str) -> (bool, String) {
    let Some(pages) = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("pages"))
        .and_then(Value::as_array)
    else {
        return (false, text(result));
    };
    assert!(!pages.is_empty());
    for page in pages {
        let page = page.as_object().expect("structured page object");
        assert!(!page.contains_key("id"));
    }
    let serialized = Value::Array(pages.clone()).to_string();
    assert!(
        serialized.contains(own_url),
        "structured pages did not contain {own_url}: {serialized}"
    );
    assert!(
        !serialized.contains(other_url),
        "structured pages exposed {other_url}: {serialized}"
    );
    (true, text(result))
}

fn assert_no_page_ids(text: &str) {
    assert!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
    );
}

#[tokio::test]
async fn compact_startup_does_not_require_downstream() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-startup-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).expect("create private startup test directory");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("restrict startup test directory");
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let downstream = root.join("downstream");
    let downstream_started = root.join("downstream-started");
    std::fs::write(
        &downstream,
        format!("#!/bin/sh\ntouch {}\n", downstream_started.display()),
    )
    .expect("write downstream marker executable");
    std::fs::set_permissions(&downstream, std::fs::Permissions::from_mode(0o700))
        .expect("make downstream marker executable");
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve startup test CDP port")
        .local_addr()
        .expect("startup test CDP address")
        .port();
    let mut command = adapter_command(&root, false, &profile, port, "shared-window");
    command
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("OPENCODE_BROWSER_PROFILE")
        .env("TMPDIR", &root)
        .env("OPENCODE_BROWSER_EXECUTABLE", "/bin/true")
        .env("OPENCODE_BROWSER_HEADLESS", "true")
        .env("OPENCODE_BROWSER_DOWNSTREAM_COMMAND", &downstream)
        .arg("--profile")
        .arg(&profile)
        .stderr(Stdio::null())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let transport = TokioChildProcess::new(command).expect("start compact MCP adapter");
    let client = ().serve(transport).await.expect("initialize compact adapter");
    wait_for_socket(&socket).await;
    assert_eq!(
        client
            .list_all_tools()
            .await
            .expect("list compact startup tools")
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["dispatch", "help"]
    );
    assert!(
        !downstream_started.exists(),
        "compact startup ran downstream"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "compact startup opened the browser"
    );
    client.cancel().await.expect("stop compact MCP adapter");
    wait_for_broker_exit(&socket, &profile).await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn adapters_share_one_automatically_managed_broker() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-profile-lock-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).expect("create private profile-lock test directory");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("restrict profile-lock test directory");
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve profile-lock test port")
        .local_addr()
        .expect("profile-lock test address")
        .port();
    let (adapter_a, adapter_b) = tokio::join!(
        client(&root, false, &profile, port, "shared-window"),
        client(&root, false, &profile, port, "shared-window"),
    );
    wait_for_socket(&socket).await;

    let mismatch = tokio::time::timeout(
        Duration::from_secs(5),
        adapter_command(&root, false, &profile, port, "window-per-session")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("mismatched adapter did not exit")
    .expect("start mismatched adapter");
    assert!(!mismatch.status.success());
    assert!(
        String::from_utf8_lossy(&mismatch.stderr)
            .contains("browser configuration does not match the running managed broker"),
        "mismatched adapter failed without a clear diagnostic: {}",
        String::from_utf8_lossy(&mismatch.stderr)
    );

    adapter_a.cancel().await.expect("stop first MCP adapter");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        socket.exists(),
        "managed broker stopped while another adapter was connected"
    );
    adapter_b.cancel().await.expect("stop second MCP adapter");
    wait_for_broker_exit(&socket, &profile).await;

    let restarted = client(&root, false, &profile, port, "shared-window").await;
    wait_for_socket(&socket).await;
    restarted
        .cancel()
        .await
        .expect("stop restarted MCP adapter");
    wait_for_broker_exit(&socket, &profile).await;
    let _ = std::fs::remove_dir_all(root);
}

async fn exercise_shared_broker() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-e2e-{}-shared",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).expect("create private test directory");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("restrict test directory");
    let broker_launch = root.join("broker-launch");
    std::fs::create_dir(&broker_launch).expect("create broker launch directory");
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve test CDP port")
        .local_addr()
        .expect("test CDP address")
        .port();
    let (test_site, test_site_task) = start_test_site().await;
    let mut starter = adapter_command(&root, false, &profile, port, "shared-window");
    starter.current_dir(&broker_launch);
    let starter = TokioChildProcess::new(starter).expect("start broker-launching MCP adapter");
    let starter = ().serve(starter).await.expect("initialize MCP adapter");
    let client_a = client(&root, true, &profile, port, "shared-window").await;
    let client_b = client(&root, true, &profile, port, "shared-window").await;
    starter.cancel().await.expect("stop starter MCP adapter");
    std::fs::remove_dir(&broker_launch).expect("remove broker launch directory");

    let direct_tools = client_a
        .list_all_tools()
        .await
        .expect("list direct tools after removing the broker launch directory");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "tool discovery started the browser"
    );
    for tool in direct_tools {
        assert_ne!(tool.name, "select_page");
        assert_ne!(tool.name, "get_tab_id");
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(Value::as_object);
        assert!(properties.is_none_or(|properties| {
            !properties.contains_key("pageId") && !properties.contains_key(SESSION_KEY)
        }));
    }
    let (navigate_a, navigate_b) = tokio::join!(
        call_ok(
            &client_a,
            "navigate_page",
            "smoke-a",
            json!({"type": "url", "url": "https://example.com/"}),
        ),
        call_ok(
            &client_b,
            "navigate_page",
            "smoke-b",
            json!({"type": "url", "url": "https://example.org/"}),
        ),
    );
    let (structured_a, navigate_a) = isolated_view(&navigate_a, "example.com", "example.org");
    let (structured_b, navigate_b) = isolated_view(&navigate_b, "example.org", "example.com");
    assert!(structured_a || navigate_a.contains("example.com"));
    assert!(structured_b || navigate_b.contains("example.org"));
    assert!(!navigate_a.contains("example.org"));
    assert!(!navigate_b.contains("example.com"));
    assert_no_page_ids(&navigate_a);
    assert_no_page_ids(&navigate_b);
    create_unowned_tab(port).await;

    let window_a = window_for_url(port, "example.com").await;
    let window_b = window_for_url(port, "example.org").await;
    assert_eq!(
        window_a, window_b,
        "shared-window sessions used different top-level windows"
    );
    call_ok(
        &client_a,
        "new_page",
        "smoke-a",
        json!({"url": "https://example.net/#same-session-window"}),
    )
    .await;
    assert_eq!(
        window_for_url(port, "same-session-window").await,
        window_a,
        "a session's second tab was placed in another window"
    );

    call_ok(
        &client_a,
        "navigate_page",
        "smoke-a",
        json!({"type": "url", "url": format!("{test_site}/#cookie-writer")}),
    )
    .await;
    let stored_cookie = call_ok(
        &client_a,
        "evaluate_script",
        "smoke-a",
        json!({"function": "() => { document.cookie = 'shared_broker_cookie=visible; path=/'; return document.cookie; }"}),
    )
    .await;
    assert!(
        text(&stored_cookie).contains("shared_broker_cookie=visible"),
        "first adapter could not store the shared cookie: {}",
        text(&stored_cookie)
    );
    call_ok(
        &client_b,
        "navigate_page",
        "smoke-b",
        json!({"type": "url", "url": format!("{test_site}/#cookie-reader")}),
    )
    .await;
    let shared_cookie = call_ok(
        &client_b,
        "evaluate_script",
        "smoke-b",
        json!({"function": "() => document.cookie"}),
    )
    .await;
    assert!(
        text(&shared_cookie).contains("shared_broker_cookie=visible"),
        "independent adapters did not share browser profile cookies: {}",
        text(&shared_cookie)
    );

    tokio::join!(
        call_ok(
            &client_a,
            "navigate_page",
            "smoke-a",
            json!({"type": "url", "url": "data:text/html,<title>Session%20A</title>"}),
        ),
        call_ok(
            &client_b,
            "navigate_page",
            "smoke-b",
            json!({"type": "url", "url": "data:text/html,<title>Session%20B</title>"}),
        ),
    );
    let started = tokio::time::Instant::now();
    let (delayed_a, delayed_b) = tokio::join!(
        call_ok(
            &client_a,
            "evaluate_script",
            "smoke-a",
            json!({"function": "async () => { await new Promise(resolve => setTimeout(resolve, 2000)); return location.href; }"}),
        ),
        call_ok(
            &client_b,
            "evaluate_script",
            "smoke-b",
            json!({"function": "async () => { await new Promise(resolve => setTimeout(resolve, 2000)); return location.href; }"}),
        ),
    );
    assert!(
        started.elapsed() < Duration::from_millis(3500),
        "different sessions were globally serialized ({:?})",
        started.elapsed()
    );
    assert!(!text(&delayed_a).contains("Session B"));
    assert!(!text(&delayed_b).contains("Session A"));

    let (pages_a, pages_b) = tokio::join!(
        call_ok(&client_a, "list_pages", "smoke-a", json!({})),
        call_ok(&client_b, "list_pages", "smoke-b", json!({})),
    );
    let (structured_a, pages_a) = isolated_view(&pages_a, "Session%20A", "Session%20B");
    let (structured_b, pages_b) = isolated_view(&pages_b, "Session%20B", "Session%20A");
    assert_eq!(
        structured_a, structured_b,
        "sessions returned inconsistent structured page data"
    );
    assert!(structured_a || pages_a.contains("Session%20A"));
    assert!(!pages_a.contains("Session%20B"));
    assert!(structured_b || pages_b.contains("Session%20B"));
    assert!(!pages_b.contains("Session%20A"));
    assert_no_page_ids(&pages_a);
    assert_no_page_ids(&pages_b);

    call_ok(
        &client_a,
        "new_page",
        "smoke-a",
        json!({"url": "https://example.net/#extra-a"}),
    )
    .await;
    let extra_a = call_ok(&client_a, "list_pages", "smoke-a", json!({})).await;
    let extra_b = call_ok(&client_b, "list_pages", "smoke-b", json!({})).await;
    let (extra_a_structured, extra_a_text) = isolated_view(&extra_a, "extra-a", "Session%20B");
    let (extra_b_structured, extra_b_text) = isolated_view(&extra_b, "Session%20B", "extra-a");
    assert!(
        extra_a_structured || extra_a_text.contains("extra-a"),
        "second page was not created or attributed: {extra_a_text}"
    );
    assert!(!extra_b_text.contains("extra-a"));
    assert!(extra_b_structured || extra_b_text.contains("Session%20B"));
    assert_no_page_ids(&extra_a_text);
    assert_no_page_ids(&extra_b_text);

    let tabs = browser_tabs(port).await;
    assert!(tabs_contain_url(&tabs, "user-tab"));
    assert!(tabs_contain_url(&tabs, "Session%20A"));
    assert!(tabs_contain_url(&tabs, "Session%20B"));
    assert!(tabs_contain_url(&tabs, "extra-a"));

    let mut target_ids = tabs
        .iter()
        .filter_map(|tab| tab.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    target_ids.sort_unstable();
    call_ok(&client_a, "list_pages", "smoke-empty", json!({})).await;
    let tabs_after_list = browser_tabs(port).await;
    let mut target_ids_after_list = tabs_after_list
        .iter()
        .filter_map(|tab| tab.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    target_ids_after_list.sort_unstable();
    assert_eq!(
        target_ids_after_list, target_ids,
        "listing an empty session created a blank tab"
    );

    let release_started = Instant::now();
    let (_, release_elapsed) = tokio::join!(
        call_ok(
            &client_a,
            "evaluate_script",
            "smoke-a",
            json!({"function": "async () => { await new Promise(resolve => setTimeout(resolve, 6000)); return location.href; }"}),
        ),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            release_session(&root, &profile, port, "shared-window", "smoke-a").await;
            release_started.elapsed()
        },
    );
    assert!(
        release_elapsed >= Duration::from_millis(5500),
        "session release returned before the active browser call finished ({release_elapsed:?})"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tabs = browser_tabs(port).await;
        if !tabs_contain_url(&tabs, "Session%20A") && !tabs_contain_url(&tabs, "extra-a") {
            assert!(tabs_contain_url(&tabs, "Session%20B"));
            assert!(tabs_contain_url(&tabs, "user-tab"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "released session tabs remained open"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client_a.cancel().await.expect("stop first MCP adapter");
    client_b.cancel().await.expect("stop second MCP adapter");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok(),
        "browser stopped before the adapter reconnect grace elapsed"
    );

    let reconnected = client(&root, true, &profile, port, "shared-window").await;
    let pages_b = call_ok(&reconnected, "list_pages", "smoke-b", json!({})).await;
    let (structured_b, pages_b) = isolated_view(&pages_b, "Session%20B", "Session%20A");
    assert!(structured_b || pages_b.contains("Session%20B"));

    let compact = client(&root, false, &profile, port, "shared-window").await;
    let help = call_ok(
        &compact,
        "help",
        "compact-session",
        json!({"action": "navigate_page"}),
    )
    .await;
    assert!(text(&help).contains("navigate_page"));
    let compact_result = call_ok(
        &compact,
        "dispatch",
        "compact-session",
        json!({
            "action": "navigate_page",
            "arguments": {
                "type": "url",
                "url": "data:text/html,<title>Compact%20session</title>"
            }
        }),
    )
    .await;
    assert!(text(&compact_result).contains("Compact%20session"));
    release_session(&root, &profile, port, "shared-window", "compact-session").await;
    compact.cancel().await.expect("stop compact MCP adapter");
    reconnected
        .cancel()
        .await
        .expect("stop reconnected MCP adapter");

    wait_for_broker_exit(&socket, &profile).await;
    wait_for_browser_exit(port).await;
    test_site_task.abort();
    let _ = test_site_task.await;
    let _ = std::fs::remove_dir_all(root);
}

async fn exercise_window_per_session_layout() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-e2e-{}-window-layout",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).expect("create private layout test directory");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("restrict layout test directory");
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve layout test CDP port")
        .local_addr()
        .expect("layout test CDP address")
        .port();
    let client = client(&root, true, &profile, port, "window-per-session").await;
    call_ok(
        &client,
        "navigate_page",
        "layout-a",
        json!({
            "type": "url",
            "url": "data:text/html,<title>Layout%20A</title>"
        }),
    )
    .await;
    call_ok(
        &client,
        "navigate_page",
        "layout-b",
        json!({
            "type": "url",
            "url": "data:text/html,<title>Layout%20B</title>"
        }),
    )
    .await;
    let window_a = window_for_url(port, "Layout%20A").await;
    let window_b = window_for_url(port, "Layout%20B").await;
    assert_ne!(window_a, window_b, "different sessions shared a window");
    call_ok(
        &client,
        "new_page",
        "layout-a",
        json!({"url": "data:text/html,<title>Layout%20A2</title>"}),
    )
    .await;
    assert_eq!(
        window_for_url(port, "Layout%20A2").await,
        window_a,
        "a session's second tab was placed in another window"
    );

    client.cancel().await.expect("stop layout MCP adapter");
    wait_for_broker_exit(&socket, &profile).await;
    wait_for_browser_exit(port).await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "starts isolated headed browsers"]
async fn isolates_sessions_in_both_window_layouts() {
    exercise_shared_broker().await;
    exercise_window_per_session_layout().await;
}
