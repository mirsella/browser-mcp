use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use tokio::{net::UnixStream, process::Command};

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

async fn client(socket: &Path, profile: &Path, port: u16) -> Client {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-browser-mcp"));
    command
        .arg("--socket")
        .arg(socket)
        .arg("--profile")
        .arg(profile)
        .arg("--cdp-port")
        .arg(port.to_string())
        .arg("--disable-crash-reporter")
        .stderr(Stdio::inherit());
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
#[ignore = "starts an isolated headed Helium browser"]
async fn isolates_two_sessions_and_hides_routing_fields() {
    let socket = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .expect("XDG_RUNTIME_DIR")
        .join(format!(
            "opencode-browser-mcp-e2e-{}.sock",
            std::process::id()
        ));
    let profile = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-e2e-profile-{}",
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
        client(&socket, &profile, port),
        client(&socket, &profile, port)
    );

    for tool in client_a.list_all_tools().await.expect("list tools") {
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

    let started = tokio::time::Instant::now();
    let (delayed_a, delayed_b) = tokio::join!(
        call_ok(
            &client_a,
            "navigate_page",
            "smoke-a",
            json!({"type": "url", "url": "data:text/html,<title>Session%20A</title><script>const%20end=Date.now()%2B2000;while(Date.now()%3Cend)%7B%7D</script>"}),
        ),
        call_ok(
            &client_b,
            "navigate_page",
            "smoke-b",
            json!({"type": "url", "url": "data:text/html,<title>Session%20B</title><script>const%20end=Date.now()%2B2000;while(Date.now()%3Cend)%7B%7D</script>"}),
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

    let persisted = std::fs::read_to_string(&state).expect("persisted ownership state");
    assert!(persisted.contains("smoke-a"));
    assert!(persisted.contains("smoke-b"));

    let persisted: Value = serde_json::from_slice(
        &std::fs::read(&state).expect("read pre-shutdown persisted ownership state"),
    )
    .expect("parse pre-shutdown persisted ownership state");
    assert_ne!(persisted["owners"], json!({}));
    assert_ne!(persisted["sessions"], json!({}));

    let original_daemon_pid = daemon_pid(&socket).await;
    client_a.cancel().await.expect("stop bridge A");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    call_ok(&client_b, "list_pages", "smoke-b", json!({})).await;
    client_b.cancel().await.expect("stop bridge B");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let client_c = client(&socket, &profile, port).await;
    assert_eq!(daemon_pid(&socket).await, original_daemon_pid);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    call_ok(&client_c, "list_pages", "smoke-a", json!({})).await;
    client_c.cancel().await.expect("stop reconnecting bridge");
    wait_for_exit(original_daemon_pid).await;
    assert!(!socket.exists());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "Helium remained reachable after the last OpenCode client disconnected"
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
