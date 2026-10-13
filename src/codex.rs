use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf};

use crate::{
    Config,
    broker::{CODEX_SESSION_KEY, SESSION_KEY, SessionClient, queue_codex_release},
};

pub(super) async fn run(config: Config, profile: Option<PathBuf>) -> Result<()> {
    let event = serde_json::from_reader(std::io::stdin().lock())
        .context("invalid Codex browser hook input")
        .and_then(read_event);
    match event {
        Ok(HookEvent::Ignore) => Ok(()),
        Ok(HookEvent::Rewrite(output)) => {
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer(&mut stdout, &output)?;
            writeln!(stdout)?;
            Ok(())
        }
        Ok(HookEvent::Release(id)) => queue_codex_release(config, profile, id).await,
        Err(error) => {
            eprintln!("Codex browser hook failed: {error:#}");
            // Exit 2 blocks PreToolUse; malformed routing must fail closed.
            std::process::exit(2);
        }
    }
}

enum HookEvent {
    Ignore,
    Rewrite(Value),
    Release(String),
}

fn read_event(mut input: Value) -> Result<HookEvent> {
    let event = input["hook_event_name"]
        .as_str()
        .context("Codex browser hook is missing hook_event_name")?;
    match event {
        "PreToolUse" => {
            let name = input["tool_name"]
                .as_str()
                .context("Codex browser hook is missing tool_name")?;
            // Codex namespaces plugin MCP servers with plugin_<plugin name>_.
            if !name.starts_with("mcp__browser__")
                && !name.starts_with("mcp__plugin_browser_browser__")
            {
                return Ok(HookEvent::Ignore);
            }
            let id = session_id(&mut input)?;
            let mut arguments = input["tool_input"].take();
            let object = arguments
                .as_object_mut()
                .context("Codex browser tool_input must be an object")?;
            // Routing identity comes from Codex, never from model arguments.
            object.remove(SESSION_KEY);
            object.insert(CODEX_SESSION_KEY.to_owned(), Value::from(id));
            let mut output = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": null
                }
            });
            output["hookSpecificOutput"]["updatedInput"] = arguments;
            Ok(HookEvent::Rewrite(output))
        }
        "SessionEnd" => Ok(HookEvent::Release(session_id(&mut input)?)),
        _ => bail!("unsupported Codex browser hook event {event}"),
    }
}

fn session_id(input: &mut Value) -> Result<String> {
    let Value::String(id) = input["session_id"].take() else {
        bail!("Codex browser hook is missing session_id");
    };
    SessionClient::Codex.validate_id(&id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_SESSION_ID_BYTES;

    #[test]
    fn routes_only_browser_tools_using_the_codex_identity() {
        for name in [
            "mcp__browser__dispatch",
            "mcp__plugin_browser_browser__help",
        ] {
            let input = json!({
                "hook_event_name": "PreToolUse", "session_id": "thread-a",
                "tool_name": name,
                "tool_input": {"action": "list_pages", "arguments": {},
                    (SESSION_KEY): "forged-open-code", (CODEX_SESSION_KEY): "forged-codex"}
            });
            let HookEvent::Rewrite(output) = read_event(input).unwrap() else {
                panic!("missing rewrite")
            };
            let arguments = &output["hookSpecificOutput"]["updatedInput"];
            assert_eq!(arguments[CODEX_SESSION_KEY], "thread-a");
            assert!(arguments.get(SESSION_KEY).is_none());
            assert_eq!(arguments["action"], "list_pages");
            assert_eq!(arguments["arguments"], json!({}));
        }
        assert!(matches!(
            read_event(json!({"hook_event_name": "PreToolUse",
            "tool_name": "mcp__unrelated__dispatch"}))
            .unwrap(),
            HookEvent::Ignore
        ));
    }

    #[test]
    fn rejects_missing_identity_and_malformed_arguments() {
        let mut input = json!({"hook_event_name": "PreToolUse", "session_id": "thread-a",
            "tool_name": "mcp__browser__dispatch", "tool_input": {}});
        input["tool_input"] = json!([]);
        assert!(read_event(input.clone()).is_err());
        input["tool_input"] = json!({});
        for id in [
            json!(null),
            json!(""),
            json!("a".repeat(MAX_SESSION_ID_BYTES)),
        ] {
            input["session_id"] = id;
            assert!(read_event(input.clone()).is_err());
        }
        assert!(
            matches!(read_event(json!({"hook_event_name": "SessionEnd", "session_id": "thread-a"}))
            .unwrap(), HookEvent::Release(id) if id == "thread-a")
        );
    }
}
