//! Model-facing page snapshot views.
//!
//! Upstream structured snapshots (`--experimental-structured-content`) are the
//! parser truth. The upstream text is only used to locate the snapshot section
//! that the rendered view replaces. Displayed UIDs carry a per-worker
//! namespace so references from a replaced worker are rejected instead of
//! silently addressing a different element.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value};
use tracing::{info, warn};

const MARKER: &str = "## Latest page snapshot\n";
const SELECTED_NOTE: &str = "Note: there is a selected element";
const VIEW_KEYS: [&str; 5] = ["find", "within", "interactive", "diff", "maxLines"];
const DEFAULT_LINES: usize = 300;
const MAX_LINES: usize = 2000;
const MAX_FIND_MATCHES: usize = 50;
const MAX_FIELD_CHARS: usize = 300;
const MAX_BASELINE_BYTES: usize = 2 * 1024 * 1024;
const NONCE_LEN: u32 = 13;
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "checkbox",
    "combobox",
    "heading",
    "link",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "textbox",
    "treeitem",
];
const ADDED: u8 = 1;
const CHANGED: u8 = 2;
const MOVED: u8 = 4;

/// Snapshot view state for one downstream worker. Replace it with
/// `ViewState::default()` whenever the worker is replaced.
#[derive(Debug)]
pub struct ViewState {
    prefix: String,
    baselines: HashMap<String, Baseline>,
    bytes: usize,
    clock: u64,
}

/// What `prepare` learned about one call, consumed by `finish`.
#[derive(Debug, Clone)]
pub struct ViewRequest {
    source: Source,
    verbose: bool,
    /// Lowercased search text.
    find: Option<String>,
    /// Raw upstream UID.
    within: Option<String>,
    interactive: bool,
    diff: bool,
    max_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Explicit,
    SavedToFile,
    Embedded,
}

#[derive(Debug)]
struct Baseline {
    verbose: bool,
    index: HashMap<String, u32>,
    nodes: Vec<BaseNode>,
    bytes: usize,
    used: u64,
}

#[derive(Debug)]
struct BaseNode {
    id: String,
    rest: String,
    content_hash: u128,
    parent: Option<u32>,
    /// Index among the parent's children.
    pos: u32,
}

/// Parsed snapshot in preorder; index 0 is the root document.
struct Tree {
    nodes: Vec<Node>,
    duplicate: bool,
}

struct Node {
    id: String,
    /// Rendered line after `uid=…`.
    rest: String,
    content_hash: u128,
    matches_find: bool,
    /// Physical lines in the unescaped upstream node, including its final newline.
    source_lines: usize,
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
    /// End of this node's preorder subtree range.
    end: usize,
    interactive: bool,
}

struct Lines {
    lines: Vec<String>,
    limit: usize,
    omitted: usize,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            prefix: format!("v{}:", new_nonce()),
            baselines: HashMap::new(),
            bytes: 0,
            clock: 0,
        }
    }
}

impl ViewRequest {
    fn embedded() -> Self {
        Self {
            source: Source::Embedded,
            verbose: false,
            find: None,
            within: None,
            interactive: false,
            diff: false,
            max_lines: DEFAULT_LINES,
        }
    }

    fn filtered(&self) -> bool {
        self.find.is_some() || self.within.is_some() || self.interactive
    }
}

impl ViewState {
    /// Validates and strips view options and rewrites explicit UID fields to
    /// raw upstream UIDs. Call on public arguments before invoking upstream.
    pub fn prepare(&self, action: &str, args: &mut Map<String, Value>) -> Result<ViewRequest> {
        self.rewrite_uids(action, args)?;
        let mut request = ViewRequest::embedded();
        let present = VIEW_KEYS
            .iter()
            .copied()
            .find(|key| args.contains_key(*key));
        if action != "take_snapshot" {
            if let Some(key) = present {
                bail!("`{key}` is only supported by take_snapshot");
            }
            return Ok(request);
        }
        request.verbose = args
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if args.get("filePath").is_some_and(|path| !path.is_null()) {
            if let Some(key) = present {
                bail!(
                    "`{key}` cannot be combined with filePath; saved snapshots are full upstream text"
                );
            }
            request.source = Source::SavedToFile;
            return Ok(request);
        }
        request.source = Source::Explicit;
        request.find = match args.remove("find") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.to_lowercase()),
            Some(_) => bail!("`find` must be a non-empty string"),
        };
        request.within = match args.remove("within") {
            None | Some(Value::Null) => None,
            Some(value) => Some(self.decode_uid("within", &value)?),
        };
        request.interactive = take_bool(args, "interactive")?;
        request.diff = take_bool(args, "diff")?;
        request.max_lines = match args.remove("maxLines") {
            None | Some(Value::Null) => DEFAULT_LINES,
            Some(value) => value
                .as_u64()
                .filter(|lines| (1..=MAX_LINES as u64).contains(lines))
                .ok_or_else(|| anyhow!("`maxLines` must be an integer from 1 to {MAX_LINES}"))?
                as usize,
        };
        if request.diff && request.filtered() {
            bail!("`diff` cannot be combined with find, within or interactive");
        }
        Ok(request)
    }

    /// Replaces the upstream snapshot text with the requested view and
    /// records it as this tab's diff baseline. Leaves `structured_content`
    /// and every other content block untouched.
    pub fn finish(
        &mut self,
        request: &ViewRequest,
        result: &mut CallToolResult,
        tab: &str,
    ) -> Result<()> {
        let snapshot = result
            .structured_content
            .as_ref()
            .and_then(|structured| structured.get("snapshot"))
            .filter(|snapshot| !snapshot.is_null());
        let Some(snapshot) = snapshot else {
            if request.source == Source::Explicit && !result.is_error.unwrap_or(false) {
                bail!("take_snapshot response has no structured snapshot");
            }
            if result.content.iter().any(|block| match block {
                ContentBlock::Text(text) => has_snapshot_marker(&text.text),
                _ => false,
            }) {
                bail!("response has a page snapshot without structured snapshot data");
            }
            return Ok(());
        };
        let tree = Tree::parse(snapshot, request.find.as_deref())
            .context("invalid structured page snapshot")?;
        let (block, range) = result
            .content
            .iter()
            .enumerate()
            .find_map(|(index, block)| match block {
                ContentBlock::Text(text) => locate(&text.text, &tree).map(|range| (index, range)),
                _ => None,
            })
            .ok_or_else(|| anyhow!("page snapshot text does not match its structured snapshot"))?;
        let view = self.render(request, &tree, tab)?;
        let ContentBlock::Text(text) = &mut result.content[block] else {
            unreachable!("snapshot section was located in a text block");
        };
        text.text.replace_range(range, &view);
        self.retain(tab, request.verbose, tree);
        Ok(())
    }

    /// Drops the tab's baseline, e.g. after a detected navigation.
    pub fn invalidate_tab(&mut self, tab: &str) {
        if let Some(old) = self.baselines.remove(tab) {
            self.bytes -= old.bytes;
        }
    }

    fn rewrite_uids(&self, action: &str, args: &mut Map<String, Value>) -> Result<()> {
        for key in ["uid", "from_uid", "to_uid"] {
            if let Some(value) = args.get_mut(key).filter(|value| !value.is_null()) {
                *value = Value::String(self.decode_uid(key, value)?);
            }
        }
        let (list, field) = match action {
            "fill_form" => ("elements", "elements[].uid"),
            "evaluate_script" => ("args", "args[]"),
            _ => return Ok(()),
        };
        let Some(Value::Array(items)) = args.get_mut(list) else {
            return Ok(());
        };
        for item in items {
            let slot = if action == "fill_form" {
                match item
                    .as_object_mut()
                    .and_then(|element| element.get_mut("uid"))
                {
                    Some(uid) => uid,
                    None => continue,
                }
            } else {
                item
            };
            *slot = Value::String(self.decode_uid(field, slot)?);
        }
        Ok(())
    }

    fn decode_uid(&self, field: &str, value: &Value) -> Result<String> {
        let uid = value
            .as_str()
            .ok_or_else(|| anyhow!("`{field}` must be a snapshot uid string"))?;
        if let Some(raw) = uid.strip_prefix(&self.prefix).filter(|raw| !raw.is_empty()) {
            return Ok(raw.to_owned());
        }
        if uid
            .split_once(':')
            .is_some_and(|(namespace, _)| looks_like_namespace(namespace))
        {
            bail!(
                "`{field}` comes from a snapshot of an earlier browser worker; take a new snapshot and use its uids"
            );
        }
        bail!(
            "`{field}` is not a uid from this session's snapshots; copy uids exactly as shown, like `{}1_2`",
            self.prefix
        )
    }

    fn render(&self, request: &ViewRequest, tree: &Tree, tab: &str) -> Result<String> {
        let mut lines = Lines {
            lines: Vec::new(),
            limit: request.max_lines,
            omitted: 0,
        };
        let mut notes = Vec::new();
        let baseline = self
            .baselines
            .get(tab)
            .filter(|base| base.verbose == request.verbose && base.nodes[0].id == tree.nodes[0].id)
            .filter(|_| !tree.duplicate);
        if request.diff || request.source == Source::Embedded {
            if let Some(base) = baseline {
                notes.push(self.render_diff(tree, base, &mut lines));
            } else {
                if request.diff {
                    info!(
                        "snapshot diff requested without a compatible baseline; returning full view"
                    );
                    notes.push("Diff unavailable: no compatible previous snapshot of this tab (first snapshot, navigation, worker restart, verbose change or oversized page). Showing the full snapshot.".to_owned());
                } else {
                    notes.push(
                        "Full snapshot (no compatible previous snapshot to diff against)."
                            .to_owned(),
                    );
                }
                self.render_subtree(tree, 0, &mut lines, |_| true);
            }
        } else if request.filtered() {
            notes.push(self.render_filtered(request, tree, &mut lines)?);
        } else {
            self.render_subtree(tree, 0, &mut lines, |_| true);
        }
        if tree.duplicate {
            notes.push("Warning: this snapshot repeats uids, so it cannot be diffed.".to_owned());
        } else if tree.bytes() > MAX_BASELINE_BYTES {
            notes.push("Note: this page is too large to keep for diffs; the next snapshot will be shown in full.".to_owned());
        }
        let mut out = String::from(MARKER);
        for line in notes.iter().chain(&lines.lines) {
            out.push_str(line);
            out.push('\n');
        }
        if lines.omitted > 0 {
            out.push_str(&format!(
                "[{} more lines omitted; use take_snapshot with find, within, interactive or maxLines (up to {MAX_LINES})]\n",
                lines.omitted
            ));
        }
        Ok(out)
    }

    fn render_filtered(
        &self,
        request: &ViewRequest,
        tree: &Tree,
        lines: &mut Lines,
    ) -> Result<String> {
        let start = match &request.within {
            Some(raw) => tree
                .nodes
                .iter()
                .position(|node| &node.id == raw)
                .ok_or_else(|| {
                    anyhow!("`within` uid is not in the current snapshot; take a new snapshot")
                })?,
            None => 0,
        };
        let mut include = vec![false; tree.nodes.len()];
        include[start] = true;
        let candidate = |index: usize| !request.interactive || tree.nodes[index].interactive;
        let mut notes = Vec::new();
        if request.within.is_some() {
            notes.push("subtree of the `within` element".to_owned());
        }
        if request.interactive {
            notes.push("interactive elements and headings".to_owned());
        }
        let range = start..tree.nodes[start].end;
        if request.find.is_some() {
            let mut matches = 0;
            for index in range {
                if !candidate(index) || !tree.nodes[index].matches_find {
                    continue;
                }
                matches += 1;
                if matches > MAX_FIND_MATCHES {
                    continue;
                }
                let mut current = Some(index);
                while let Some(node) = current.filter(|&node| !include[node]) {
                    include[node] = true;
                    current = tree.nodes[node].parent;
                }
            }
            notes.push(if matches > MAX_FIND_MATCHES {
                format!("first {MAX_FIND_MATCHES} of {matches} find matches with their ancestors")
            } else {
                format!("{matches} find matches with their ancestors")
            });
        } else {
            for index in range {
                include[index] |= candidate(index);
            }
        }
        self.render_subtree(tree, start, lines, |index| include[index]);
        Ok(format!("Filtered view: {}.", notes.join("; ")))
    }

    fn render_subtree(
        &self,
        tree: &Tree,
        start: usize,
        lines: &mut Lines,
        include: impl Fn(usize) -> bool,
    ) {
        walk(tree, start, |index, depth| {
            if !include(index) {
                return false;
            }
            lines.push(|| self.line("", depth, &tree.nodes[index].id, &tree.nodes[index].rest));
            true
        });
    }

    /// Linear diff keyed by uid: text changes, reparenting and sibling
    /// reorders (longest increasing subsequence, O(n log n)).
    fn render_diff(&self, tree: &Tree, base: &Baseline, lines: &mut Lines) -> String {
        let count = tree.nodes.len();
        let mut seen = vec![false; base.nodes.len()];
        let base_of: Vec<Option<u32>> = tree
            .nodes
            .iter()
            .map(|node| {
                let found = base.index.get(&node.id).copied();
                if let Some(old) = found {
                    seen[old as usize] = true;
                }
                found
            })
            .collect();
        let mut mark = vec![0u8; count];
        let mut same_parent = vec![false; count];
        for (index, node) in tree.nodes.iter().enumerate() {
            let Some(old) = base_of[index].map(|old| &base.nodes[old as usize]) else {
                mark[index] = ADDED;
                continue;
            };
            if old.content_hash != node.content_hash {
                mark[index] |= CHANGED;
            }
            same_parent[index] = match (old.parent, node.parent) {
                (None, None) => true,
                (Some(old_parent), Some(parent)) => base_of[parent] == Some(old_parent),
                _ => false,
            };
            if !same_parent[index] {
                mark[index] |= MOVED;
            }
        }
        for node in &tree.nodes {
            let kept: Vec<(usize, u32)> = node
                .children
                .iter()
                .filter(|&&child| same_parent[child])
                .filter_map(|&child| {
                    base_of[child].map(|old| (child, base.nodes[old as usize].pos))
                })
                .collect();
            for (&(child, _), keep) in kept.iter().zip(longest_increasing(&kept, |&(_, pos)| pos)) {
                if !keep {
                    mark[child] |= MOVED;
                }
            }
        }
        let mut show: Vec<bool> = mark.iter().map(|&flags| flags != 0).collect();
        for index in (1..count).rev() {
            if let Some(parent) = tree.nodes[index].parent.filter(|_| show[index]) {
                show[parent] = true;
            }
        }
        walk(tree, 0, |index, depth| {
            if !show[index] {
                return false;
            }
            let node = &tree.nodes[index];
            let flags = mark[index];
            let marker = match flags {
                0 => "  ",
                ADDED => "+ ",
                CHANGED => "~ ",
                MOVED => "> ",
                _ => "~>",
            };
            lines.push(|| self.line(marker, depth, &node.id, &node.rest));
            if flags & CHANGED != 0
                && let Some(old) = base_of[index]
            {
                let old = &base.nodes[old as usize].rest;
                lines.push(|| format!("  {}    was:{old}", "  ".repeat(depth)));
            }
            true
        });
        let removed = seen.iter().filter(|seen| !**seen).count();
        let mut top_removed = 0;
        for (index, old) in base.nodes.iter().enumerate() {
            if !seen[index] && old.parent.is_none_or(|parent| seen[parent as usize]) {
                top_removed += 1;
                lines.push(|| {
                    format!(
                        "- uid={}{}{} [removed; not actionable]",
                        self.prefix, old.id, old.rest
                    )
                });
            }
        }
        if removed > top_removed {
            lines.push(|| format!("  (+{} removed descendants)", removed - top_removed));
        }
        let count_of = |flag: u8| mark.iter().filter(|&&flags| flags & flag != 0).count();
        let (added, changed, moved) = (count_of(ADDED), count_of(CHANGED), count_of(MOVED));
        if added + changed + moved + removed == 0 {
            return "No changes since the previous snapshot of this tab.".to_owned();
        }
        format!(
            "Changes since the previous snapshot of this tab: {added} added (+), {changed} changed (~), {moved} moved (>), {removed} removed (-, not actionable). Unmarked lines are context."
        )
    }

    fn line(&self, marker: &str, depth: usize, id: &str, rest: &str) -> String {
        format!(
            "{marker}{}uid={}{id}{rest}",
            "  ".repeat(depth),
            self.prefix
        )
    }

    fn retain(&mut self, tab: &str, verbose: bool, tree: Tree) {
        self.invalidate_tab(tab);
        let bytes = tree.bytes();
        if tree.duplicate {
            warn!(
                nodes = tree.nodes.len(),
                "page snapshot repeats uids; diff baseline not retained"
            );
            return;
        }
        if bytes > MAX_BASELINE_BYTES {
            warn!(
                bytes,
                limit = MAX_BASELINE_BYTES,
                "page snapshot exceeds the diff baseline budget; baseline not retained"
            );
            return;
        }
        while self.bytes + bytes > MAX_BASELINE_BYTES {
            let oldest = self
                .baselines
                .iter()
                .min_by_key(|(_, base)| base.used)
                .map(|(tab, _)| tab.clone())
                .expect("baseline bytes are accounted only for retained baselines");
            self.invalidate_tab(&oldest);
        }
        let mut pos = vec![0u32; tree.nodes.len()];
        for node in &tree.nodes {
            for (order, &child) in node.children.iter().enumerate() {
                pos[child] = order as u32;
            }
        }
        let mut index = HashMap::with_capacity(tree.nodes.len());
        let nodes = tree
            .nodes
            .into_iter()
            .enumerate()
            .map(|(order, node)| {
                index.insert(node.id.clone(), order as u32);
                BaseNode {
                    id: node.id,
                    rest: node.rest,
                    content_hash: node.content_hash,
                    parent: node.parent.map(|parent| parent as u32),
                    pos: pos[order],
                }
            })
            .collect();
        self.clock += 1;
        self.bytes += bytes;
        self.baselines.insert(
            tab.to_owned(),
            Baseline {
                verbose,
                index,
                nodes,
                bytes,
                used: self.clock,
            },
        );
    }
}

impl Tree {
    fn parse(root: &Value, find: Option<&str>) -> Result<Self> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut ids = HashSet::new();
        let mut duplicate = false;
        let mut stack = vec![(root, None::<usize>, 0usize)];
        while let Some((value, parent, depth)) = stack.pop() {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("snapshot node is not an object"))?;
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace))
                .ok_or_else(|| anyhow!("snapshot node has no valid uid"))?;
            for key in ["role", "name"] {
                if object.get(key).is_some_and(|value| !value.is_string()) {
                    bail!("snapshot node {key} is not a string");
                }
            }
            let children = match object.get("children") {
                None => &[][..],
                Some(Value::Array(children)) => children.as_slice(),
                Some(_) => bail!("snapshot node children is not an array"),
            };
            duplicate |= !ids.insert(id);
            let index = nodes.len();
            if let Some(parent) = parent {
                nodes[parent].children.push(index);
            }
            let role = object.get("role").and_then(Value::as_str).unwrap_or("");
            // Search and compare full fields; truncate only the displayed copy.
            let source = describe(object, false);
            nodes.push(Node {
                id: id.to_owned(),
                rest: describe(object, true),
                content_hash: xxhash_rust::xxh3::xxh3_128(source.as_bytes()),
                matches_find: find.is_some_and(|needle| source.to_lowercase().contains(needle)),
                source_lines: 1 + source.bytes().filter(|&byte| byte == b'\n').count(),
                parent,
                children: Vec::new(),
                depth,
                end: index + 1,
                interactive: INTERACTIVE_ROLES.contains(&role)
                    || object.get("focusable") == Some(&Value::Bool(true)),
            });
            stack.extend(
                children
                    .iter()
                    .rev()
                    .map(|child| (child, Some(index), depth + 1)),
            );
        }
        for index in (1..nodes.len()).rev() {
            if let Some(parent) = nodes[index].parent {
                nodes[parent].end = nodes[parent].end.max(nodes[index].end);
            }
        }
        Ok(Self { nodes, duplicate })
    }

    fn bytes(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| node.id.len() * 2 + node.rest.len() + std::mem::size_of::<BaseNode>())
            .sum()
    }
}

impl Lines {
    fn push(&mut self, make: impl FnOnce() -> String) {
        if self.lines.len() < self.limit {
            self.lines.push(make());
        } else {
            self.omitted += 1;
        }
    }
}

/// Preorder walk; children of an excluded node keep the excluded node's depth.
fn walk(tree: &Tree, start: usize, mut visit: impl FnMut(usize, usize) -> bool) {
    let mut stack = vec![(start, 0usize)];
    while let Some((index, depth)) = stack.pop() {
        let child_depth = if visit(index, depth) {
            depth + 1
        } else {
            depth
        };
        stack.extend(
            tree.nodes[index]
                .children
                .iter()
                .rev()
                .map(|&child| (child, child_depth)),
        );
    }
}

fn describe(object: &Map<String, Value>, bounded: bool) -> String {
    let mut rest = String::new();
    if let Some(role) = object
        .get("role")
        .and_then(Value::as_str)
        .filter(|role| !role.is_empty())
    {
        rest.push(' ');
        rest.push_str(if role == "none" { "ignored" } else { role });
    }
    if let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        rest.push_str(" \"");
        push_field(&mut rest, name, bounded);
        rest.push('"');
    }
    let mut keys: Vec<&String> = object
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "id" | "role"
                    | "name"
                    | "children"
                    | "backendNodeId"
                    | "loaderId"
                    | "elementHandle"
            )
        })
        .collect();
    keys.sort();
    for key in keys {
        match &object[key] {
            Value::Bool(true) => {
                rest.push(' ');
                rest.push_str(key);
            }
            Value::String(value) => {
                rest.push_str(&format!(" {key}=\""));
                push_field(&mut rest, value, bounded);
                rest.push('"');
            }
            Value::Number(value) => rest.push_str(&format!(" {key}=\"{value}\"")),
            _ => {}
        }
    }
    rest
}

fn push_field(out: &mut String, value: &str, bounded: bool) {
    if !bounded {
        out.push_str(value);
        return;
    }
    for (index, character) in value.chars().enumerate() {
        if index == MAX_FIELD_CHARS {
            out.push('…');
            break;
        }
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            character => out.push(character),
        }
    }
}

/// Byte range of the upstream snapshot section, anchored on the structured
/// root uid and verified line by line against the structured tree.
fn locate(text: &str, tree: &Tree) -> Option<Range<usize>> {
    let mut from = 0;
    while let Some(offset) = text[from..].find(MARKER) {
        let start = from + offset;
        from = start + MARKER.len();
        if start > 0 && !text[..start].ends_with('\n') {
            continue;
        }
        let body = &text[from..];
        if !(body.starts_with(SELECTED_NOTE) || starts_with_uid(body, &tree.nodes[0].id)) {
            continue;
        }
        let mut at = from - 1;
        if body.starts_with(SELECTED_NOTE) {
            at = find_node_line(text, at, &tree.nodes[0])?;
        }
        for node in &tree.nodes {
            if find_node_line(text, at, node)? != at {
                return None;
            }
            // A name or attribute can contain newlines, fake UIDs, and section
            // headers. Consume all its lines before checking the next node.
            for _ in 0..node.source_lines {
                at += 1 + text[at + 1..].find('\n')?;
            }
        }
        return Some(start..at + 1);
    }
    None
}

fn find_node_line(text: &str, from: usize, node: &Node) -> Option<usize> {
    let needle = format!("\n{}uid={}", "  ".repeat(node.depth), node.id);
    let mut at = from;
    loop {
        let found = at + text[at..].find(&needle)?;
        if matches!(
            text.as_bytes().get(found + needle.len()),
            None | Some(b' ' | b'\n')
        ) {
            return Some(found);
        }
        at = found + 1;
    }
}

fn starts_with_uid(body: &str, id: &str) -> bool {
    body.strip_prefix("uid=")
        .and_then(|rest| rest.strip_prefix(id))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\n']))
}

fn has_snapshot_marker(text: &str) -> bool {
    text.match_indices(MARKER).any(|(start, _)| {
        let body = &text[start + MARKER.len()..];
        (start == 0 || text[..start].ends_with('\n'))
            && (body.starts_with("uid=") || body.starts_with(SELECTED_NOTE))
    })
}

fn looks_like_namespace(namespace: &str) -> bool {
    namespace.len() == 1 + NONCE_LEN as usize
        && namespace.starts_with('v')
        && namespace[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
}

fn take_bool(args: &mut Map<String, Value>, key: &str) -> Result<bool> {
    match args.remove(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(value),
        Some(_) => bail!("`{key}` must be a boolean"),
    }
}

fn longest_increasing<T>(values: &[T], key: impl Fn(&T) -> u32) -> Vec<bool> {
    let mut keep = vec![false; values.len()];
    if values.is_sorted_by_key(&key) {
        keep.fill(true);
        return keep;
    }
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; values.len()];
    for (index, value) in values.iter().enumerate() {
        let value = key(value);
        let slot = tails.partition_point(|&tail| key(&values[tail]) < value);
        previous[index] = slot.checked_sub(1).map(|slot| tails[slot]);
        if slot == tails.len() {
            tails.push(index);
        } else {
            tails[slot] = index;
        }
    }
    let mut current = tails.last().copied();
    while let Some(index) = current {
        keep[index] = true;
        current = previous[index];
    }
    keep
}

/// Thirteen base-36 characters from wall-clock nanoseconds, pid and a process
/// counter, never repeating the previous nonce of this process.
fn new_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos() as u64,
        Err(_) => {
            warn!(
                "system clock is before the Unix epoch; snapshot uid namespace relies on pid and counter only"
            );
            0
        }
    };
    loop {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut seed = [0u8; 24];
        seed[..8].copy_from_slice(&nanos.to_le_bytes());
        seed[8..16].copy_from_slice(&u64::from(std::process::id()).to_le_bytes());
        seed[16..].copy_from_slice(&count.to_le_bytes());
        let mut value = xxhash_rust::xxh3::xxh3_64(&seed);
        if LAST.swap(value, Ordering::Relaxed) == value {
            continue;
        }
        let mut nonce = Vec::with_capacity(NONCE_LEN as usize);
        for _ in 0..NONCE_LEN {
            nonce.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize]);
            value /= 36;
        }
        return String::from_utf8(nonce).expect("base-36 digits are ASCII");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upstream_lines(node: &Value, depth: usize, out: &mut String) {
        out.push_str(&format!(
            "{}uid={}",
            "  ".repeat(depth),
            node["id"].as_str().unwrap()
        ));
        if let Some(role) = node.get("role").and_then(Value::as_str) {
            out.push_str(&format!(" {role}"));
        }
        if let Some(name) = node.get("name").and_then(Value::as_str) {
            out.push_str(&format!(" \"{name}\""));
        }
        for (key, value) in node.as_object().unwrap() {
            if matches!(key.as_str(), "id" | "role" | "name" | "children") {
                continue;
            }
            match value {
                Value::Bool(true) => out.push_str(&format!(" {key}")),
                Value::String(value) => out.push_str(&format!(" {key}=\"{value}\"")),
                Value::Number(value) => out.push_str(&format!(" {key}=\"{value}\"")),
                _ => {}
            }
        }
        out.push('\n');
        for child in node
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            upstream_lines(child, depth + 1, out);
        }
    }

    /// Mirrors upstream McpResponse: message, snapshot section, later sections, image.
    fn response(snapshot: Value) -> CallToolResult {
        let mut text = String::from("Clicked.\n## Latest page snapshot\n");
        upstream_lines(&snapshot, 0, &mut text);
        text.push_str("\n## Network requests\nreqid=1 GET https://example.test");
        let mut result = CallToolResult::success(vec![
            ContentBlock::text(text),
            ContentBlock::image("aW1n", "image/png"),
        ]);
        result.structured_content = Some(json!({"snapshot": snapshot, "message": "Clicked."}));
        result
    }

    fn node(id: &str, role: &str, name: &str, children: Vec<Value>) -> Value {
        let mut node = json!({"id": id, "role": role, "name": name});
        if !children.is_empty() {
            node["children"] = Value::Array(children);
        }
        node
    }

    fn page() -> Value {
        node(
            "1_0",
            "RootWebArea",
            "Shop",
            vec![
                node(
                    "1_1",
                    "navigation",
                    "Main",
                    vec![
                        node("1_2", "link", "Home", vec![]),
                        node("1_3", "link", "Cart", vec![]),
                    ],
                ),
                node(
                    "1_4",
                    "main",
                    "",
                    vec![
                        node("1_5", "heading", "Products", vec![]),
                        node("1_6", "StaticText", "Red shoes", vec![]),
                        node("1_7", "button", "Buy shoes", vec![]),
                    ],
                ),
            ],
        )
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn text(result: &CallToolResult) -> &str {
        result.content[0].as_text().unwrap().text.as_str()
    }

    fn run(
        state: &mut ViewState,
        action: &str,
        arguments: Value,
        snapshot: Value,
    ) -> Result<String> {
        let request = state.prepare(action, &mut args(arguments))?;
        let mut result = response(snapshot);
        state.finish(&request, &mut result, "tab")?;
        Ok(text(&result).to_owned())
    }

    #[test]
    fn namespaces_differ_between_workers() {
        let (first, second) = (ViewState::default(), ViewState::default());
        assert_ne!(first.prefix, second.prefix);
        assert!(looks_like_namespace(first.prefix.trim_end_matches(':')));
    }

    #[test]
    fn rewrites_only_explicit_uid_fields() {
        let state = ViewState::default();
        let p = &state.prefix;
        let mut click = args(json!({"uid": format!("{p}1_7"), "pageId": 1}));
        state.prepare("click", &mut click).unwrap();
        assert_eq!(click["uid"], "1_7");
        let mut drag = args(json!({"from_uid": format!("{p}1_2"), "to_uid": format!("{p}1_3")}));
        state.prepare("drag", &mut drag).unwrap();
        assert_eq!(
            (drag["from_uid"].as_str(), drag["to_uid"].as_str()),
            (Some("1_2"), Some("1_3"))
        );
        let mut form =
            args(json!({"elements": [{"uid": format!("{p}1_5"), "value": format!("{p}1_9")}]}));
        state.prepare("fill_form", &mut form).unwrap();
        assert_eq!(
            form["elements"][0],
            json!({"uid": "1_5", "value": format!("{p}1_9")})
        );
        let script = format!("(el) => '{p}1_2' + el.id");
        let mut evaluate = args(json!({"function": script, "args": [format!("{p}1_2")]}));
        state.prepare("evaluate_script", &mut evaluate).unwrap();
        assert_eq!(evaluate["args"], json!(["1_2"]));
        assert_eq!(evaluate["function"], script);
        let mut fill = args(json!({"uid": format!("{p}1_5"), "value": format!("{p}1_6")}));
        state.prepare("fill", &mut fill).unwrap();
        assert_eq!(fill["value"], format!("{p}1_6"));
    }

    #[test]
    fn rejects_stale_and_unprefixed_uids_including_nested() {
        let state = ViewState::default();
        let old = ViewState::default();
        let stale = format!("{}1_2", old.prefix);
        let error = state
            .prepare("click", &mut args(json!({"uid": stale})))
            .unwrap_err();
        assert!(error.to_string().contains("earlier browser worker"));
        let error = state
            .prepare(
                "fill_form",
                &mut args(json!({"elements": [{"uid": stale, "value": "x"}]})),
            )
            .unwrap_err();
        assert!(error.to_string().contains("elements[].uid"));
        let error = state
            .prepare(
                "evaluate_script",
                &mut args(json!({"function": "x", "args": [stale]})),
            )
            .unwrap_err();
        assert!(error.to_string().contains("earlier browser worker"));
        let error = state
            .prepare("hover", &mut args(json!({"uid": "1_2"})))
            .unwrap_err();
        assert!(error.to_string().contains("copy uids exactly"));
        let error = state
            .prepare("take_snapshot", &mut args(json!({"within": stale})))
            .unwrap_err();
        assert!(error.to_string().contains("earlier browser worker"));
    }

    #[test]
    fn validates_view_options() {
        let state = ViewState::default();
        let p = &state.prefix;
        let conflict = |value: Value| {
            state
                .prepare("take_snapshot", &mut args(value))
                .unwrap_err()
                .to_string()
        };
        assert!(conflict(json!({"diff": true, "find": "x"})).contains("cannot be combined"));
        assert!(
            conflict(json!({"diff": true, "interactive": true})).contains("cannot be combined")
        );
        assert!(
            conflict(json!({"diff": true, "within": format!("{p}1_1")}))
                .contains("cannot be combined")
        );
        assert!(conflict(json!({"find": "x", "filePath": "s.txt"})).contains("filePath"));
        assert!(conflict(json!({"maxLines": 0})).contains("maxLines"));
        assert!(conflict(json!({"maxLines": 2001})).contains("maxLines"));
        assert!(
            state
                .prepare("click", &mut args(json!({"diff": true})))
                .is_err()
        );
        let mut options = args(
            json!({"find": "Shoes", "within": format!("{p}1_4"), "interactive": true, "maxLines": 2000, "verbose": true}),
        );
        let request = state.prepare("take_snapshot", &mut options).unwrap();
        assert_eq!(options, args(json!({"verbose": true})));
        assert_eq!(
            (request.find.as_deref(), request.within.as_deref()),
            (Some("shoes"), Some("1_4"))
        );
        assert_eq!(request.max_lines, 2000);
        let mut saved = args(json!({"filePath": "s.txt"}));
        assert_eq!(
            state.prepare("take_snapshot", &mut saved).unwrap().source,
            Source::SavedToFile
        );
    }

    #[test]
    fn full_view_namespaces_and_preserves_other_sections() {
        let mut state = ViewState::default();
        let request = state.prepare("take_snapshot", &mut Map::new()).unwrap();
        let mut result = response(page());
        let structured = result.structured_content.clone();
        state.finish(&request, &mut result, "tab").unwrap();
        let p = &state.prefix;
        let out = text(&result);
        assert!(out.starts_with("Clicked.\n## Latest page snapshot\n"));
        assert!(out.contains(&format!("uid={p}1_0 RootWebArea \"Shop\"\n")));
        assert!(out.contains(&format!(
            "\n    uid={p}1_7 button \"Buy shoes\"\n\n## Network requests\nreqid=1"
        )));
        assert!(!out.contains("\nuid=1_"));
        assert_eq!(result.content.len(), 2);
        assert!(matches!(result.content[1], ContentBlock::Image(_)));
        assert_eq!(result.structured_content, structured);
    }

    #[test]
    fn multiline_fields_cannot_end_or_spoof_the_snapshot_section() {
        let mut state = ViewState::default();
        let mut child = node("1_1", "button", "Buy\n\n## injected\nuid=9_9 raw", vec![]);
        child["value"] = json!("Value\n\n## attribute\nuid=8_8 raw");
        let snapshot = node(
            "1_0",
            "RootWebArea",
            "Shop\n  uid=1_1 button \"fake\"",
            vec![child],
        );
        let out = run(&mut state, "take_snapshot", json!({}), snapshot).unwrap();
        assert!(out.contains("Buy\\n\\n## injected\\nuid=9_9 raw"));
        assert!(!out.contains("\n## injected"));
        assert!(!out.contains("\nuid=9_9"));
        assert!(!out.contains("\n## attribute"));
        assert!(!out.contains("\nuid=8_8"));
        assert!(out.contains("\n\n## Network requests\nreqid=1"));
        assert_eq!(
            out.lines()
                .filter(|line| line.contains("uid=") && !line.contains("RootWebArea"))
                .count(),
            1
        );
    }

    #[test]
    fn search_and_diff_include_suffixes_beyond_the_display_limit() {
        for field in ["name", "value"] {
            let mut state = ViewState::default();
            let mut child = node("1_1", "textbox", "Input", vec![]);
            child[field] = json!(format!("{}TARGET", "A".repeat(MAX_FIELD_CHARS + 1)));
            let mut snapshot = node("1_0", "RootWebArea", "Shop", vec![child]);
            let out = run(
                &mut state,
                "take_snapshot",
                json!({"find":"target"}),
                snapshot.clone(),
            )
            .unwrap();
            assert!(out.contains("1 find matches"), "{field}: {out}");
            assert!(out.contains(&format!("uid={}1_1", state.prefix)));
            assert!(!out.contains("TARGET"), "display should stay bounded");
            snapshot["children"][0][field] =
                json!(format!("{}CHANGED", "A".repeat(MAX_FIELD_CHARS + 1)));
            let out = run(&mut state, "take_snapshot", json!({"diff":true}), snapshot).unwrap();
            assert!(out.contains("1 changed (~)"), "{field}: {out}");
            assert!(!out.contains("CHANGED"), "display should stay bounded");
        }
    }

    #[test]
    fn find_shows_ancestors_case_insensitively() {
        let mut state = ViewState::default();
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"find": "SHOES"}),
            page(),
        )
        .unwrap();
        let p = &state.prefix;
        assert!(out.contains("2 find matches"));
        assert!(out.contains(&format!("uid={p}1_0 RootWebArea")));
        assert!(out.contains(&format!(
            "  uid={p}1_4 main\n    uid={p}1_6 StaticText \"Red shoes\"\n    uid={p}1_7 button"
        )));
        assert!(!out.contains("Home") && !out.contains("Products"));
    }

    #[test]
    fn within_and_interactive_filters() {
        let mut state = ViewState::default();
        let within = format!("{}1_1", state.prefix);
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"within": within}),
            page(),
        )
        .unwrap();
        assert!(
            out.contains("Home")
                && out.contains("Cart")
                && !out.contains("Shop")
                && !out.contains("Buy")
        );
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"interactive": true}),
            page(),
        )
        .unwrap();
        assert!(out.contains("Home") && out.contains("Products") && out.contains("Buy shoes"));
        assert!(!out.contains("Red shoes") && !out.contains("navigation"));
        let missing = format!("{}9_9", state.prefix);
        assert!(
            run(
                &mut state,
                "take_snapshot",
                json!({"within": missing}),
                page()
            )
            .is_err()
        );
    }

    #[test]
    fn find_matches_are_bounded() {
        let mut state = ViewState::default();
        let items = (1..=60)
            .map(|i| node(&format!("1_{i}"), "button", &format!("item {i}"), vec![]))
            .collect();
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"find": "item"}),
            node("1_0", "RootWebArea", "", items),
        )
        .unwrap();
        assert!(out.contains("first 50 of 60 find matches"));
        assert!(out.contains("\"item 50\"") && !out.contains("\"item 51\""));
    }

    #[test]
    fn max_lines_truncates_with_notice() {
        let mut state = ViewState::default();
        let out = run(&mut state, "take_snapshot", json!({"maxLines": 3}), page()).unwrap();
        assert!(out.contains("[5 more lines omitted"));
        assert!(!out.contains("Cart"));
    }

    #[test]
    fn diff_reports_added_changed_moved_reordered_and_removed() {
        let mut state = ViewState::default();
        run(&mut state, "take_snapshot", json!({}), page()).unwrap();
        let next = node(
            "1_0",
            "RootWebArea",
            "Shop",
            vec![
                node(
                    "1_1",
                    "navigation",
                    "Main",
                    vec![
                        node("1_3", "link", "Cart", vec![]),
                        node("1_2", "link", "Home", vec![]),
                        node("1_7", "button", "Buy shoes", vec![]),
                    ],
                ),
                node(
                    "1_4",
                    "main",
                    "",
                    vec![
                        node("1_5", "heading", "Products (2)", vec![]),
                        node("2_1", "button", "Checkout", vec![]),
                    ],
                ),
            ],
        );
        let out = run(&mut state, "take_snapshot", json!({"diff": true}), next).unwrap();
        let p = &state.prefix;
        assert!(out.contains("1 added (+), 1 changed (~), 2 moved (>), 1 removed"));
        assert!(out.contains(&format!("+     uid={p}2_1 button \"Checkout\"")));
        assert!(out.contains(&format!("~     uid={p}1_5 heading \"Products (2)\"")));
        assert!(out.contains("was: heading \"Products\""));
        assert!(out.contains(&format!(">     uid={p}1_7 button")));
        assert!(
            out.contains(&format!(">     uid={p}1_3 link \"Cart\""))
                || out.contains(&format!(">     uid={p}1_2 link \"Home\""))
        );
        assert!(out.contains(&format!(
            "- uid={p}1_6 StaticText \"Red shoes\" [removed; not actionable]"
        )));
        let uid = format!("{p}1_7");
        let out = run(&mut state, "click", json!({"uid": uid}), page()).unwrap();
        assert!(out.contains("Changes since the previous snapshot"));
        let same = run(&mut state, "wait_for", json!({"text": ["x"]}), page()).unwrap();
        assert!(same.contains("No changes since the previous snapshot"));
    }

    #[test]
    fn removed_subtrees_are_reported_once() {
        let mut state = ViewState::default();
        run(&mut state, "take_snapshot", json!({}), page()).unwrap();
        let next = node(
            "1_0",
            "RootWebArea",
            "Shop",
            vec![node("1_4", "main", "", vec![])],
        );
        let out = run(&mut state, "take_snapshot", json!({"diff": true}), next).unwrap();
        assert!(out.contains("1_1 navigation \"Main\" [removed; not actionable]"));
        assert!(out.contains("(+2 removed descendants)"));
        assert!(!out.contains("1_2 link"));
    }

    #[test]
    fn baseline_fallbacks_and_invalidation() {
        let mut state = ViewState::default();
        let out = run(&mut state, "click", json!({}), page()).unwrap();
        assert!(out.contains("Full snapshot (no compatible previous snapshot"));
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"diff": true, "verbose": true}),
            page(),
        )
        .unwrap();
        assert!(out.contains("Diff unavailable"));
        assert!(out.contains("Buy shoes"));
        state.invalidate_tab("tab");
        let out = run(&mut state, "take_snapshot", json!({"diff": true}), page()).unwrap();
        assert!(out.contains("Diff unavailable"));
        let navigated = node(
            "5_0",
            "RootWebArea",
            "Other",
            vec![node("1_2", "link", "Home", vec![])],
        );
        let out = run(
            &mut state,
            "take_snapshot",
            json!({"diff": true}),
            navigated,
        )
        .unwrap();
        assert!(out.contains("Diff unavailable"));
        let out = run(&mut state, "take_snapshot", json!({"diff": true}), page()).unwrap();
        assert!(out.contains("Diff unavailable"));
        let out = run(&mut state, "take_snapshot", json!({"diff": true}), page()).unwrap();
        assert!(out.contains("No changes"));
    }

    #[test]
    fn oversized_snapshot_is_shown_but_not_retained() {
        let mut state = ViewState::default();
        let long = "x".repeat(MAX_FIELD_CHARS);
        let items = (1..8000)
            .map(|i| node(&format!("1_{i}"), "button", &long, vec![]))
            .collect();
        let big = node("1_0", "RootWebArea", "", items);
        let out = run(&mut state, "take_snapshot", json!({}), big).unwrap();
        assert!(out.contains("too large to keep for diffs"));
        assert!(out.contains("more lines omitted"));
        assert!(state.baselines.is_empty() && state.bytes == 0);
    }

    #[test]
    fn baselines_share_a_bounded_budget() {
        let mut state = ViewState::default();
        let long = "y".repeat(MAX_FIELD_CHARS);
        let items: Vec<Value> = (1..3500)
            .map(|i| node(&format!("1_{i}"), "button", &long, vec![]))
            .collect();
        for tab in ["a", "b", "c"] {
            let request = state.prepare("take_snapshot", &mut Map::new()).unwrap();
            let mut result = response(node("1_0", "RootWebArea", "", items.clone()));
            state.finish(&request, &mut result, tab).unwrap();
        }
        assert!(state.bytes <= MAX_BASELINE_BYTES);
        assert!(!state.baselines.contains_key("a") && state.baselines.contains_key("c"));
        assert_eq!(
            state.bytes,
            state
                .baselines
                .values()
                .map(|base| base.bytes)
                .sum::<usize>()
        );
    }

    #[test]
    fn invalid_or_missing_structure_is_an_error() {
        let mut state = ViewState::default();
        let request = state.prepare("click", &mut Map::new()).unwrap();
        let mut result = response(page());
        result.structured_content = Some(json!({"snapshot": {"role": "RootWebArea"}}));
        assert!(
            state
                .finish(&request, &mut result, "tab")
                .unwrap_err()
                .to_string()
                .contains("invalid structured")
        );
        let mut result = response(page());
        result.structured_content = None;
        assert!(state.finish(&request, &mut result, "tab").is_err());
        let mut result = response(page());
        result.structured_content = Some(
            json!({"snapshot": node("1_0", "RootWebArea", "", vec![node("1_99", "link", "", vec![])])}),
        );
        assert!(
            state
                .finish(&request, &mut result, "tab")
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        let explicit = state.prepare("take_snapshot", &mut Map::new()).unwrap();
        let mut plain = CallToolResult::success(vec![ContentBlock::text("ok")]);
        assert!(state.finish(&explicit, &mut plain, "tab").is_err());
        let mut plain = CallToolResult::success(vec![ContentBlock::text(
            "## Latest page snapshot\nnot a tree",
        )]);
        state.finish(&request, &mut plain, "tab").unwrap();
    }

    #[test]
    fn lis_marks_minimal_moves() {
        assert_eq!(
            longest_increasing(&[2, 0, 1], |&n| n),
            vec![false, true, true]
        );
        assert_eq!(longest_increasing(&[0, 1, 2], |&n| n), vec![true; 3]);
        assert_eq!(
            longest_increasing(&[3, 1, 2, 0], |&n| n)
                .iter()
                .filter(|keep| **keep)
                .count(),
            2
        );
    }
}
