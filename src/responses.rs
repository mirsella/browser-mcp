use anyhow::{Context, Result, ensure};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

pub fn attached_page_notices(result: &mut CallToolResult) -> Result<()> {
    let Some(data) = result
        .structured_content
        .as_mut()
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    if let Some(pages) = data.remove("openedPages") {
        let pages = pages
            .as_array()
            .context("invalid upstream opened-page list")?;
        ensure!(!pages.is_empty(), "empty upstream opened-page list");
        let mut section = String::from("## Opened pages");
        for page in pages {
            let id = page
                .get("id")
                .and_then(Value::as_u64)
                .context("invalid opened-page ID")?;
            let title = page
                .get("title")
                .and_then(Value::as_str)
                .context("invalid opened-page title")?;
            let url = page
                .get("url")
                .and_then(Value::as_str)
                .context("invalid opened-page URL")?;
            section.push_str(&format!("\n{id}: "));
            if title.is_empty() {
                section.push_str(url);
            } else {
                section.push_str(&format!("{title} ({url})"));
            }
        }
        let mut matches = 0;
        for block in &mut result.content {
            let ContentBlock::Text(text) = block else {
                continue;
            };
            let mut ranges = text
                .text
                .match_indices(&section)
                .filter_map(|(start, section)| {
                    let end = start + section.len();
                    let prefix = &text.text[..start];
                    let rest = &text.text[end..];
                    if (prefix.is_empty() || prefix.ends_with('\n'))
                        && (rest.is_empty() || rest.starts_with('\n'))
                    {
                        Some(start..end + usize::from(rest.starts_with('\n')))
                    } else {
                        None
                    }
                });
            if let Some(range) = ranges.next() {
                ensure!(
                    matches == 0 && ranges.next().is_none(),
                    "ambiguous upstream opened-page notice"
                );
                matches += 1;
                text.text.replace_range(range, "");
            }
        }
        ensure!(
            matches == 1,
            "upstream opened-page notice does not match its structured list"
        );
    }
    if let Some(url) = data.get_mut("navigatedToUrl") {
        let raw = url.as_str().context("invalid upstream navigation URL")?;
        let safe = super::privacy::url(raw);
        if raw != safe {
            let original = format!("Page navigated to {raw}.");
            let redacted = format!("Page navigated to {safe}.");
            for block in &mut result.content {
                if let ContentBlock::Text(text) = block {
                    text.text = text.text.replace(&original, &redacted);
                }
            }
            *url = safe.into();
        }
    }
    Ok(())
}

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
    fn attached_page_notices_do_not_disclose_raw_urls_or_ids() {
        let mut result = CallToolResult::success(vec![ContentBlock::text(
            "Page navigated to https://example.com/?token=hidden.\n## Opened pages\n17: https://popup.test/?token=hidden\n## Latest page snapshot\nuid=1_0\n",
        )]);
        result.structured_content = Some(serde_json::json!({
            "navigatedToUrl":"https://example.com/?token=hidden",
            "openedPages":[{"id":17,"title":"","url":"https://popup.test/?token=hidden"}]
        }));
        attached_page_notices(&mut result).unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.starts_with("Page navigated to https://example.com/"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("17:"));
        assert!(text.ends_with("## Latest page snapshot\nuid=1_0\n"));
        assert!(
            result
                .structured_content
                .unwrap()
                .get("openedPages")
                .is_none()
        );
    }
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
