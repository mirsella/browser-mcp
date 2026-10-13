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
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
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

async fn codex_request(action: &str, id: &str, arguments: Value) -> CallToolRequestParams {
    let input = json!({
        "hook_event_name": "PreToolUse", "session_id": id,
        "tool_name": "mcp__plugin_browser_browser__dispatch",
        "tool_input": {"action": action, "arguments": arguments},
    });
    let mut process = Command::new(env!("CARGO_BIN_EXE_browser-mcp"))
        .arg("codex-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&input).unwrap())
        .await
        .unwrap();
    let output = process.wait_with_output().await.unwrap();
    assert!(output.status.success(), "Codex routing hook failed");
    let output: Value = serde_json::from_slice(&output.stdout).unwrap();
    CallToolRequestParams::new("dispatch").with_arguments(
        output["hookSpecificOutput"]["updatedInput"]
            .as_object()
            .unwrap()
            .clone(),
    )
}

async fn codex_call(client: &Client, action: &str, id: &str, arguments: Value) -> CallToolResult {
    let result = client
        .call_tool(codex_request(action, id, arguments).await)
        .await
        .unwrap();
    assert!(
        !result.is_error.unwrap_or(false),
        "{action}: {}",
        text(&result)
    );
    result
}

async fn codex_end(runtime: &Path, profile: &Path, port: u16, id: &str) {
    let root = profile.parent().unwrap();
    let plugin_root = root.join("codex-plugin");
    let vendor = plugin_root.join("vendor/browser-mcp");
    std::fs::create_dir_all(vendor.parent().unwrap()).unwrap();
    if !vendor.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_browser-mcp"), &vendor).unwrap();
    }
    let hooks: Value = serde_json::from_str(include_str!("../hooks/hooks.json")).unwrap();
    let config = runtime_command(runtime, profile, port, "shared-window");
    let mut command = Command::new("sh");
    for (key, value) in config.as_std().get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    // Codex starts hooks in the conversation cwd; the MCP adapter uses the plugin cwd.
    let mut process = command
        .args([
            "-c",
            hooks["hooks"]["SessionEnd"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
        ])
        .current_dir(root)
        .env("PLUGIN_ROOT", &plugin_root)
        .env(
            "OPENCODE_BROWSER_PROFILE",
            Path::new("..").join(profile.file_name().unwrap()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!({"hook_event_name": "SessionEnd", "session_id": id}))
                .unwrap(),
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(3), process.wait_with_output())
        .await
        .expect("Codex SessionEnd exceeded its hook deadline")
        .unwrap();
    assert!(output.status.success(), "Codex session cleanup failed");
}

fn runtime_command(runtime: &Path, profile: &Path, port: u16, layout: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_browser-mcp"));
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
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &limit) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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

async fn broker_pid(socket: &Path) -> i32 {
    tokio::net::UnixStream::connect(socket)
        .await
        .expect("connect to broker for its PID")
        .peer_cred()
        .expect("broker credentials")
        .pid()
        .expect("broker PID")
}

fn kill_process(pid: i32, signal: i32) {
    assert_eq!(
        unsafe { libc::kill(pid, signal) },
        0,
        "failed to signal process {pid}"
    );
}

async fn wait_for_profile_lease(profile: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let file = OpenOptions::new()
            .read(true)
            .open(profile.join("opencode-browser-mcp.lock"))
            .unwrap();
        if file.try_lock().is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "crashed broker retained its profile lease"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn connected_adapters_recover_from_killed_and_segfaulted_brokers() {
    let root =
        std::env::temp_dir().join(format!("opencode-browser-mcp-crash-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (a, b) = tokio::join!(
        client(&root, false, &profile, port, "shared-window"),
        client(&root, false, &profile, port, "shared-window"),
    );
    for signal in [libc::SIGKILL, libc::SIGSEGV] {
        let old_pid = broker_pid(&socket).await;
        kill_process(old_pid, signal);
        if signal == libc::SIGSEGV {
            // Rust's stack-overflow handler first restores the default handler
            // for a synthetic SIGSEGV; unlike a real fault it does not recur.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let result = unsafe { libc::kill(old_pid, signal) };
            assert!(
                result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            );
        }
        wait_for_profile_lease(&profile).await;
        let (tools_a, tools_b) = tokio::join!(
            a.send_request(rmcp::model::ClientRequest::ListToolsRequest(
                Default::default()
            )),
            b.send_request(rmcp::model::ClientRequest::ListToolsRequest(
                Default::default()
            )),
        );
        for result in [tools_a, tools_b] {
            let rmcp::model::ServerResult::ListToolsResult(result) =
                result.expect("adapter recovered")
            else {
                panic!("unexpected broker response");
            };
            assert_eq!(result.tools.len(), 2);
        }
        assert_ne!(broker_pid(&socket).await, old_pid);
    }
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
    wait_for_broker_exit(&socket, &profile).await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_catalog_discovery_can_be_retried() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-catalog-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let downstream = root.join("downstream");
    std::fs::write(&downstream, r##"#!/usr/bin/env bun
import { existsSync, writeFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
if (!existsSync('failed-once')) { writeFileSync('failed-once', ''); process.exit(1); }
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  if (request.id === undefined) continue;
  let result = {};
  if (request.method === 'initialize') result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'test-worker', version: '1' } };
  if (request.method === 'tools/list') result = { tools: ['get_tab_id', 'list_pages'].map(name => ({ name, inputSchema: { type: 'object', properties: {} } })) };
  console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }));
}
"##).unwrap();
    std::fs::set_permissions(&downstream, std::fs::Permissions::from_mode(0o700)).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut command = adapter_command(&root, false, &profile, port, "shared-window");
    command
        .current_dir(&root)
        .env("OPENCODE_BROWSER_DOWNSTREAM_COMMAND", downstream);
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    let failed = client
        .call_tool(request("help", "catalog", json!({})))
        .await
        .unwrap();
    assert!(failed.is_error.unwrap_or(false));
    let recovered = call_ok(&client, "help", "catalog", json!({})).await;
    assert!(text(&recovered).contains("list_pages"));
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();
    wait_for_broker_exit(&socket, &profile).await;
    std::fs::remove_dir_all(root).unwrap();
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

type CdpSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn cdp_request(url: &str, method: &str, params: Value) -> Value {
    let (mut socket, _) = connect_async(url).await.unwrap();
    cdp_call(&mut socket, 1, method, params).await
}

async fn cdp_call(socket: &mut CdpSocket, id: u64, method: &str, params: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = socket.next().await {
            let Message::Text(text) = message.unwrap() else {
                continue;
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            if response["id"] == id {
                assert!(response.get("error").is_none(), "CDP failed: {response}");
                return response["result"].clone();
            }
        }
        panic!("CDP socket closed before responding to {method}");
    })
    .await
    .unwrap()
}

async fn browser_process_id(port: u16) -> i32 {
    let version = browser_json(port, "version").await;
    let processes = cdp_request(
        version["webSocketDebuggerUrl"].as_str().unwrap(),
        "SystemInfo.getProcessInfo",
        json!({}),
    )
    .await;
    processes["processInfo"]
        .as_array()
        .unwrap()
        .iter()
        .find(|process| process["type"] == "browser")
        .unwrap()["id"]
        .as_i64()
        .unwrap()
        .try_into()
        .unwrap()
}

async fn renderer_process_id(browser: &mut CdpSocket, page: &Value) -> i32 {
    let marker = format!("browser-mcp-renderer-{}", page["id"].as_str().unwrap());
    cdp_call(
        browser,
        2,
        "Tracing.start",
        json!({"categories": "blink.user_timing", "transferMode": "ReportEvents"}),
    )
    .await;
    // Emit a unique trace event through the exact page's CDP connection.
    cdp_request(
        page["webSocketDebuggerUrl"].as_str().unwrap(),
        "Runtime.evaluate",
        json!({"expression": format!("performance.mark({})", serde_json::to_string(&marker).unwrap())}),
    )
    .await;
    browser
        .send(Message::Text(
            json!({"id": 3, "method": "Tracing.end"}).to_string().into(),
        ))
        .await
        .unwrap();
    let mut renderer = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = browser.next().await {
            let Message::Text(text) = message.unwrap() else {
                continue;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert!(event.get("error").is_none(), "Tracing failed: {event}");
            if event["method"] == "Tracing.dataCollected" {
                for entry in event["params"]["value"].as_array().unwrap() {
                    if entry["name"] == marker {
                        renderer = entry["pid"].as_i64();
                    }
                }
            } else if event["method"] == "Tracing.tracingComplete" {
                assert_ne!(event["params"]["dataLossOccurred"], true);
                return;
            }
        }
        panic!("browser closed before completing its trace");
    })
    .await
    .expect("renderer identity trace");
    let pid: i32 = renderer
        .expect("Chrome trace omitted the exact page marker")
        .try_into()
        .unwrap();
    assert!(pid > 1);
    let processes = cdp_call(browser, 4, "SystemInfo.getProcessInfo", json!({})).await;
    assert!(
        processes["processInfo"]
            .as_array()
            .unwrap()
            .iter()
            .any(|process| {
                process["type"] == "renderer" && process["id"].as_i64() == Some(pid.into())
            }),
        "traced process is not a live renderer"
    );
    pid
}

fn kill_process_tree(pid: i32) {
    assert!(pid > 1);
    if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for child in children.split_whitespace() {
            kill_process_tree(child.parse().unwrap());
        }
    }
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
}

fn kill_recorded_workers(path: &Path) {
    let pids = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, "").unwrap();
    for pid in pids.lines() {
        kill_process_tree(pid.parse().unwrap());
    }
}

async fn wait_for_page_counter(page: &Value, counter: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = cdp_request(
            page["webSocketDebuggerUrl"].as_str().unwrap(),
            "Runtime.evaluate",
            json!({"expression": format!("window.{counter}")}),
        )
        .await;
        if value["result"]["value"] == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "interrupted call never reached the browser"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
#[ignore = "requires Chromium and the real Chrome DevTools MCP worker"]
async fn recovers_from_browser_worker_and_broker_process_deaths() {
    let root = std::env::temp_dir().join(format!(
        "opencode-browser-mcp-runtime-crash-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let worker_pids = root.join("workers");
    let launcher = root.join("downstream");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> '{}'\nexec bunx \"$@\"\n",
            worker_pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let make_client = || async {
        let mut command = adapter_command(&root, true, &profile, port, "shared-window");
        command.env("OPENCODE_BROWSER_DOWNSTREAM_COMMAND", &launcher);
        ().serve(TokioChildProcess::new(command).unwrap())
            .await
            .unwrap()
    };
    let a = make_client().await;
    let b = make_client().await;
    let (url, site) = start_test_site().await;
    call_ok(
        &a,
        "navigate_page",
        "crash-a",
        json!({"url": format!("{url}/#owned-a")}),
    )
    .await;
    call_ok(
        &b,
        "navigate_page",
        "crash-b",
        json!({"url": format!("{url}/#owned-b")}),
    )
    .await;
    create_unowned_tab(port).await;
    let original = browser_tabs(port).await;
    let page_a = original
        .iter()
        .find(|tab| {
            tab["url"]
                .as_str()
                .is_some_and(|url| url.ends_with("#owned-a"))
        })
        .unwrap();
    let generation = browser_json(port, "version").await;

    // Observe the crash at browser scope: a renderer socket may disappear first.
    let (mut browser_events, _) =
        connect_async(generation["webSocketDebuggerUrl"].as_str().unwrap())
            .await
            .unwrap();
    cdp_call(
        &mut browser_events,
        1,
        "Target.setDiscoverTargets",
        json!({"discover": true}),
    )
    .await;
    // Page.crash can stall in CI's crash handler. SIGKILL bypasses it while
    // still requiring a real crash of the renderer for this exact page.
    let renderer_pid = renderer_process_id(&mut browser_events, page_a).await;
    kill_process_tree(renderer_pid);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = browser_events.next().await {
            if let Message::Text(text) = message.unwrap() {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["method"] == "Target.targetCrashed"
                    && event["params"]["targetId"] == page_a["id"]
                {
                    return;
                }
            }
        }
        panic!("browser closed before reporting the renderer crash");
    })
    .await
    .expect("renderer crash event");
    drop(browser_events);
    call_ok(
        &a,
        "navigate_page",
        "crash-a",
        json!({"url": format!("{url}/#owned-a")}),
    )
    .await;
    call_ok(
        &b,
        "navigate_page",
        "crash-b",
        json!({"url": format!("{url}/#owned-b")}),
    )
    .await;
    assert_eq!(browser_json(port, "version").await, generation);

    // Dead workers are replaced before dispatch, preserving stable tab ownership.
    kill_recorded_workers(&worker_pids);
    let view_a = call_ok(&a, "list_pages", "crash-a", json!({})).await;
    let view_b = call_ok(&b, "list_pages", "crash-b", json!({})).await;
    isolated_view(&view_a, "#owned-a", "#owned-b");
    isolated_view(&view_b, "#owned-b", "#owned-a");

    // An interrupted call reports failure and is not replayed on a new worker.
    let interrupted = a.call_tool(request("evaluate_script", "crash-a", json!({"function": "async () => { window.workerCrashCount = (window.workerCrashCount || 0) + 1; await new Promise(resolve => setTimeout(resolve, 30000)); return window.workerCrashCount; }"})));
    let kill = async {
        wait_for_page_counter(page_a, "workerCrashCount").await;
        kill_recorded_workers(&worker_pids);
    };
    let (result, ()) = tokio::join!(interrupted, kill);
    assert!(result.unwrap().is_error.unwrap_or(false));
    let counter = call_ok(
        &a,
        "evaluate_script",
        "crash-a",
        json!({"function": "() => window.workerCrashCount"}),
    )
    .await;
    assert!(
        text(&counter).contains("\n1\n"),
        "interrupted worker action was replayed: {}",
        text(&counter)
    );

    // Broker death leaves the upstream stdio connection and surviving browser intact.
    let old_pid = broker_pid(&socket).await;
    let interrupted = a.call_tool(request("evaluate_script", "crash-a", json!({"function": "async () => { window.brokerCrashCount = (window.brokerCrashCount || 0) + 1; await new Promise(resolve => setTimeout(resolve, 30000)); return window.brokerCrashCount; }"})));
    let kill = async {
        wait_for_page_counter(page_a, "brokerCrashCount").await;
        kill_process(old_pid, libc::SIGKILL);
        wait_for_profile_lease(&profile).await;
    };
    let (result, ()) = tokio::join!(interrupted, kill);
    let result = result.unwrap();
    assert!(result.is_error.unwrap_or(false));
    assert!(text(&result).contains("may have taken effect"));
    let counter = call_ok(
        &a,
        "evaluate_script",
        "crash-a",
        json!({"function": "() => window.brokerCrashCount"}),
    )
    .await;
    assert!(
        text(&counter).contains("\n1\n"),
        "interrupted broker action was replayed: {}",
        text(&counter)
    );
    isolated_view(
        &call_ok(&b, "list_pages", "crash-b", json!({})).await,
        "#owned-b",
        "#owned-a",
    );
    assert_eq!(browser_json(port, "version").await, generation);
    let surviving = browser_tabs(port).await;
    for tab in &original {
        assert!(
            surviving.iter().any(|survivor| survivor["id"] == tab["id"]),
            "recovery closed a surviving tab: {tab}"
        );
    }

    // Browser death starts a new generation and discards old target mappings.
    kill_process(browser_process_id(port).await, libc::SIGKILL);
    wait_for_browser_exit(port).await;
    let empty = call_ok(&a, "list_pages", "crash-a", json!({})).await;
    assert_eq!(empty.structured_content.unwrap()["pages"], json!([]));
    assert_ne!(
        browser_json(port, "version").await["webSocketDebuggerUrl"],
        generation["webSocketDebuggerUrl"]
    );
    call_ok(
        &a,
        "navigate_page",
        "crash-a",
        json!({"url": format!("{url}/#new-a")}),
    )
    .await;
    call_ok(
        &b,
        "navigate_page",
        "crash-b",
        json!({"url": format!("{url}/#new-b")}),
    )
    .await;
    isolated_view(
        &call_ok(&a, "list_pages", "crash-a", json!({})).await,
        "#new-a",
        "#new-b",
    );
    isolated_view(
        &call_ok(&b, "list_pages", "crash-b", json!({})).await,
        "#new-b",
        "#new-a",
    );

    // An OOM kill can take out the browser and broker together.
    let browser_pid = browser_process_id(port).await;
    kill_process(broker_pid(&socket).await, libc::SIGKILL);
    kill_process(browser_pid, libc::SIGKILL);
    wait_for_profile_lease(&profile).await;
    wait_for_browser_exit(port).await;
    let empty = call_ok(&b, "list_pages", "crash-b", json!({})).await;
    assert_eq!(empty.structured_content.unwrap()["pages"], json!([]));
    call_ok(
        &a,
        "navigate_page",
        "crash-a",
        json!({"url": format!("{url}/#after-group-kill")}),
    )
    .await;

    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
    wait_for_broker_exit(&socket, &profile).await;
    wait_for_browser_exit(port).await;
    site.abort();
    std::fs::remove_dir_all(root).unwrap();
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

fn isolated_view(result: &CallToolResult, own_url: &str, other_url: &str) -> String {
    if let Some(pages) = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("pages"))
        .and_then(Value::as_array)
    {
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
    }

    let text = text(result);
    assert!(
        text.contains(own_url),
        "page text did not contain {own_url}: {text}"
    );
    assert!(
        !text.contains(other_url),
        "page text exposed {other_url}: {text}"
    );
    text
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
    let navigate_a = isolated_view(&navigate_a, "example.com", "example.org");
    let navigate_b = isolated_view(&navigate_b, "example.org", "example.com");
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
    let pages_a = isolated_view(&pages_a, "Session%20A", "Session%20B");
    let pages_b = isolated_view(&pages_b, "Session%20B", "Session%20A");
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
    let extra_a_text = isolated_view(&extra_a, "extra-a", "Session%20B");
    let extra_b_text = isolated_view(&extra_b, "Session%20B", "extra-a");
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
    isolated_view(&pages_b, "Session%20B", "Session%20A");

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the real Chrome DevTools MCP worker"]
async fn exports_allow_workspace_and_scratch_but_reject_outside_paths() {
    let root = std::env::temp_dir().join(format!("browser-mcp-exports-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = root.join("profile");
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let direct = client(&root, true, &profile, port, "shared-window").await;
    let compact = client(&root, false, &profile, port, "shared-window").await;
    let scratch =
        Path::new("/tmp/opencode").join(format!("browser-mcp-exports-{}", std::process::id()));
    std::fs::create_dir(&scratch).unwrap();
    let workspace_file = std::env::current_dir()
        .unwrap()
        .join(format!(".browser-mcp-export-{}.txt", std::process::id()));
    call_ok(
        &direct,
        "navigate_page",
        "exports",
        json!({
            "url": "data:text/html,<title>Export%20boundary</title><p>scratch export marker</p>"
        }),
    )
    .await;
    call_ok(
        &direct,
        "take_snapshot",
        "exports",
        json!({"filePath": workspace_file}),
    )
    .await;
    assert!(
        std::fs::read_to_string(&workspace_file)
            .unwrap()
            .contains("scratch export marker")
    );
    codex_call(
        &compact,
        "navigate_page",
        "exports",
        json!({
            "url": "data:text/html,<title>Compact%20export</title><p>compact export marker</p>"
        }),
    )
    .await;
    let scratch_file = scratch.join("compact.txt");
    codex_call(
        &compact,
        "take_snapshot",
        "exports",
        json!({"filePath": scratch_file}),
    )
    .await;
    assert!(
        std::fs::read_to_string(&scratch_file)
            .unwrap()
            .contains("compact export marker")
    );

    let home = std::env::var_os("HOME").expect("test requires HOME");
    let filename = format!("browser-mcp-forbidden-{}.txt", std::process::id());
    let outside = Path::new(&home).join(&filename);
    std::os::unix::fs::symlink(&home, scratch.join("outside")).unwrap();
    for path in [&outside, &scratch.join("outside").join(filename)] {
        let result = direct
            .call_tool(request(
                "take_snapshot",
                "exports",
                json!({"filePath": path}),
            ))
            .await
            .unwrap();
        assert!(
            result.is_error.unwrap_or(false),
            "export escaped roots: {}",
            text(&result)
        );
        assert!(!outside.exists(), "outside export was written");
    }
    codex_end(&root, &profile, port, "exports").await;
    direct.cancel().await.unwrap();
    compact.cancel().await.unwrap();
    wait_for_broker_exit(&broker_socket(&profile), &profile).await;
    std::fs::remove_file(workspace_file).unwrap();
    std::fs::remove_dir_all(scratch).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the real Chrome DevTools MCP worker"]
async fn codex_hooks_share_profile_isolate_sessions_and_queue_cleanup() {
    let root =
        std::env::temp_dir().join(format!("opencode-browser-mcp-codex-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = root.join("profile");
    let socket = broker_socket(&profile);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    codex_end(&root, &profile, port, "unused").await;
    assert!(!socket.exists(), "cleanup started an unused broker");
    let open_code = client(&root, true, &profile, port, "shared-window").await;
    let codex = client(&root, false, &profile, port, "shared-window").await;
    let (site, site_task) = start_test_site().await;
    call_ok(
        &open_code,
        "navigate_page",
        "same",
        json!({"url": format!("{site}/opencode")}),
    )
    .await;
    codex_call(
        &codex,
        "navigate_page",
        "same",
        json!({"url": format!("{site}/codex-a")}),
    )
    .await;
    codex_call(
        &codex,
        "navigate_page",
        "other",
        json!({"url": format!("{site}/codex-b")}),
    )
    .await;
    create_unowned_tab(port).await;
    let pages = codex_call(&codex, "list_pages", "same", json!({})).await;
    let view = isolated_view(&pages, "/codex-a", "/codex-b");
    assert!(!view.contains("/opencode") && !view.contains("user-tab"));
    let pages = call_ok(&open_code, "list_pages", "same", json!({})).await;
    isolated_view(&pages, "/opencode", "/codex-a");
    codex_call(&codex, "evaluate_script", "same", json!({
        "function": "() => { document.cookie = 'codexShared=yes;path=/'; return document.cookie; }"
    })).await;
    let shared = call_ok(
        &open_code,
        "evaluate_script",
        "same",
        json!({"function": "() => document.cookie"}),
    )
    .await;
    assert!(
        text(&shared).contains("codexShared=yes"),
        "clients did not share the browser profile"
    );
    let tabs = browser_tabs(port).await;
    let page = tabs
        .iter()
        .find(|tab| {
            tab["url"]
                .as_str()
                .is_some_and(|url| url.contains("/codex-a"))
        })
        .unwrap();
    let active = codex.send_cancellable_request(
        rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
            codex_request("evaluate_script", "same", json!({
                "function": "async () => { window.codexBusy = 1; await new Promise(resolve => setTimeout(resolve, 5500)); return 'finished'; }"
            })).await)), rmcp::service::PeerRequestOptions::no_options()).await.unwrap();
    wait_for_page_counter(page, "codexBusy").await;
    codex_end(&root, &profile, port, "same").await;
    active
        .await_response()
        .await
        .expect("cleanup interrupted the active call");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tabs = browser_tabs(port).await;
        if !tabs_contain_url(&tabs, "/codex-a") {
            for url in ["/opencode", "/codex-b", "user-tab"] {
                assert!(
                    tabs_contain_url(&tabs, url),
                    "cleanup closed another owner's tab: {url}"
                );
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queued Codex cleanup did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    codex_end(&root, &profile, port, "other").await;
    open_code.cancel().await.unwrap();
    codex.cancel().await.unwrap();
    wait_for_broker_exit(&socket, &profile).await;
    site_task.abort();
    let _ = site_task.await;
    std::fs::remove_dir_all(root).unwrap();
}
