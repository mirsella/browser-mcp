use rmcp::model::{CallToolResult, ContentBlock};

pub fn network_result(result: &mut CallToolResult) {
    if let Some(data) = result.structured_content.as_mut()
        && data.as_object().is_some_and(|object| {
            object.contains_key("networkRequests") || object.contains_key("networkRequest")
        })
    {
        super::privacy::network(data);
        result
            .content
            .retain(|block| !matches!(block, ContentBlock::Text(_)));
        result
            .content
            .insert(0, ContentBlock::text(data.to_string()));
    }
}

pub fn model_result(mut result: CallToolResult) -> CallToolResult {
    // Ownership, snapshot and network processing have already consumed this data.
    // Pinned upstream declares no outputSchema, so avoid sending both copies.
    result.structured_content = None;
    let mut remaining = 32 * 1024;
    let mut truncated = false;
    for block in &mut result.content {
        if let ContentBlock::Text(text) = block {
            if let std::borrow::Cow::Owned(redacted) = super::privacy::known(&text.text) {
                text.text = redacted;
            }
            if text.text.len() > remaining {
                let mut end = remaining;
                while !text.text.is_char_boundary(end) {
                    end -= 1;
                }
                text.text.truncate(end);
                truncated = true;
            }
            remaining -= text.text.len();
        }
    }
    if truncated {
        result.content.push(ContentBlock::text("[Output limited to 32 KiB. Narrow the snapshot, paginate lists, or export to a workspace file.]"));
    }
    tracing::info!(
        text_bytes = 32 * 1024 - remaining,
        truncated,
        "model-facing browser response"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn avoids_duplicate_snapshot_and_bounds_utf8_text() {
        let mut result = CallToolResult::success(vec![ContentBlock::text("é".repeat(20000))]);
        result.structured_content = Some(serde_json::json!({"snapshot":{"id":"raw"}}));
        let result = model_result(result);
        assert!(result.structured_content.is_none());
        assert_eq!(result.content[0].as_text().unwrap().text.len(), 32768);
        assert!(
            result.content[1]
                .as_text()
                .unwrap()
                .text
                .contains("limited")
        );
    }
}
