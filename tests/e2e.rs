use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};

use futures_util::{SinkExt, StreamExt};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use tokio::{net::UnixStream, process::Command};
use tokio_tungstenite::{connect_async, tungstenite::Message};

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

async fn call_generic_ok(client: &Client, name: &str, arguments: Value) -> CallToolResult {
    let result = client
        .call_tool(
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(arguments.as_object().cloned().unwrap_or_else(Map::new)),
        )
        .await
        .expect("generic tool call transport");
    assert!(
        !result.is_error.unwrap_or(false),
        "generic {name} returned a tool error: {}",
        text(&result)
    );
    result
}

fn bridge_command(
    socket: &Path,
    profile: &Path,
    port: u16,
    routing: &str,
    layout: &str,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-browser-mcp"));
    command
        .arg("--socket")
        .arg(socket)
        .arg("--profile")
        .arg(profile)
        .arg("--cdp-port")
        .arg(port.to_string())
        .arg("--disable-crash-reporter")
        .arg("--routing")
        .arg(routing)
        .arg("--session-layout")
        .arg(layout)
        .stderr(Stdio::inherit());
    command
}

async fn client(socket: &Path, profile: &Path, port: u16, routing: &str, layout: &str) -> Client {
    let command = bridge_command(socket, profile, port, routing, layout);
    let transport = TokioChildProcess::new(command).expect("start stdio bridge");
    ().serve(transport).await.expect("initialize bridge")
}

async fn daemon_pid(socket: &Path) -> libc::pid_t {
    UnixStream::connect(socket)
        .await
        .expect("connect to daemon")
        .peer_cred()
        .expect("read daemon credentials")
        .pid()
        .expect("daemon PID")
}

async fn release_session(socket: &Path, session_id: &str) -> bool {
    let status = Command::new(env!("CARGO_BIN_EXE_opencode-browser-mcp"))
        .arg("--socket")
        .arg(socket)
        .arg("--release-session")
        .arg(session_id)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("run session release control command");
    status.success()
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

async fn wait_for_tab_absent(port: u16, fragment: &str) {
    for _ in 0..100 {
        if !tabs_contain_url(&browser_tabs(port).await, fragment) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("browser tab containing {fragment:?} remained open");
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

async fn wait_for_exit(pid: libc::pid_t) {
    for _ in 0..800 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon {pid} did not exit");
}

fn sibling_path(socket: &Path, suffix: &str) -> std::path::PathBuf {
    let mut path = socket.as_os_str().to_owned();
    path.push(suffix);
    path.into()
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
    let socket = root.join("daemon.sock");
    let profile = root.join("profile");
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
    let mut command = bridge_command(&socket, &profile, port, "opencode", "shared-window");
    command
        .arg("--browser-executable")
        .arg("/bin/true")
        .arg("--downstream-command")
        .arg(&downstream)
        .stderr(Stdio::null());
    let transport = TokioChildProcess::new(command).expect("start compact startup bridge");
    let client = ().serve(transport).await.expect("initialize compact bridge");
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
    let pid = daemon_pid(&socket).await;
    client.cancel().await.expect("stop compact startup bridge");
    wait_for_exit(pid).await;
    let _ = std::fs::remove_dir_all(root);
}

async fn exercise_layout(layout: &str, separate_windows: bool) {
    let socket = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .expect("XDG_RUNTIME_DIR")
        .join(format!(
            "opencode-browser-mcp-e2e-{}-{layout}.sock",
            std::process::id()
        ));
    let profile = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-e2e-profile-{}-{layout}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir(&profile).expect("create private test profile");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))
        .expect("restrict test profile");
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve test CDP port")
        .local_addr()
        .expect("test CDP address")
        .port();
    let _ = std::fs::remove_file(&socket);
    let state = sibling_path(&socket, ".state.json");
    let lock = sibling_path(&socket, ".lock");
    let log = sibling_path(&socket, ".stderr.log");
    let _ = std::fs::remove_file(&state);
    let _ = std::fs::remove_file(&lock);
    let _ = std::fs::remove_file(&log);
    let (client_a, client_b) = tokio::join!(
        client(&socket, &profile, port, "opencode-direct", layout),
        client(&socket, &profile, port, "opencode-direct", layout)
    );

    let direct_tools = client_a.list_all_tools().await.expect("list direct tools");
    let compact = client(&socket, &profile, port, "opencode", layout).await;
    let compact_tools = compact.list_all_tools().await.expect("list compact tools");
    assert_eq!(
        compact_tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["dispatch", "help"]
    );
    let direct_schema_bytes = serde_json::to_vec(&direct_tools).unwrap().len();
    let compact_schema_bytes = serde_json::to_vec(&compact_tools).unwrap().len();
    eprintln!(
        "browser tool definitions: {direct_schema_bytes} direct bytes, {compact_schema_bytes} compact bytes"
    );
    assert!(compact_schema_bytes * 4 < direct_schema_bytes);
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "tool discovery started the browser"
    );
    let invalid = compact
        .call_tool(request(
            "dispatch",
            "compact-smoke",
            json!({"action": "not_a_browser_action"}),
        ))
        .await
        .expect("invalid compact call transport");
    assert!(invalid.is_error.unwrap_or(false));
    let help = call_ok(&compact, "help", "compact-smoke", json!({})).await;
    let mut documented = text(&help)
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Available browser actions: ")
        .unwrap()
        .split(", ")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut direct_names = direct_tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    documented.sort_unstable();
    direct_names.sort_unstable();
    assert_eq!(documented, direct_names);
    assert!(release_session(&socket, "unused-session").await);
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "invalid calls, browser help, or unused-session release started the browser"
    );
    call_ok(
        &compact,
        "dispatch",
        "compact-smoke",
        json!({
            "action": "navigate_page",
            "arguments": {"type": "url", "url": "https://example.net/#compact-client"}
        }),
    )
    .await;
    create_unowned_tab(port).await;
    assert!(tabs_contain_url(
        &browser_tabs(port).await,
        "compact-client"
    ));
    assert!(release_session(&socket, "compact-smoke").await);
    compact.cancel().await.expect("stop compact bridge");

    let generic = client(&socket, &profile, port, "connection", layout).await;
    call_generic_ok(
        &generic,
        "navigate_page",
        json!({"type": "url", "url": "https://example.edu/#generic-client"}),
    )
    .await;
    assert!(tabs_contain_url(
        &browser_tabs(port).await,
        "generic-client"
    ));
    generic.cancel().await.expect("stop generic bridge");
    wait_for_tab_absent(port, "generic-client").await;
    let remaining = browser_tabs(port).await;
    assert!(tabs_contain_url(&remaining, "user-tab"));

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

    let window_a = window_for_url(port, "example.com").await;
    let window_b = window_for_url(port, "example.org").await;
    if separate_windows {
        assert_ne!(
            window_a, window_b,
            "different browser sessions shared one top-level window"
        );
    } else {
        assert_eq!(
            window_a, window_b,
            "shared-window sessions used different top-level windows"
        );
    }
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
        "navigate_page",
        "smoke-a",
        json!({"type": "url", "url": "data:text/html,<title>Busy%20A</title>"}),
    )
    .await;
    let (_, release_queued_while_busy) = tokio::join!(
        call_ok(
            &client_a,
            "evaluate_script",
            "smoke-a",
            json!({"function": "async () => { await new Promise(resolve => setTimeout(resolve, 2000)); return location.href; }"}),
        ),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            release_session(&socket, "smoke-a").await
        },
    );
    assert!(
        release_queued_while_busy,
        "did not queue active session release"
    );
    wait_for_tab_absent(port, "Busy").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

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

    assert!(release_session(&socket, "smoke-a").await);
    call_ok(&client_b, "list_pages", "smoke-b", json!({})).await;
    let tabs = browser_tabs(port).await;
    assert!(tabs_contain_url(&tabs, "user-tab"));
    assert!(tabs_contain_url(&tabs, "Session%20B"));
    assert!(!tabs_contain_url(&tabs, "Session%20A"));
    assert!(!tabs_contain_url(&tabs, "extra-a"));

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

    let persisted: Value = serde_json::from_slice(
        &std::fs::read(&state).expect("read pre-shutdown persisted ownership state"),
    )
    .expect("parse pre-shutdown persisted ownership state");
    let owners = persisted["owners"].as_object().expect("persisted owners");
    assert_eq!(owners.len(), 1);
    assert!(owners.values().all(|owner| owner == "opencode:smoke-b"));
    let sessions = persisted["sessions"]
        .as_object()
        .expect("persisted sessions");
    assert_eq!(sessions.len(), 1);
    assert!(sessions.contains_key("opencode:smoke-b"));

    let original_daemon_pid = daemon_pid(&socket).await;
    client_a.cancel().await.expect("stop bridge A");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    call_ok(&client_b, "list_pages", "smoke-b", json!({})).await;
    client_b.cancel().await.expect("stop bridge B");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let client_c = client(&socket, &profile, port, "opencode-direct", layout).await;
    assert_eq!(daemon_pid(&socket).await, original_daemon_pid);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    call_ok(&client_c, "list_pages", "smoke-b", json!({})).await;
    client_c.cancel().await.expect("stop reconnecting bridge");
    wait_for_exit(original_daemon_pid).await;
    assert!(!socket.exists());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "browser remained reachable after the last MCP stdio bridge disconnected"
    );
    let persisted: Value = serde_json::from_slice(
        &std::fs::read(&state).expect("read final persisted ownership state"),
    )
    .expect("parse final persisted ownership state");
    assert_eq!(persisted["owners"], json!({}));
    assert_eq!(persisted["sessions"], json!({}));
    let _ = std::fs::remove_file(state);
    let _ = std::fs::remove_file(lock);
    let _ = std::fs::remove_file(log);
    let _ = std::fs::remove_dir_all(profile);
}

#[tokio::test]
#[ignore = "starts isolated headed browsers"]
async fn isolates_sessions_in_both_window_layouts() {
    exercise_layout("shared-window", false).await;
    exercise_layout("window-per-session", true).await;
}
