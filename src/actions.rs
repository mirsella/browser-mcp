//! Broker-owned actions and the deliberately small, pinned JSON-schema dialect.
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Tool {
    Tool::new(name, description, json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).as_object().unwrap().clone())
        .with_annotations(ToolAnnotations::new().read_only(read_only).destructive(!read_only).open_world(!read_only))
}

pub fn builtins(secrets: bool) -> Vec<Tool> {
    let mut tools = vec![
        tool(
            "status",
            "Observe this session without starting a browser or worker.",
            json!({}),
            &[],
            true,
        ),
        tool(
            "select_page",
            "Change the session's current owned page. Does not focus the window unless requested.",
            json!({"page":{"type":"string"},"bringToFront":{"type":"boolean","default":false}}),
            &["page"],
            false,
        ),
        tool(
            "handoff",
            "Give a headed page to the user; finish only after their confirmation. Nonblocking; end your turn after start.",
            json!({"mode":{"type":"string","enum":["start","finish"]},"page":{"type":"string"},"message":{"type":"string","maxLength":1000}}),
            &["mode"],
            false,
        ),
        tool(
            "batch",
            "Run up to 8 known actions in order, stopping on error without replay. Only the last step may produce a snapshot. Compact mode only.",
            json!({"steps":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{"action":{"type":"string"},"arguments":{"type":"object","additionalProperties":true}},"required":["action"],"additionalProperties":false}}}),
            &["steps"],
            false,
        ),
    ];
    if secrets {
        tools.push(tool("fill_secret", "Fill a snapshot input from the configured secrets file, on an allowed HTTPS top-level origin. No secret value is returned.", json!({"name":{"type":"string"},"uid":{"type":"string"},"page":{"type":"string"}}), &["name","uid"], false));
    }
    tools
}

pub fn expose_upstream(name: &str, recording: bool) -> bool {
    match name {
        "screencast_start" | "screencast_stop" => recording,
        "click"
        | "close_page"
        | "drag"
        | "emulate"
        | "evaluate_script"
        | "fill"
        | "fill_form"
        | "get_console_message"
        | "get_network_request"
        | "handle_dialog"
        | "hover"
        | "lighthouse_audit"
        | "list_console_messages"
        | "list_network_requests"
        | "list_pages"
        | "navigate_page"
        | "new_page"
        | "performance_analyze_insight"
        | "performance_start_trace"
        | "performance_stop_trace"
        | "press_key"
        | "resize_page"
        | "take_heapsnapshot"
        | "take_screenshot"
        | "take_snapshot"
        | "type_text"
        | "upload_file"
        | "wait_for" => true,
        _ => false,
    }
}

pub fn extend(tool: &mut Tool, page_scoped: bool) -> Result<()> {
    if tool.output_schema.is_some() {
        bail!(
            "upstream {} now declares an output schema; update response handling before upgrading the worker",
            tool.name
        );
    }
    let mut schema = (*tool.input_schema).clone();
    check_schema(&Value::Object(schema.clone()))?;
    schema.insert("additionalProperties".into(), false.into());
    let required = match tool.name.as_ref() {
        "evaluate_script" => Some(json!(["function"])),
        "upload_file" => Some(json!(["uid", "filePath"])),
        _ => None,
    };
    if let Some(required) = required {
        schema.insert("required".into(), required);
    }
    let props = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("{} has no object properties", tool.name))?;
    if page_scoped
        && props.insert("page".into(), json!({"type":"string","description":"Owned page handle from list_pages; omit for current. Does not change current."})).is_some() {
        bail!("upstream {} conflicts with broker page option", tool.name);
    }
    if tool.name == "screencast_start" {
        props.remove("filePath");
        tool.description = Some("Start an explicitly enabled recording to a broker-assigned workspace artifact. Stop it before cleanup.".into());
    }
    let descriptions: &[(&str, &str)] = match tool.name.as_ref() {
        "evaluate_script" => {
            props.remove("sourcePath");
            props.remove("format");
            &[(
                "function",
                "A JavaScript function declaration to execute in the target page, e.g. `() => document.title`.",
            )]
        }
        "upload_file" => {
            let paths = props
                .remove("filePaths")
                .context("upstream upload_file is missing filePaths")?;
            let path = paths
                .get("items")
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("string"))
                .context("upstream upload_file must accept an array of paths")?;
            props.insert("filePath".into(), path.clone());
            &[("filePath", "The local path of the file to upload")]
        }
        "emulate" => &[
            (
                "viewport",
                "Viewport as widthxheightxdevicePixelRatio[,mobile][,touch][,landscape].",
            ),
            (
                "extraHttpHeaders",
                "JSON object of request headers, persisted across navigations. Pass an empty string to clear.",
            ),
            ("geolocation", "Latitude,longitude; omit to clear."),
        ],
        _ => &[],
    };
    for &(key, description) in descriptions {
        let parameter = props
            .get_mut(key)
            .and_then(Value::as_object_mut)
            .with_context(|| format!("upstream {} is missing parameter {key}", tool.name))?;
        parameter.insert("description".into(), description.into());
    }
    if tool.name == "new_page" {
        props.remove("isolatedContext");
        if let Some(background) = props.get_mut("background").and_then(Value::as_object_mut) {
            background.insert("default".into(), true.into());
            background.insert("description".into(), "Open in the background by default. Window-per-session placement may briefly activate its anchor window.".into());
        }
    }
    if tool.name == "take_snapshot" {
        for (key, value) in [
            (
                "find",
                json!({"type":"string","description":"Case-insensitive text search, with ancestors."}),
            ),
            (
                "within",
                json!({"type":"string","description":"Snapshot UID whose subtree to show."}),
            ),
            (
                "interactive",
                json!({"type":"boolean","description":"Show controls, headings and their ancestors."}),
            ),
            (
                "diff",
                json!({"type":"boolean","description":"Show changes since the previous compatible snapshot."}),
            ),
            (
                "maxLines",
                json!({"type":"integer","minimum":1,"maximum":2000,"default":300}),
            ),
        ] {
            if props.insert(key.into(), value).is_some() {
                bail!("upstream take_snapshot conflicts with broker {key} option");
            }
        }
    }
    tool.input_schema = Arc::new(schema);
    Ok(())
}

pub fn check_schema(schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("schema must be an object"))?;
    for (key, value) in object {
        match key.as_str() {
            "type"
                if matches!(
                    value.as_str(),
                    Some("object" | "array" | "string" | "boolean" | "number" | "integer" | "null")
                ) => {}
            "description" | "title" | "$schema" if value.is_string() => {}
            "default" => {}
            "enum" if value.as_array().is_some_and(|values| !values.is_empty()) => {}
            "required"
                if value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string)) => {}
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                if value.is_number() => {}
            "minLength" | "maxLength" | "minItems" | "maxItems" if value.as_u64().is_some() => {}
            "properties" => {
                for child in value
                    .as_object()
                    .ok_or_else(|| anyhow!("invalid properties"))?
                    .values()
                {
                    check_schema(child)?;
                }
            }
            "items" => check_schema(value)?,
            "additionalProperties" if value.is_boolean() => {}
            _ => bail!(
                "unsupported pinned tool schema constraint {key:?}; update the broker validator before upgrading the worker"
            ),
        }
    }
    Ok(())
}

pub fn validate(tool: &Tool, arguments: &Map<String, Value>) -> Result<()> {
    validate_value(
        &tool.input_schema,
        &Value::Object(arguments.clone()),
        "arguments",
    )
    .map_err(|error| anyhow!("{error}. {}", signature(tool)))
}

fn validate_value(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<()> {
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
            "null" => value.is_null(),
            _ => bail!("unsupported schema type {kind}"),
        };
        if !valid {
            bail!("{path} must be {kind}");
        }
    }
    if schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.contains(value))
    {
        bail!("{path} is outside the allowed enum");
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    bail!("{path}.{key} is required");
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, child) in object {
                let child_schema = properties
                    .get(key)
                    .ok_or_else(|| anyhow!("unknown field {path}.{key}"))?;
                validate_value(
                    child_schema.as_object().expect("catalog schema checked"),
                    child,
                    &format!("{path}.{key}"),
                )?;
            }
        } else if schema.get("additionalProperties") == Some(&Value::Bool(false))
            && !object.is_empty()
        {
            bail!("{path} accepts no fields");
        }
    }
    if let Some(array) = value.as_array() {
        for (key, too_small) in [("minItems", true), ("maxItems", false)] {
            if let Some(bound) = schema.get(key).and_then(Value::as_u64)
                && if too_small {
                    array.len() < bound as usize
                } else {
                    array.len() > bound as usize
                }
            {
                bail!("{path} violates {key}={bound}");
            }
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                validate_value(
                    items.as_object().expect("catalog schema checked"),
                    item,
                    &format!("{path}[{index}]"),
                )?;
            }
        }
    }
    if let Some(text) = value.as_str() {
        for (key, too_small) in [("minLength", true), ("maxLength", false)] {
            if let Some(bound) = schema.get(key).and_then(Value::as_u64)
                && if too_small {
                    text.chars().count() < bound as usize
                } else {
                    text.chars().count() > bound as usize
                }
            {
                bail!("{path} violates {key}={bound}");
            }
        }
    }
    if let Some(number) = value.as_f64() {
        for (key, valid) in [
            (
                "minimum",
                schema
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|n| number >= n),
            ),
            (
                "maximum",
                schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|n| number <= n),
            ),
            (
                "exclusiveMinimum",
                schema
                    .get("exclusiveMinimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|n| number > n),
            ),
            (
                "exclusiveMaximum",
                schema
                    .get("exclusiveMaximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|n| number < n),
            ),
        ] {
            if !valid {
                bail!("{path} violates {key}");
            }
        }
    }
    Ok(())
}

pub fn signature(tool: &Tool) -> String {
    let required = tool.input_schema.get("required").and_then(Value::as_array);
    let args = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, schema)| {
            let mark = if required
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(name)))
            {
                ""
            } else {
                "?"
            };
            let kind = schema.get("enum").map(Value::to_string).unwrap_or_else(|| {
                schema
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("value")
                    .into()
            });
            format!("{name}{mark}: {kind}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({args})", tool.name)
}

pub fn help(tool: &Tool) -> String {
    fn compact(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            for key in ["$schema", "title"] {
                object.remove(key);
            }
            for child in object.values_mut() {
                compact(child);
            }
        } else if let Some(array) = value.as_array_mut() {
            for child in array {
                compact(child);
            }
        }
    }
    let mut schema = Value::Object((*tool.input_schema).clone());
    compact(&mut schema);
    format!(
        "{}\n{}\n{schema}",
        signature(tool),
        tool.description.as_deref().unwrap_or_default(),
    )
}

pub fn batch_steps(
    arguments: &Map<String, Value>,
    tools: &[Tool],
) -> Result<Vec<(String, Map<String, Value>)>> {
    let steps = arguments
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("batch steps must be an array"))?;
    if steps.is_empty() || steps.len() > 8 {
        bail!("batch requires 1..8 steps");
    }
    let mut planned = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let name = step
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("step {} has no action", index + 1))?;
        if !matches!(
            name,
            "navigate_page"
                | "click"
                | "hover"
                | "fill"
                | "fill_form"
                | "press_key"
                | "type_text"
                | "drag"
                | "wait_for"
                | "take_snapshot"
                | "evaluate_script"
                | "list_pages"
                | "list_network_requests"
                | "get_network_request"
                | "list_console_messages"
                | "get_console_message"
        ) {
            bail!("action {name} is not allowed in a batch");
        }
        let empty = Map::new();
        let args = match step.get("arguments") {
            None => &empty,
            Some(Value::Object(args)) => args,
            _ => bail!("step arguments must be an object"),
        };
        if index + 1 < steps.len()
            && (matches!(name, "take_snapshot" | "wait_for")
                || args.get("includeSnapshot") == Some(&Value::Bool(true)))
        {
            bail!(
                "only the final batch step may produce a snapshot; use refs from an earlier visible snapshot"
            );
        }
        let tool = tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| anyhow!("unknown batch action {name}"))?;
        validate(tool, args)?;
        planned.push((name.to_owned(), args.clone()));
    }
    Ok(planned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<Tool> {
        let mut tools: Vec<Tool> = serde_json::from_str(include_str!(
            "../tests/fixtures/chrome-devtools-mcp-1.13.0.json"
        ))
        .unwrap();
        tools.retain(|tool| expose_upstream(&tool.name, false));
        for tool in &mut tools {
            let mut schema = (*tool.input_schema).clone();
            let scoped = crate::remove_schema_property(&mut schema, "pageId");
            tool.input_schema = Arc::new(schema);
            extend(tool, scoped).unwrap();
        }
        tools.extend(builtins(false));
        tools
    }

    #[test]
    fn context_budgets_and_pinned_schema_dialect() {
        let tools = catalog();
        let initial = serde_json::to_vec(&crate::broker::compact_tools())
            .unwrap()
            .len();
        let action_names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
            .len();
        let (maximum, longest) = tools
            .iter()
            .map(|tool| (help(tool).len(), tool.name.as_ref()))
            .max()
            .unwrap();
        println!(
            "context bytes: initial={initial}, names={action_names}, largest action help={maximum}, full direct={}",
            serde_json::to_vec(&tools).unwrap().len()
        );
        assert_eq!(crate::broker::compact_tools().len(), 2);
        assert!(initial <= 1800, "initial context grew: {initial}");
        assert!(action_names <= 1200);
        assert!(maximum <= 2400, "{longest} action help grew: {maximum}");
        assert!(include_str!("../skills/browser-automation/SKILL.md").len() <= 4000);
        let evaluate = tools
            .iter()
            .find(|tool| tool.name == "evaluate_script")
            .unwrap();
        assert!(help(evaluate).contains("uid of an element"));
        assert!(validate(evaluate, &Map::new()).is_err());
        assert!(!expose_upstream("unreviewed_tool", true));
        assert!(!expose_upstream("screencast_start", false));
        let resize = tools
            .iter()
            .find(|tool| tool.name == "resize_page")
            .unwrap();
        assert!(
            validate(
                resize,
                json!({"width":800.0,"height":600}).as_object().unwrap()
            )
            .is_ok()
        );
        assert!(
            validate(
                resize,
                json!({"width":800.5,"height":600}).as_object().unwrap()
            )
            .is_err()
        );
        let upload = tools
            .iter()
            .find(|tool| tool.name == "upload_file")
            .unwrap();
        assert!(
            validate(
                upload,
                json!({"uid":"1_2","filePath":"a.txt"}).as_object().unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn validates_before_effects_and_rejects_new_constraints() {
        let tools = catalog();
        let fill = tools.iter().find(|tool| tool.name == "fill").unwrap();
        assert!(
            validate(
                fill,
                json!({"uid":"v1:1_2","value":"hello","typo":true})
                    .as_object()
                    .unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("typo")
        );
        assert!(validate(fill, json!({"uid":1,"value":"hello"}).as_object().unwrap()).is_err());
        assert!(check_schema(&json!({"type":"string","pattern":".*"})).is_err());
        assert!(check_schema(&json!({"type":["string","null"]})).is_err());
        assert!(check_schema(&json!({"type":"string","maxLength":"10"})).is_err());
        let invalid = json!({"steps":[{"action":"click","arguments":{"uid":"v1:1_2"}},{"action":"fill","arguments":{"uid":"v1:1_3"}}]});
        assert!(batch_steps(invalid.as_object().unwrap(), &tools).is_err());
        let intermediate = json!({"steps":[{"action":"take_snapshot"},{"action":"click","arguments":{"uid":"v1:1_2"}}]});
        assert!(batch_steps(intermediate.as_object().unwrap(), &tools).is_err());
        assert!(
            batch_steps(
                json!({"steps":[{"action":"batch","arguments":{}}]})
                    .as_object()
                    .unwrap(),
                &tools
            )
            .is_err()
        );
    }
}
