use super::*;
use futures_util::future::Either;
use std::os::unix::fs::OpenOptionsExt;

/// Restore a scoped selection even when the caller drops an in-flight future.
struct PageScope<'a> {
    session: &'a mut Session,
    previous: Option<TargetId>,
}

impl<'a> PageScope<'a> {
    fn new(session: &'a mut Session, selected: Option<TargetId>, scoped: bool) -> Self {
        let previous = scoped
            .then(|| session.persisted.current_tab.clone())
            .flatten();
        if let Some(tab) = selected {
            session.persisted.current_tab = Some(tab);
        }
        Self { session, previous }
    }
}

impl Drop for PageScope<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take()
            && self
                .session
                .pages
                .local_to_tab
                .values()
                .any(|tab| tab == &previous)
        {
            self.session.persisted.current_tab = Some(previous);
        }
    }
}

impl Session {
    pub(super) fn update_handles(&mut self) -> Result<()> {
        let live: HashSet<_> = self.pages.local_to_tab.values().cloned().collect();
        self.persisted
            .controls
            .handles
            .retain(|_, tab| live.contains(tab));
        let mut tabs: Vec<_> = live.into_iter().collect();
        tabs.sort();
        for tab in tabs {
            if self
                .persisted
                .controls
                .handles
                .values()
                .any(|known| known == &tab)
            {
                continue;
            }
            self.persisted.controls.next_handle = self
                .persisted
                .controls
                .next_handle
                .checked_add(1)
                .context("page handle counter exhausted")?;
            self.persisted
                .controls
                .handles
                .insert(format!("p{}", self.persisted.controls.next_handle), tab);
        }
        Ok(())
    }

    fn handle(&self, tab: &str) -> Result<String> {
        self.persisted
            .controls
            .handles
            .iter()
            .find_map(|(handle, target)| (target == tab).then(|| handle.clone()))
            .context("owned page has no public handle")
    }

    fn resolve_page(&self, handle: Option<&str>) -> Result<String> {
        let tab = match handle {
            Some(handle) => self
                .persisted
                .controls
                .handles
                .get(handle)
                .cloned()
                .context("unknown or closed page handle; use list_pages")?,
            None => self
                .persisted
                .current_tab
                .clone()
                .context("no current page; create one first")?,
        };
        if !self.pages.local_to_tab.values().any(|live| live == &tab) {
            bail!("page is closed or unavailable; use list_pages");
        }
        Ok(tab)
    }
}

impl BrowserProxy {
    pub(super) async fn status(&self, workspace: &Path, id: &str) -> Result<CallToolResult> {
        let slot = self.sessions.lock().await.get(id).cloned();
        let mut lines = vec![
            format!("browser-mcp {}", env!("CARGO_PKG_VERSION")),
            format!(
                "mode: {}; layout: {:?}; CDP: {}",
                if self.config.headless {
                    "headless"
                } else {
                    "headed"
                },
                self.config.session_layout,
                self.config.browser_url()
            ),
        ];
        let tracker = self.tracker.read().await;
        lines.push(format!(
            "tracker: {}",
            if let Some(tracker) = tracker.as_ref() {
                if tracker.is_available().await {
                    "connected"
                } else {
                    "disconnected"
                }
            } else {
                "not started"
            }
        ));
        drop(tracker);
        if let Some(slot) = slot {
            let guard = slot.session.lock().await;
            if let Some(session) = guard.as_ref() {
                if session.persisted.workspace != workspace {
                    bail!("session belongs to a different workspace");
                }
                lines.push(format!(
                    "worker: {}; cleanup: {}; idle limit: {} minutes",
                    if session
                        .downstream
                        .as_ref()
                        .is_some_and(|worker| !worker.is_closed())
                    {
                        "connected"
                    } else {
                        "not running"
                    },
                    session.persisted.cleanup_pending,
                    self.config.idle_minutes
                ));
                for (handle, tab) in &session.persisted.controls.handles {
                    lines.push(format!(
                        "{handle}{}",
                        if session.persisted.current_tab.as_ref() == Some(tab) {
                            " [current]"
                        } else {
                            ""
                        }
                    ));
                }
                if let Some(handoff) = &session.persisted.controls.handoff {
                    lines.push(format!(
                        "handoff: user controls {}; finish only after user confirmation",
                        session
                            .handle(&handoff.tab)
                            .unwrap_or_else(|_| "closed page".into())
                    ));
                }
                if let Some(path) = &session.recording {
                    lines.push(format!("recording: {}", path.display()));
                }
                if session.recording_lost {
                    lines.push(
                        "previous recording interrupted by worker loss; file may be incomplete"
                            .into(),
                    );
                }
            } else {
                lines.push("session: not started".into());
            }
        } else {
            lines.push("session: not started".into());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            lines.join("\n"),
        )]))
    }

    pub(super) async fn call_session(
        &self,
        catalog: &ToolCatalog,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        if request.name != "batch" {
            return self.call_one(catalog, tracker, id, session, request).await;
        }
        let steps = actions::batch_steps(
            request.arguments.as_ref().unwrap_or(&JsonObject::new()),
            &catalog.tools,
        )?;
        self.reconcile(tracker, id, session).await?;
        // Validate every ref and explicit page before any effect.
        let validation = (|| -> Result<()> {
            for (action, arguments) in &steps {
                let mut arguments = arguments.clone();
                session.views.prepare(action, &mut arguments)?;
                if let Some(page) = arguments.get("page").and_then(Value::as_str) {
                    session.resolve_page(Some(page))?;
                }
            }
            Ok(())
        })();
        if let Err(error) = validation {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Batch rejected before any step: {error}"
            ))]));
        }
        let total = steps.len();
        let mut summary = Vec::new();
        for (index, (action, arguments)) in steps.into_iter().enumerate() {
            let response = self
                .call_one(
                    catalog,
                    tracker,
                    id,
                    session,
                    CallToolRequestParams::new(action.clone()).with_arguments(arguments),
                )
                .await;
            let mut result = match response {
                Ok(result) => result,
                Err(error) => return Err(error.context(format!("batch stopped at step {} ({action}); {} completed, {} not run; failed step may have taken effect", index + 1, index, total - index - 1))),
            };
            let failed = result.is_error.unwrap_or(false);
            if failed || index + 1 == total {
                summary.push(format!(
                    "Batch: {} completed; {} not run{}",
                    if failed { index } else { index + 1 },
                    total - index - 1,
                    if failed {
                        "; failed step may have taken effect"
                    } else {
                        ""
                    }
                ));
                result
                    .content
                    .insert(0, ContentBlock::text(summary.join("\n")));
                return Ok(result);
            }
            summary.push(format!("{} {action}: completed", index + 1));
        }
        bail!("validated nonempty batch produced no result")
    }

    async fn call_one(
        &self,
        catalog: &ToolCatalog,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
        mut request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        let action = request.name.to_string();
        let _rendering = if self.config.headless {
            Some(if action == "take_screenshot" {
                Either::Left(self.headless_rendering.write().await)
            } else {
                Either::Right(self.headless_rendering.read().await)
            })
        } else {
            None
        };
        let previous_handles = session
            .persisted
            .controls
            .handles
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let arguments = request.arguments.get_or_insert_default();
        let view = match session.views.prepare(&action, arguments) {
            Ok(view) => view,
            Err(error) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    error.to_string(),
                )]));
            }
        };
        let page = arguments
            .remove("page")
            .and_then(|value| value.as_str().map(str::to_owned));
        self.reconcile(tracker, id, session).await?;
        let finishing_handoff =
            action == "handoff" && arguments.get("mode").and_then(Value::as_str) == Some("finish");
        let selected = if !finishing_handoff
            && (page.is_some() || matches!(action.as_str(), "select_page" | "handoff"))
        {
            match session.resolve_page(page.as_deref()) {
                Ok(tab) => Some(tab),
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        error.to_string(),
                    )]));
                }
            }
        } else {
            session.persisted.current_tab.clone()
        };
        if action == "select_page" {
            let tab = selected.context("selection has no target")?;
            if arguments.get("bringToFront") == Some(&Value::Bool(true)) {
                tracker.activate_target(&tab).await?;
            }
            session.persisted.current_tab = Some(tab.clone());
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{} [current]",
                session.handle(&tab)?
            ))]));
        }
        if action == "handoff" {
            let mode = arguments
                .get("mode")
                .and_then(Value::as_str)
                .context("handoff mode missing")?;
            if mode == "start" {
                if self.config.headless {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "handoff requires a headed browser",
                    )]));
                }
                if session.persisted.controls.handoff.is_some() {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "handoff already active; finish it after user confirmation",
                    )]));
                }
                let tab = selected.context("handoff has no page")?;
                tracker.activate_target(&tab).await?;
                session.persisted.controls.handoff = Some(persistence::Handoff {
                    tab: tab.clone(),
                    generation: tracker.generation().to_owned(),
                    started: unix_time_seconds()?,
                });
                let message = arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|message| format!(" Task: {message}"))
                    .unwrap_or_default();
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "User controls {}. End this turn and ask them to complete the task. Call handoff finish only after they confirm. Automation on this page is paused for at most 24h.{message}",
                    session.handle(&tab)?
                ))]));
            }
            let Some(handoff) = session.persisted.controls.handoff.take() else {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "no handoff is active",
                )]));
            };
            if handoff.generation != tracker.generation()
                || !tracker.is_live_tab(&handoff.tab).await?
            {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "handoff target closed or browser restarted; inspect list_pages before continuing",
                )]));
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "Handoff finished. Take a fresh snapshot before acting.",
            )]));
        }
        if let Some(handoff) = &session.persisted.controls.handoff {
            if handoff.expired(unix_time_seconds()?) {
                session.persisted.controls.handoff = None;
                warn!(session_id = %id, "human handoff expired after 24 hours");
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "handoff expired after 24h; inspect state before continuing",
                )]));
            }
            if selected.as_ref() == Some(&handoff.tab) && action != "list_pages" {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "page is controlled by the user; finish handoff after their confirmation, or target another owned page",
                )]));
            }
        }
        if action == "screencast_start" {
            if session.recording.is_some() {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "recording already active; stop it first",
                )]));
            }
            if arguments.contains_key("filePath") {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "recording paths are broker-assigned under .scratch/browser-mcp",
                )]));
            }
            let directory = session.persisted.workspace.join(".scratch/browser-mcp");
            std::fs::create_dir_all(&directory)?;
            let directory = directory.canonicalize()?;
            if !directory.starts_with(&session.persisted.workspace) {
                bail!("recording directory escapes workspace");
            }
            let path = directory.join(format!(
                "recording-{}.webm",
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
            ));
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            arguments.insert(
                "filePath".into(),
                path.to_string_lossy().into_owned().into(),
            );
            session.recording = Some(path);
        }
        if action == "screencast_stop" && session.recording.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                if session.recording_lost {
                    "recording lost when worker was replaced"
                } else {
                    "no recording is active"
                },
            )]));
        }
        let secret = action == "fill_secret";
        if secret {
            let function = match self.secrets.function(
                arguments
                    .get("name")
                    .and_then(Value::as_str)
                    .context("secret name missing")?,
            ) {
                Ok(function) => function,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        error.to_string(),
                    )]));
                }
            };
            let uid = arguments.get("uid").context("secret uid missing")?.clone();
            *arguments = json!({"function":function,"args":[uid],"dialogAction":"dismiss"})
                .as_object()
                .unwrap()
                .clone();
            request.name = "evaluate_script".into();
        }
        if matches!(
            action.as_str(),
            "list_network_requests" | "list_console_messages"
        ) {
            arguments.entry("pageSize").or_insert(50.into());
        }
        let epoch = session.worker_epoch;
        let recording_action = matches!(action.as_str(), "screencast_start" | "screencast_stop");
        let (result, target) = {
            let scope = PageScope::new(session, selected, page.is_some());
            if action == "take_screenshot" && self.config.headless {
                // Chromium can wait indefinitely for an inactive tab's frame.
                // Headless activation has no desktop focus effect and does not
                // change the session's public selection (restored by the scope).
                let tab = scope
                    .session
                    .persisted
                    .current_tab
                    .as_deref()
                    .context("screenshot has no current target")?;
                tracker.activate_target(tab).await?;
            }
            let call = self.call_upstream_session(catalog, tracker, id, scope.session, request);
            let result = if recording_action {
                match timeout(Duration::from_secs(30), call).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!(
                        "Recording operation exceeded 30 seconds; recording may be incomplete. The worker will be replaced; inspect state before retrying."
                    )),
                }
            } else {
                call.await
            };
            (result, scope.session.persisted.current_tab.clone())
        };
        let mut result = result?;
        if recording_action
            && result.content.iter().any(|block| {
                block
                    .as_text()
                    .is_some_and(|text| text.text.contains("Error:"))
            })
        {
            result.is_error = Some(true);
        }
        if action == "screencast_stop"
            && !result.is_error.unwrap_or(false)
            && let Some(path) = session.recording.as_ref()
            && std::fs::metadata(path)?.len() == 0
        {
            result = CallToolResult::error(vec![ContentBlock::text(
                "Recording produced an empty artifact; the encoder failed or no frames were captured.",
            )]);
        }
        if action == "screencast_stop"
            || (action == "screencast_start" && result.is_error.unwrap_or(false))
        {
            session.recording = None;
        }
        if secret {
            let text = result
                .content
                .iter()
                .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            let confirmed = text
                .split_once("```json\n")
                .and_then(|(_, rest)| rest.split_once("\n```"))
                .and_then(|(value, _)| serde_json::from_str::<Value>(value).ok())
                .is_some_and(|value| value.as_str() == Some("ok"));
            return Ok(if result.is_error.unwrap_or(false) || !confirmed {
                CallToolResult::error(vec![ContentBlock::text(
                    "Secret fill failed; check the allowed origin, top-level frame, and editable UID using a fresh snapshot.",
                )])
            } else {
                CallToolResult::success(vec![ContentBlock::text(
                    "Secret filled on an allowed origin. Verify the page without reading the value.",
                )])
            });
        }
        if epoch != session.worker_epoch
            && result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("snapshot"))
                .is_some()
        {
            bail!(
                "worker replaced during reconciliation; discard old references and take a fresh snapshot (action was not replayed)"
            );
        }
        if let Some(tab) = target {
            if action == "navigate_page" {
                session.views.invalidate_tab(&tab);
            }
            session.views.finish(&view, &mut result, &tab)?;
        }
        if let Some(pages) = result
            .structured_content
            .as_mut()
            .and_then(|value| value.get_mut("pages"))
            .and_then(Value::as_array_mut)
        {
            for page in pages {
                if let Some(tab) = page
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| session.pages.local_to_tab.get(&id))
                {
                    page["page"] = session.handle(tab)?.into();
                    page["selected"] = (session.persisted.current_tab.as_ref() == Some(tab)).into();
                    if let Some(url) = page.get("url").and_then(Value::as_str).map(str::to_owned) {
                        page["url"] = privacy::url(&url).into();
                    }
                }
            }
        }
        let mut result = filter_page_content(
            result,
            &session.pages.local_to_tab.keys().copied().collect(),
        )?;
        // Sanitize upstream network text before adding broker notices or batch summaries.
        responses::network_result(&mut result);
        if action != "new_page" && !previous_handles.is_empty() {
            let new_handles = session
                .persisted
                .controls
                .handles
                .keys()
                .filter(|handle| !previous_handles.contains(*handle))
                .cloned()
                .collect::<Vec<_>>();
            if !new_handles.is_empty() {
                result.content.push(ContentBlock::text(format!(
                    "New owned page(s): {}. Use list_pages to inspect; current page is unchanged.",
                    new_handles.join(", ")
                )));
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, task::Poll};

    #[tokio::test]
    async fn cancelled_scoped_call_restores_current_without_reviving_closed_tabs() {
        let mut session = Session {
            views: Default::default(),
            recording: None,
            recording_lost: false,
            worker_epoch: 0,
            downstream: None,
            action_pending: true,
            persisted: PersistedSession {
                controls: Default::default(),
                workspace: PathBuf::new(),
                current_tab: Some("background".into()),
                last_activity_unix_seconds: 0,
                cleanup_pending: false,
            },
            pages: PageState {
                local_to_tab: [(1, "shop".into()), (3, "background".into())].into(),
            },
        };
        let mut call = Box::pin(async {
            let scope = PageScope::new(&mut session, Some("shop".into()), true);
            assert_eq!(scope.session.persisted.current_tab.as_deref(), Some("shop"));
            std::future::pending::<()>().await;
            drop(scope);
        });
        std::future::poll_fn(|context| {
            assert!(call.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(call);
        assert_eq!(session.persisted.current_tab.as_deref(), Some("background"));
        assert!(
            session.action_pending,
            "cancellation must still replace the interrupted worker"
        );
        {
            let scope = PageScope::new(&mut session, Some("shop".into()), true);
            scope.session.pages.local_to_tab.remove(&3);
        }
        assert_eq!(session.persisted.current_tab.as_deref(), Some("shop"));
    }
}
