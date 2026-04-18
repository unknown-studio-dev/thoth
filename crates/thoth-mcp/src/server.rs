//! MCP server core: request dispatch and tool implementations.
//!
//! The transport layer (stdio) lives at the bottom of this file in
//! [`run_stdio`]; the rest is pure logic driven by a [`Server`] handle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use thoth_core::{Event, Fact, Lesson, MemoryKind, MemoryMeta, Outcome, Query, UserSignal};
use thoth_memory::{DisciplineConfig, MemoryManager};
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, RetrieveConfig, Retriever};
use thoth_store::StoreRoot;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};
use uuid::Uuid;

use thoth_retrieve::WatchConfig;

use crate::proto::{
    CallToolResult, Capabilities, ContentBlock, GetPromptResult, InitializeResult,
    MCP_PROTOCOL_VERSION, Prompt, PromptArgument, PromptMessage, Resource, ResourceContents,
    RpcError, RpcIncoming, RpcResponse, ServerInfo, Tool, ToolOutput, error_codes,
};

/// URI of the `MEMORY.md` resource.
const MEMORY_URI: &str = "thoth://memory/MEMORY.md";
/// URI of the `LESSONS.md` resource.
const LESSONS_URI: &str = "thoth://memory/LESSONS.md";

// ===========================================================================
// Server
// ===========================================================================

/// MCP server handle. Cheap to clone — all backing state is behind `Arc`.
#[derive(Clone)]
pub struct Server {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) root: PathBuf,
    store: StoreRoot,
    indexer: Indexer,
    retriever: Retriever,
    graph: thoth_graph::Graph,
}

impl Server {
    /// Open a server rooted at `path` (the `.thoth/` directory).
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = path.as_ref().to_path_buf();
        let store = StoreRoot::open(&root).await?;
        let indexer = Indexer::new(store.clone(), LanguageRegistry::new());
        let retrieve_cfg = RetrieveConfig::load_or_default(&root).await;
        let retriever =
            Retriever::new(store.clone()).with_markdown_boost(retrieve_cfg.rerank_markdown_boost);
        let graph = thoth_graph::Graph::new(store.kv.clone());
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                store,
                indexer,
                retriever,
                graph,
            }),
        })
    }

    /// Spawn a background file watcher if `[watch] enabled = true` in
    /// `config.toml`. The watcher reuses the server's `Indexer` so there
    /// is no lock contention with the MCP daemon. Returns `true` if a
    /// watcher was spawned.
    ///
    /// `src` is the source tree to watch (typically the project root,
    /// i.e. the parent of `.thoth/`).
    pub async fn spawn_watcher(&self, src: PathBuf) -> bool {
        let cfg = WatchConfig::load_or_default(&self.inner.root).await;
        if !cfg.enabled {
            return false;
        }
        let debounce = std::time::Duration::from_millis(cfg.debounce_ms);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if let Err(e) = run_watcher(inner, src, debounce).await {
                warn!(error = %e, "background watcher exited");
            }
        });
        true
    }

    /// Dispatch a single request. Returns `Ok(None)` for notifications.
    pub async fn handle(&self, msg: RpcIncoming) -> Option<RpcResponse> {
        let is_note = msg.is_notification();
        let id = msg.id.clone().unwrap_or(Value::Null);

        let outcome = match msg.method.as_str() {
            "initialize" => Ok(self.initialize()),
            "initialized" | "notifications/initialized" => {
                // Notification — silently accept.
                return None;
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => self.tools_call(msg.params).await,
            // Thoth-private extension: same dispatch as `tools/call` but
            // returns the raw `ToolOutput` (with structured `data`) instead
            // of the text-only `CallToolResult`. Consumed by the CLI
            // thin-client so it can honour `--json` and pretty-print.
            "thoth.call" => self.thoth_call(msg.params).await,
            "resources/list" => Ok(self.resources_list()),
            "resources/read" => self.resources_read(msg.params).await,
            "prompts/list" => Ok(self.prompts_list()),
            "prompts/get" => self.prompts_get(msg.params).await,
            other => Err(RpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        };

        if is_note {
            if let Err(e) = &outcome {
                warn!(code = e.code, msg = %e.message, "notification error (dropped)");
            }
            return None;
        }

        Some(match outcome {
            Ok(result) => RpcResponse::ok(id, result),
            Err(err) => RpcResponse::err(id, err),
        })
    }

    // ---- method handlers --------------------------------------------------

    fn initialize(&self) -> Value {
        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION,
            capabilities: Capabilities {
                tools: Some(json!({})),
                resources: Some(json!({})),
                prompts: Some(json!({})),
            },
            server_info: ServerInfo {
                name: "thoth-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        serde_json::to_value(result).unwrap_or_else(|_| json!({}))
    }

    fn tools_list(&self) -> Value {
        json!({ "tools": tools_catalog() })
    }

    /// MCP `tools/call` — returns a text-only [`CallToolResult`] (which is
    /// what every MCP client understands). The structured `data` half of
    /// [`ToolOutput`] is dropped; clients wanting the machine-readable
    /// form should call [`Self::thoth_call`] via `thoth.call` instead.
    async fn tools_call(&self, params: Value) -> Result<Value, RpcError> {
        let out = self.dispatch_tool(params).await?;
        let wrapped = CallToolResult {
            content: vec![ContentBlock::text(out.text)],
            is_error: out.is_error,
        };
        serde_json::to_value(wrapped)
            .map_err(|e| RpcError::new(error_codes::INTERNAL_ERROR, e.to_string()))
    }

    /// Thoth-private `thoth.call` — returns the raw [`ToolOutput`] so the
    /// CLI thin-client can honour `--json` and pretty-print structured
    /// data. Dispatch logic is shared with [`Self::tools_call`].
    async fn thoth_call(&self, params: Value) -> Result<Value, RpcError> {
        let out = self.dispatch_tool(params).await?;
        serde_json::to_value(out)
            .map_err(|e| RpcError::new(error_codes::INTERNAL_ERROR, e.to_string()))
    }

    /// Shared dispatch used by both `tools/call` and `thoth.call`. Tool
    /// errors are folded into `ToolOutput { is_error: true, .. }` so the
    /// RPC layer can still emit a successful envelope (callers inspect
    /// `is_error` on the payload).
    async fn dispatch_tool(&self, params: Value) -> Result<ToolOutput, RpcError> {
        #[derive(Deserialize)]
        struct CallParams {
            name: String,
            #[serde(default)]
            arguments: Value,
        }
        let CallParams { name, arguments } = serde_json::from_value(params)
            .map_err(|e| RpcError::new(error_codes::INVALID_PARAMS, e.to_string()))?;

        let result = match name.as_str() {
            "thoth_recall" => self.tool_recall(arguments).await,
            "thoth_index" => self.tool_index(arguments).await,
            "thoth_remember_fact" => self.tool_remember_fact(arguments).await,
            "thoth_remember_lesson" => self.tool_remember_lesson(arguments).await,
            "thoth_skills_list" => self.tool_skills_list().await,
            "thoth_memory_show" => self.tool_memory_show().await,
            "thoth_memory_forget" => self.tool_memory_forget().await,
            "thoth_episode_append" => self.tool_episode_append(arguments).await,
            "thoth_lesson_outcome" => self.tool_lesson_outcome(arguments).await,
            "thoth_memory_pending" => self.tool_memory_pending().await,
            "thoth_memory_promote" => self.tool_memory_promote(arguments).await,
            "thoth_memory_reject" => self.tool_memory_reject(arguments).await,
            "thoth_defer_reflect" => self.tool_defer_reflect().await,
            "thoth_memory_history" => self.tool_memory_history(arguments).await,
            "thoth_request_review" => self.tool_request_review(arguments).await,
            "thoth_skill_propose" => self.tool_skill_propose(arguments).await,
            "thoth_impact" => self.tool_impact(arguments).await,
            "thoth_symbol_context" => self.tool_symbol_context(arguments).await,
            "thoth_detect_changes" => self.tool_detect_changes(arguments).await,
            other => {
                return Err(RpcError::new(
                    error_codes::METHOD_NOT_FOUND,
                    format!("unknown tool: {other}"),
                ));
            }
        };

        Ok(match result {
            Ok(out) => out,
            Err(e) => ToolOutput::error(format!("{e:#}")),
        })
    }

    fn resources_list(&self) -> Value {
        let resources = vec![
            Resource {
                uri: MEMORY_URI.to_string(),
                name: "MEMORY.md".to_string(),
                description: "Declarative facts about the codebase.".to_string(),
                mime_type: "text/markdown".to_string(),
            },
            Resource {
                uri: LESSONS_URI.to_string(),
                name: "LESSONS.md".to_string(),
                description: "Lessons learned from past mistakes.".to_string(),
                mime_type: "text/markdown".to_string(),
            },
        ];
        json!({ "resources": resources })
    }

    async fn resources_read(&self, params: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct ReadParams {
            uri: String,
        }
        let ReadParams { uri } = serde_json::from_value(params)
            .map_err(|e| RpcError::new(error_codes::INVALID_PARAMS, e.to_string()))?;

        let file = match uri.as_str() {
            MEMORY_URI => "MEMORY.md",
            LESSONS_URI => "LESSONS.md",
            other => {
                return Err(RpcError::new(
                    error_codes::INVALID_PARAMS,
                    format!("unknown resource uri: {other}"),
                ));
            }
        };

        let path = self.inner.root.join(file);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(RpcError::new(error_codes::INTERNAL_ERROR, e.to_string())),
        };

        let contents = ResourceContents {
            uri,
            mime_type: "text/markdown".to_string(),
            text,
        };
        Ok(json!({ "contents": [contents] }))
    }

    // ---- tool impls -------------------------------------------------------

    async fn tool_recall(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default)]
            top_k: Option<usize>,
            /// Whether to persist this recall as a `QueryIssued` event.
            ///
            /// Default `true` — agent-initiated recalls (MCP tool calls from
            /// Claude) must log, because that's the signal `thoth-gate` keys
            /// off to prove the agent consulted memory before mutating.
            ///
            /// The `UserPromptSubmit` hook passes `false`: its recall is
            /// context injection, not deliberate memory consultation.
            /// Letting the hook's ceremonial recall satisfy the gate would
            /// make the discipline vacuous — every prompt would pre-approve
            /// every subsequent Write/Edit/Bash regardless of whether the
            /// agent actually looked at the chunks.
            #[serde(default)]
            log_event: Option<bool>,
        }
        let Args {
            query,
            top_k,
            log_event,
        } = serde_json::from_value(args)?;
        let q = Query {
            text: query.clone(),
            top_k: top_k.unwrap_or(8).max(1),
            ..Query::text("")
        };
        let out = self.inner.retriever.recall(&q).await?;

        // Log a `QueryIssued` event so the strict-mode gate can prove the
        // agent actually consulted memory before mutating files. Failure
        // here is non-fatal — recall still returns the chunks — but we warn
        // because a missing log entry will defeat the gate.
        if log_event.unwrap_or(true) {
            let ev = Event::QueryIssued {
                id: Uuid::new_v4(),
                text: query,
                at: OffsetDateTime::now_utc(),
            };
            if let Err(e) = self.inner.store.episodes.append(&ev).await {
                warn!(error = %e, "failed to log QueryIssued event");
            }
        }

        let text = render_retrieval(&out, &self.inner.root).await;
        // Serialize the full `Retrieval` so CLI `--json` sees the same
        // shape as the direct-store path. Fall back to an empty object on
        // serde failure (shouldn't happen — `Retrieval: Serialize`).
        let data = serde_json::to_value(&out).unwrap_or_else(|_| json!({}));
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_index(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize, Default)]
        struct Args {
            #[serde(default)]
            path: Option<String>,
        }
        let Args { path } = serde_json::from_value(args).unwrap_or_default();
        let src = PathBuf::from(path.unwrap_or_else(|| ".".to_string()));
        let stats = self.inner.indexer.index_path(&src).await?;
        let text = format!(
            "indexed {}: files={} chunks={} symbols={} calls={} imports={}",
            src.display(),
            stats.files,
            stats.chunks,
            stats.symbols,
            stats.calls,
            stats.imports
        );
        let data = json!({
            "path": src.display().to_string(),
            "files": stats.files,
            "chunks": stats.chunks,
            "symbols": stats.symbols,
            "calls": stats.calls,
            "imports": stats.imports,
            "embedded": stats.embedded,
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_remember_fact(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            text: String,
            #[serde(default)]
            tags: Vec<String>,
            /// If set, force staging even when the discipline config says
            /// `memory_mode = "auto"`. The agent should set this whenever
            /// it's uncertain — matches the `thoth_request_review` intent.
            #[serde(default)]
            stage: bool,
        }
        let Args { text, tags, stage } = serde_json::from_value(args)?;
        let fact = Fact {
            meta: MemoryMeta::new(MemoryKind::Semantic),
            text: text.trim().to_string(),
            tags,
        };
        let cfg = DisciplineConfig::load_or_default(&self.inner.root).await;
        let staged = stage || cfg.requires_review();
        let (path, status) = if staged {
            self.inner.store.markdown.append_pending_fact(&fact).await?;
            (
                self.inner.root.join("MEMORY.pending.md"),
                "staged (review mode) — run `thoth_memory_promote` to accept",
            )
        } else {
            self.inner.store.markdown.append_fact(&fact).await?;
            (self.inner.root.join("MEMORY.md"), "committed to MEMORY.md")
        };
        let text = format!("{status}: {}", first_line(&fact.text));
        let data = json!({
            "text": fact.text,
            "tags": fact.tags,
            "path": path.display().to_string(),
            "staged": staged,
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_remember_lesson(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            trigger: String,
            advice: String,
            #[serde(default)]
            stage: bool,
        }
        let Args {
            trigger,
            advice,
            stage,
        } = serde_json::from_value(args)?;
        let lesson = Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger.trim().to_string(),
            advice: advice.trim().to_string(),
            success_count: 0,
            failure_count: 0,
        };
        let cfg = DisciplineConfig::load_or_default(&self.inner.root).await;
        let staged = stage || cfg.requires_review();

        // Conflict check: a lesson with the same trigger already exists.
        // In review mode we always stage; in auto mode we still refuse to
        // silently overwrite — force the agent to stage + escalate.
        let conflict = self
            .inner
            .store
            .markdown
            .read_lessons()
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|l| l.trigger.trim().eq_ignore_ascii_case(lesson.trigger.trim()));

        let (path, status, staged) = if staged || conflict.is_some() {
            self.inner
                .store
                .markdown
                .append_pending_lesson(&lesson)
                .await?;
            let note = if conflict.is_some() {
                "staged (conflict with existing lesson — user must review)"
            } else {
                "staged (review mode) — run `thoth_memory_promote` to accept"
            };
            (self.inner.root.join("LESSONS.pending.md"), note, true)
        } else {
            self.inner.store.markdown.append_lesson(&lesson).await?;
            (
                self.inner.root.join("LESSONS.md"),
                "committed to LESSONS.md",
                false,
            )
        };
        let text = format!("{status}: {}", lesson.trigger);
        let data = json!({
            "trigger": lesson.trigger,
            "advice": lesson.advice,
            "path": path.display().to_string(),
            "staged": staged,
            "conflict": conflict.map(|l| json!({
                "trigger": l.trigger,
                "existing_advice": l.advice,
            })),
        });
        Ok(ToolOutput::new(data, text))
    }

    // -- review-mode plumbing ----------------------------------------------

    async fn tool_memory_pending(&self) -> anyhow::Result<ToolOutput> {
        let facts = self.inner.store.markdown.read_pending_facts().await?;
        let lessons = self.inner.store.markdown.read_pending_lessons().await?;
        let mut text = String::new();
        text.push_str(&format!("── pending facts ({}) ──\n", facts.len()));
        for (i, f) in facts.iter().enumerate() {
            text.push_str(&format!("[{i}] {}\n", first_line(&f.text)));
        }
        text.push_str(&format!("\n── pending lessons ({}) ──\n", lessons.len()));
        for (i, l) in lessons.iter().enumerate() {
            text.push_str(&format!("[{i}] {}\n", l.trigger));
        }
        if facts.is_empty() && lessons.is_empty() {
            text.push_str("(no pending entries)\n");
        }
        let data = json!({
            "facts": facts
                .iter()
                .map(|f| json!({ "text": f.text, "tags": f.tags }))
                .collect::<Vec<_>>(),
            "lessons": lessons
                .iter()
                .map(|l| json!({
                    "trigger": l.trigger,
                    "advice": l.advice,
                }))
                .collect::<Vec<_>>(),
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_memory_promote(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            kind: String,
            index: usize,
        }
        let Args { kind, index } = serde_json::from_value(args)?;
        let (title, ok) = match kind.as_str() {
            "fact" => match self
                .inner
                .store
                .markdown
                .promote_pending_fact(index)
                .await?
            {
                Some(f) => (first_line(&f.text), true),
                None => (String::new(), false),
            },
            "lesson" => match self
                .inner
                .store
                .markdown
                .promote_pending_lesson(index)
                .await?
            {
                Some(l) => (l.trigger, true),
                None => (String::new(), false),
            },
            other => anyhow::bail!("unknown kind: {other} (expected `fact` or `lesson`)"),
        };
        let text = if ok {
            format!("promoted {kind} [{index}]: {title}")
        } else {
            format!("no pending {kind} at index {index}")
        };
        let data = json!({ "kind": kind, "index": index, "promoted": ok, "title": title });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_memory_reject(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            kind: String,
            index: usize,
            #[serde(default)]
            reason: Option<String>,
        }
        let Args {
            kind,
            index,
            reason,
        } = serde_json::from_value(args)?;
        let (title, ok) = match kind.as_str() {
            "fact" => {
                match self
                    .inner
                    .store
                    .markdown
                    .reject_pending_fact(index, reason.as_deref())
                    .await?
                {
                    Some(f) => (first_line(&f.text), true),
                    None => (String::new(), false),
                }
            }
            "lesson" => {
                match self
                    .inner
                    .store
                    .markdown
                    .reject_pending_lesson(index, reason.as_deref())
                    .await?
                {
                    Some(l) => (l.trigger, true),
                    None => (String::new(), false),
                }
            }
            other => anyhow::bail!("unknown kind: {other} (expected `fact` or `lesson`)"),
        };
        let text = if ok {
            format!("rejected {kind} [{index}]: {title}")
        } else {
            format!("no pending {kind} at index {index}")
        };
        let data = json!({
            "kind": kind,
            "index": index,
            "rejected": ok,
            "title": title,
            "reason": reason,
        });
        Ok(ToolOutput::new(data, text))
    }

    /// In-session escape hatch for the reflection-debt gate.
    ///
    /// Touches `<root>/.reflect-defer`; the gate treats the marker as
    /// a bypass for 30 minutes. MCP tool calls don't route through the
    /// gate (PreToolUse only intercepts Write/Edit/Bash/NotebookEdit),
    /// so this stays callable even when the gate is blocking every
    /// mutation — which is exactly the deadlock it's designed to
    /// resolve. Prefer persisting a real fact/lesson to this; only
    /// reach for defer when there is genuinely nothing durable to
    /// remember from the recent edits.
    async fn tool_defer_reflect(&self) -> anyhow::Result<ToolOutput> {
        let marker = self.inner.root.join(".reflect-defer");
        // `write` updates mtime even if the file already exists, which
        // is what the gate's 30-min TTL checks.
        if let Err(e) = tokio::fs::write(&marker, b"").await {
            anyhow::bail!("write {}: {e}", marker.display());
        }
        let text = format!(
            "wrote defer marker: {} (bypass active for ~30 min)",
            marker.display()
        );
        let data = serde_json::json!({
            "marker": marker.display().to_string(),
            "ttl_secs": 1800,
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_memory_history(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize, Default)]
        struct Args {
            #[serde(default)]
            limit: Option<usize>,
        }
        let Args { limit } = serde_json::from_value(args).unwrap_or_default();
        let mut entries = self.inner.store.markdown.read_history().await?;
        if let Some(n) = limit
            && entries.len() > n
        {
            let skip = entries.len() - n;
            entries.drain(..skip);
        }
        let mut text = String::new();
        for e in &entries {
            text.push_str(&format!(
                "{}  {:<10} {:<7} {}\n",
                e.at_rfc3339, e.op, e.kind, e.title
            ));
        }
        if entries.is_empty() {
            text.push_str("(no history yet)\n");
        }
        let data = serde_json::to_value(&entries).unwrap_or_else(|_| json!([]));
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_request_review(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            kind: String,
            title: String,
            #[serde(default)]
            reason: Option<String>,
        }
        let Args {
            kind,
            title,
            reason,
        } = serde_json::from_value(args)?;
        self.inner
            .store
            .markdown
            .append_history(&thoth_store::markdown::HistoryEntry {
                op: "request_review",
                kind: match kind.as_str() {
                    "fact" => "fact",
                    "lesson" => "lesson",
                    "skill" => "skill",
                    _ => "other",
                },
                title: title.clone(),
                actor: Some("agent".to_string()),
                reason: reason.clone(),
            })
            .await?;
        let text = format!("review requested for {kind}: {title}");
        let data = json!({ "kind": kind, "title": title, "reason": reason });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_skill_propose(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            /// Slug for the proposed skill directory under
            /// `.thoth/skills/<slug>.draft/`.
            slug: String,
            /// The SKILL.md body the agent drafted. Must start with the
            /// `---\nname: ...` frontmatter.
            body: String,
            /// Triggers of the lessons that motivated this proposal — used
            /// only for the history log.
            #[serde(default)]
            source_triggers: Vec<String>,
        }
        let Args {
            slug,
            body,
            source_triggers,
        } = serde_json::from_value(args)?;
        let clean_slug = slug
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>();
        if clean_slug.is_empty() {
            anyhow::bail!("skill slug must contain alphanumeric characters");
        }
        let draft_dir = self
            .inner
            .root
            .join("skills")
            .join(format!("{clean_slug}.draft"));
        tokio::fs::create_dir_all(&draft_dir).await?;
        tokio::fs::write(draft_dir.join("SKILL.md"), body.as_bytes()).await?;
        self.inner
            .store
            .markdown
            .append_history(&thoth_store::markdown::HistoryEntry {
                op: "propose",
                kind: "skill",
                title: clean_slug.clone(),
                actor: Some("agent".to_string()),
                reason: if source_triggers.is_empty() {
                    None
                } else {
                    Some(format!("from lessons: {}", source_triggers.join(", ")))
                },
            })
            .await?;
        let text = format!(
            "skill proposal drafted at {} — review and run `thoth skills install` to accept",
            draft_dir.display()
        );
        let data = json!({
            "slug": clean_slug,
            "path": draft_dir.display().to_string(),
            "source_triggers": source_triggers,
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_skills_list(&self) -> anyhow::Result<ToolOutput> {
        let skills = self.inner.store.markdown.list_skills().await?;
        let text = if skills.is_empty() {
            format!(
                "(no skills installed — drop a folder into {}/skills/)",
                self.inner.root.display()
            )
        } else {
            let mut buf = String::new();
            for s in &skills {
                buf.push_str(&format!("{:<28}  {}\n", s.slug, s.description));
            }
            buf
        };
        let data = serde_json::to_value(&skills).unwrap_or_else(|_| json!([]));
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_memory_forget(&self) -> anyhow::Result<ToolOutput> {
        let mm = MemoryManager::open(&self.inner.root).await?;
        let report = mm.forget_pass().await?;
        let did_work = report.episodes_ttl > 0
            || report.episodes_cap > 0
            || report.lessons_dropped > 0
            || report.lessons_quarantined > 0;
        // Return an empty text surface when the pass was a no-op so
        // hook-driven callers (SessionStart curator, Stop cleanup)
        // don't flood the agent banner with "forget pass: 0 0 0 0"
        // on every healthy session. The structured `data` still
        // carries every counter so scripted callers can distinguish
        // "didn't run" from "ran with zero drops".
        let text = if did_work {
            format!(
                "forget pass: episodes_ttl={} episodes_cap={} lessons_dropped={} lessons_quarantined={}",
                report.episodes_ttl,
                report.episodes_cap,
                report.lessons_dropped,
                report.lessons_quarantined
            )
        } else {
            String::new()
        };
        let data = json!({
            "episodes_ttl": report.episodes_ttl,
            "episodes_cap": report.episodes_cap,
            "lessons_dropped": report.lessons_dropped,
            "lessons_quarantined": report.lessons_quarantined,
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_memory_show(&self) -> anyhow::Result<ToolOutput> {
        let mut text = String::new();
        let mut memory_md: Option<String> = None;
        let mut lessons_md: Option<String> = None;

        for name in ["MEMORY.md", "LESSONS.md"] {
            text.push_str(&format!("─── {name} ───\n"));
            let p = self.inner.root.join(name);
            let body = match tokio::fs::read_to_string(&p).await {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            match &body {
                Some(s) => text.push_str(s),
                None => text.push_str("(not found)\n"),
            }
            text.push('\n');
            match name {
                "MEMORY.md" => memory_md = body,
                "LESSONS.md" => lessons_md = body,
                _ => {}
            }
        }
        let data = json!({
            "memory_md": memory_md,
            "lessons_md": lessons_md,
        });
        Ok(ToolOutput::new(data, text))
    }

    /// Append a raw episodic event. Used by Claude Code hooks to record
    /// what it observed during a session (file edits, tool outcomes, etc.)
    /// so future reflective passes have ground-truth timeline data.
    async fn tool_episode_append(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            /// One of: `file_changed`, `file_deleted`, `query_issued`,
            /// `answer_returned`, `outcome_observed`.
            kind: String,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            commit: Option<String>,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            outcome: Option<Value>,
            #[serde(default)]
            related_to: Option<String>,
        }
        let Args {
            kind,
            path,
            commit,
            text,
            outcome,
            related_to,
        } = serde_json::from_value(args)?;

        let at = OffsetDateTime::now_utc();
        let ev = match kind.as_str() {
            "file_changed" => Event::FileChanged {
                path: PathBuf::from(
                    path.ok_or_else(|| anyhow::anyhow!("file_changed requires `path`"))?,
                ),
                commit,
                at,
            },
            "file_deleted" => Event::FileDeleted {
                path: PathBuf::from(
                    path.ok_or_else(|| anyhow::anyhow!("file_deleted requires `path`"))?,
                ),
                at,
            },
            "query_issued" => Event::QueryIssued {
                id: Uuid::new_v4(),
                text: text.ok_or_else(|| anyhow::anyhow!("query_issued requires `text`"))?,
                at,
            },
            "answer_returned" => Event::AnswerReturned {
                id: related_to
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4),
                chunk_ids: vec![],
                synthesized: false,
                at,
            },
            "outcome_observed" => {
                let parsed: Outcome = serde_json::from_value(
                    outcome
                        .ok_or_else(|| anyhow::anyhow!("outcome_observed requires `outcome`"))?,
                )?;
                Event::OutcomeObserved {
                    related_to: related_to
                        .as_deref()
                        .map(Uuid::parse_str)
                        .transpose()?
                        .unwrap_or_else(Uuid::new_v4),
                    outcome: parsed,
                    at,
                }
            }
            "nudge_invoked" => Event::NudgeInvoked {
                id: related_to
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4),
                intent: text.unwrap_or_default(),
                at,
            },
            other => anyhow::bail!("unknown event kind: {other}"),
        };

        let id = self.inner.store.episodes.append(&ev).await?;
        let text = format!("appended episode #{id} ({kind})");
        let data = json!({ "id": id, "kind": kind });
        Ok(ToolOutput::new(data, text))
    }

    /// Bump lesson confidence counters based on an observed outcome.
    ///
    /// `signal` is one of `success` or `failure`. `triggers` is the list of
    /// lesson triggers that were active when the outcome happened — typically
    /// fed in by a hook from the tool call's prior `thoth_recall` response.
    async fn tool_lesson_outcome(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            signal: String,
            triggers: Vec<String>,
            #[serde(default)]
            note: Option<String>,
        }
        let Args {
            signal,
            triggers,
            note,
        } = serde_json::from_value(args)?;

        let bumped = match signal.as_str() {
            "success" => {
                self.inner
                    .store
                    .markdown
                    .bump_lesson_success(&triggers)
                    .await?
            }
            "failure" => {
                self.inner
                    .store
                    .markdown
                    .bump_lesson_failure(&triggers)
                    .await?
            }
            other => anyhow::bail!("unknown signal: {other} (expected `success` or `failure`)"),
        };

        // Also append an OutcomeObserved episode so the session log shows
        // the lesson being exercised.
        let ev = Event::OutcomeObserved {
            related_to: Uuid::new_v4(),
            outcome: Outcome::UserFeedback {
                signal: match signal.as_str() {
                    "success" => UserSignal::Accept,
                    _ => UserSignal::Reject,
                },
                note,
            },
            at: OffsetDateTime::now_utc(),
        };
        let _ = self.inner.store.episodes.append(&ev).await?;

        let text = format!("{signal}: bumped {bumped} lesson(s)");
        let data = json!({ "signal": signal, "bumped": bumped, "triggers": triggers });
        Ok(ToolOutput::new(data, text))
    }

    // ---- prompts ----------------------------------------------------------

    fn prompts_list(&self) -> Value {
        json!({ "prompts": prompts_catalog() })
    }

    async fn prompts_get(&self, params: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct GetParams {
            name: String,
            #[serde(default)]
            arguments: serde_json::Map<String, Value>,
        }
        let GetParams { name, arguments } = serde_json::from_value(params)
            .map_err(|e| RpcError::new(error_codes::INVALID_PARAMS, e.to_string()))?;

        let (description, body) = match name.as_str() {
            "thoth.reflect" => (
                "Reflect on the session so far and decide what to remember.",
                render_reflect_prompt(&arguments),
            ),
            "thoth.nudge" => {
                // Record that the agent actually expanded the nudge prompt —
                // strict-mode gates use this to distinguish "ran a recall"
                // from "actually reflected on lessons".
                let intent = arg_str(&arguments, "intent").to_string();
                let ev = Event::NudgeInvoked {
                    id: Uuid::new_v4(),
                    intent: intent.clone(),
                    at: OffsetDateTime::now_utc(),
                };
                if let Err(e) = self.inner.store.episodes.append(&ev).await {
                    warn!(error = %e, "failed to log NudgeInvoked event");
                }
                (
                    "Nudge before a risky step: recall relevant lessons and plan.",
                    render_nudge_prompt(&arguments),
                )
            }
            "thoth.grounding_check" => (
                "Verify a claim against the indexed codebase before asserting it.",
                render_grounding_prompt(&arguments),
            ),
            other => {
                return Err(RpcError::new(
                    error_codes::INVALID_PARAMS,
                    format!("unknown prompt: {other}"),
                ));
            }
        };

        let result = GetPromptResult {
            description: description.to_string(),
            messages: vec![PromptMessage {
                role: "user".to_string(),
                content: ContentBlock::text(body),
            }],
        };
        serde_json::to_value(result)
            .map_err(|e| RpcError::new(error_codes::INTERNAL_ERROR, e.to_string()))
    }

    // ---- graph tools -----------------------------------------------------

    /// Blast-radius analysis: BFS from an FQN, grouped by distance.
    ///
    /// With `direction = "up"` this answers "what breaks if I change X?";
    /// `"down"` answers "what does X depend on?"; `"both"` is the union.
    /// The edge kinds followed depend on direction — see
    /// [`thoth_graph::Graph::impact`].
    async fn tool_impact(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            fqn: String,
            #[serde(default)]
            direction: Option<String>,
            #[serde(default)]
            depth: Option<usize>,
        }
        let Args {
            fqn,
            direction,
            depth,
        } = serde_json::from_value(args)?;
        let depth = depth.unwrap_or(3).clamp(1, 8);
        let dir = match direction.as_deref().unwrap_or("up") {
            "up" | "callers" | "incoming" => thoth_graph::BlastDir::Up,
            "down" | "callees" | "outgoing" => thoth_graph::BlastDir::Down,
            "both" => thoth_graph::BlastDir::Both,
            other => {
                anyhow::bail!("invalid direction {other:?}; expected one of: up | down | both")
            }
        };

        // Confirm the symbol exists so the caller gets a clear error
        // instead of an empty result on typos.
        if self.inner.graph.get(&fqn).await?.is_none() {
            let text = format!(
                "symbol not found: {fqn}\n(try `thoth_recall` first to look up the \
                 correct FQN — graph keys are module::name)"
            );
            return Ok(ToolOutput::error(text));
        }

        let hits = self.inner.graph.impact(&fqn, dir, depth).await?;

        // Group by depth for a stable, readable rendering. `BTreeMap`
        // keeps the keys in ascending order without an extra sort.
        let mut by_depth: std::collections::BTreeMap<usize, Vec<&thoth_graph::Node>> =
            std::collections::BTreeMap::new();
        for (node, d) in &hits {
            by_depth.entry(*d).or_default().push(node);
        }

        // Above `impact_group_threshold` nodes, flip to a file-grouped
        // summary per depth ring. A flat list of 200 FQNs drowns the
        // useful signal (which files are involved?); grouping counts
        // nodes per file, ordered by hit density, so the caller sees
        // the tightly-coupled subsystems at a glance. Structured `data`
        // (JSON) is unchanged — the cap is text-surface only.
        let output_cfg = thoth_retrieve::OutputConfig::load_or_default(&self.inner.root).await;
        let group_by_file =
            output_cfg.impact_group_threshold > 0 && hits.len() > output_cfg.impact_group_threshold;

        let mut text = format!(
            "impact({fqn}, direction={}, depth={depth}) — {} nodes{}\n",
            match dir {
                thoth_graph::BlastDir::Up => "up",
                thoth_graph::BlastDir::Down => "down",
                thoth_graph::BlastDir::Both => "both",
            },
            hits.len(),
            if group_by_file {
                " (grouped by file — raise `output.impact_group_threshold` for the flat list)"
            } else {
                ""
            },
        );
        for (d, nodes) in &by_depth {
            text.push_str(&format!("  depth {d}:\n"));
            if group_by_file {
                // Bucket nodes in this ring by their source file, then
                // sort buckets by descending count so the most
                // concentrated dependents surface first.
                let mut by_file: std::collections::BTreeMap<
                    std::path::PathBuf,
                    Vec<&thoth_graph::Node>,
                > = std::collections::BTreeMap::new();
                for n in nodes {
                    by_file.entry(n.path.clone()).or_default().push(*n);
                }
                let mut ordered: Vec<_> = by_file.into_iter().collect();
                ordered.sort_by(|(pa, a), (pb, b)| b.len().cmp(&a.len()).then_with(|| pa.cmp(pb)));
                for (path, bucket) in ordered {
                    // Show up to 3 example FQNs per file so the user
                    // can drill in; more than that is the same noise
                    // the grouping was meant to avoid.
                    let examples: Vec<&str> =
                        bucket.iter().take(3).map(|n| n.fqn.as_str()).collect();
                    let ellipsis = if bucket.len() > examples.len() {
                        format!(", … +{} more", bucket.len() - examples.len())
                    } else {
                        String::new()
                    };
                    text.push_str(&format!(
                        "    {}  ({} symbol{}): {}{}\n",
                        path.display(),
                        bucket.len(),
                        if bucket.len() == 1 { "" } else { "s" },
                        examples.join(", "),
                        ellipsis,
                    ));
                }
            } else {
                for n in nodes {
                    text.push_str(&format!("    {}  {}:{}\n", n.fqn, n.path.display(), n.line));
                }
            }
        }
        if hits.is_empty() {
            text.push_str("  (no reachable symbols at the requested depth)\n");
        }

        let data = json!({
            "fqn": fqn,
            "direction": match dir {
                thoth_graph::BlastDir::Up => "up",
                thoth_graph::BlastDir::Down => "down",
                thoth_graph::BlastDir::Both => "both",
            },
            "depth": depth,
            "total": hits.len(),
            "by_depth": by_depth.iter().map(|(d, nodes)| {
                json!({
                    "depth": d,
                    "nodes": nodes.iter().map(|n| json!({
                        "fqn": n.fqn,
                        "kind": n.kind,
                        "path": n.path.to_string_lossy(),
                        "line": n.line,
                    })).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        });
        Ok(ToolOutput::new(data, text))
    }

    /// 360-degree view of a symbol: callers, callees, parent types,
    /// subtypes, imports-to-this-symbol, and siblings in the same file.
    ///
    /// Unlike `thoth_recall` this is a pure graph lookup keyed on the
    /// exact FQN — use it when the agent already knows the symbol it
    /// wants to understand (e.g. after a recall returned a chunk).
    async fn tool_symbol_context(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            fqn: String,
            #[serde(default)]
            limit: Option<usize>,
        }
        let Args { fqn, limit } = serde_json::from_value(args)?;
        let limit = limit.unwrap_or(32).clamp(1, 128);

        let g = &self.inner.graph;
        let Some(self_node) = g.get(&fqn).await? else {
            return Ok(ToolOutput::error(format!(
                "symbol not found: {fqn}\n(graph keys are `module::name`; use `thoth_recall` \
                 to look up the right FQN first)"
            )));
        };

        let mut callers = g.in_neighbors(&fqn, thoth_graph::EdgeKind::Calls).await?;
        let mut callees = g.out_neighbors(&fqn, thoth_graph::EdgeKind::Calls).await?;
        let mut extends = g
            .out_neighbors(&fqn, thoth_graph::EdgeKind::Extends)
            .await?;
        let mut extended_by = g.in_neighbors(&fqn, thoth_graph::EdgeKind::Extends).await?;
        let mut references = g
            .in_neighbors(&fqn, thoth_graph::EdgeKind::References)
            .await?;
        let unresolved_imports = g
            .out_unresolved(&fqn, thoth_graph::EdgeKind::Imports)
            .await?;

        for v in [
            &mut callers,
            &mut callees,
            &mut extends,
            &mut extended_by,
            &mut references,
        ] {
            v.truncate(limit);
        }

        // Siblings — declared in the same file, excluding self.
        let mut siblings = g.symbols_in_file(&self_node.path).await?;
        siblings.retain(|n| n.fqn != fqn);
        siblings.truncate(limit);

        let node_to_json = |n: &thoth_graph::Node| {
            json!({
                "fqn": n.fqn,
                "kind": n.kind,
                "path": n.path.to_string_lossy(),
                "line": n.line,
            })
        };
        let data = json!({
            "fqn": fqn,
            "kind": self_node.kind,
            "path": self_node.path.to_string_lossy(),
            "line": self_node.line,
            "callers": callers.iter().map(node_to_json).collect::<Vec<_>>(),
            "callees": callees.iter().map(node_to_json).collect::<Vec<_>>(),
            "extends": extends.iter().map(node_to_json).collect::<Vec<_>>(),
            "extended_by": extended_by.iter().map(node_to_json).collect::<Vec<_>>(),
            "references": references.iter().map(node_to_json).collect::<Vec<_>>(),
            "imports_unresolved": unresolved_imports,
            "siblings": siblings.iter().map(node_to_json).collect::<Vec<_>>(),
        });

        let mut text = format!(
            "{} [{}]  {}:{}\n",
            self_node.fqn,
            self_node.kind,
            self_node.path.display(),
            self_node.line,
        );
        let section = |label: &str, nodes: &[thoth_graph::Node], buf: &mut String| {
            if nodes.is_empty() {
                return;
            }
            buf.push_str(&format!("  {label}:\n"));
            for n in nodes {
                buf.push_str(&format!(
                    "    {}  ({}) {}:{}\n",
                    n.fqn,
                    n.kind,
                    n.path.display(),
                    n.line
                ));
            }
        };
        section("callers", &callers, &mut text);
        section("callees", &callees, &mut text);
        section("extends", &extends, &mut text);
        section("extended_by", &extended_by, &mut text);
        section("references", &references, &mut text);
        section("siblings", &siblings, &mut text);
        if !unresolved_imports.is_empty() {
            text.push_str("  imports (external):\n");
            for i in &unresolved_imports {
                text.push_str(&format!("    {i}\n"));
            }
        }

        Ok(ToolOutput::new(data, text))
    }

    /// Given a unified diff, return the symbols the edit touches plus
    /// their upstream blast radius (who calls / references / inherits
    /// from them). Handy as a PR pre-check: "these 7 functions need
    /// re-testing because you modified X".
    ///
    /// Input is a diff text blob (what `git diff` produces). Hunks
    /// that touch files not in the graph are silently ignored.
    async fn tool_detect_changes(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            diff: String,
            #[serde(default)]
            depth: Option<usize>,
        }
        let Args { diff, depth } = serde_json::from_value(args)?;
        let depth = depth.unwrap_or(2).clamp(1, 6);

        let hunks = parse_unified_diff(&diff);
        if hunks.is_empty() {
            return Ok(ToolOutput::error(
                "diff contained no parseable hunks; expected `git diff` output".to_string(),
            ));
        }

        // Collect touched symbols: for every hunk, intersect its post-
        // image line range with the declaration spans of symbols in
        // the file. We use `symbols_in_file` on the post-image path
        // because that's the identity after the edit.
        let g = &self.inner.graph;
        let store = &self.inner.store;
        let mut touched: std::collections::BTreeMap<String, thoth_graph::Node> =
            std::collections::BTreeMap::new();
        let mut file_hits: Vec<serde_json::Value> = Vec::new();

        for DiffHunk { path, ranges } in &hunks {
            // Look up all symbol rows for this file (which carry the
            // `(start, end)` line span we need to test hunk overlap). Then
            // fetch the matching graph Nodes for rendering via a second
            // round trip — nodes and rows key on the same FQN but live in
            // different tables.
            let path_buf = std::path::PathBuf::from(path);
            let sym_rows = match store.kv.symbols_for_path(&path_buf).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            if sym_rows.is_empty() {
                continue;
            }
            let nodes = g.symbols_in_file(&path_buf).await?;
            let by_fqn: std::collections::HashMap<&str, &thoth_graph::Node> =
                nodes.iter().map(|n| (n.fqn.as_str(), n)).collect();

            let mut hit_in_file: Vec<String> = Vec::new();
            for row in &sym_rows {
                let (s, e) = (row.start_line, row.end_line);
                if ranges.iter().any(|(a, b)| !(s > *b || e < *a))
                    && let Some(n) = by_fqn.get(row.fqn.as_str())
                {
                    touched.insert(n.fqn.clone(), (*n).clone());
                    hit_in_file.push(n.fqn.clone());
                }
            }
            if !hit_in_file.is_empty() {
                file_hits.push(json!({
                    "path": path,
                    "hunks": ranges.len(),
                    "touched": hit_in_file,
                }));
            }
        }

        if touched.is_empty() {
            let text = format!(
                "diff touched {} file(s) but no indexed symbols overlapped any hunk",
                hunks.len()
            );
            return Ok(ToolOutput::new(
                json!({ "touched": [], "impact": [], "hunks": hunks.len() }),
                text,
            ));
        }

        // Blast radius: for every touched symbol, upstream impact. Union
        // into a single de-duped set so cross-symbol overlap (common on
        // real PRs) is naturally collapsed.
        let mut impact_seen: std::collections::HashMap<String, (thoth_graph::Node, usize)> =
            std::collections::HashMap::new();
        for node in touched.values() {
            let radius = g
                .impact(&node.fqn, thoth_graph::BlastDir::Up, depth)
                .await?;
            for (n, d) in radius {
                // Keep the *shortest* distance seen across all roots so
                // a symbol reached both directly and transitively is
                // rendered at its true minimum depth.
                impact_seen
                    .entry(n.fqn.clone())
                    .and_modify(|existing| {
                        if d < existing.1 {
                            existing.1 = d;
                        }
                    })
                    .or_insert((n, d));
            }
        }
        // Don't double-list the touched symbols themselves as part of
        // their own blast radius.
        for fqn in touched.keys() {
            impact_seen.remove(fqn);
        }

        let mut impact_vec: Vec<(thoth_graph::Node, usize)> = impact_seen.into_values().collect();
        impact_vec.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.fqn.cmp(&b.0.fqn)));

        let node_json = |n: &thoth_graph::Node| {
            json!({
                "fqn": n.fqn,
                "kind": n.kind,
                "path": n.path.to_string_lossy(),
                "line": n.line,
            })
        };
        let data = json!({
            "hunks": hunks.len(),
            "files": file_hits,
            "touched": touched.values().map(node_json).collect::<Vec<_>>(),
            "impact": impact_vec.iter().map(|(n, d)| {
                let mut v = node_json(n);
                v["depth"] = json!(d);
                v
            }).collect::<Vec<_>>(),
            "depth": depth,
        });

        let mut text = format!(
            "diff touched {} symbol(s) across {} file(s); upstream blast radius (depth {depth}): {} node(s)\n",
            touched.len(),
            file_hits.len(),
            impact_vec.len(),
        );
        text.push_str("touched:\n");
        for n in touched.values() {
            text.push_str(&format!("  {}  {}:{}\n", n.fqn, n.path.display(), n.line));
        }
        if !impact_vec.is_empty() {
            text.push_str("impact (depth / fqn / location):\n");
            for (n, d) in &impact_vec {
                text.push_str(&format!(
                    "  @{d}  {}  {}:{}\n",
                    n.fqn,
                    n.path.display(),
                    n.line
                ));
            }
        }

        Ok(ToolOutput::new(data, text))
    }
}

/// One parsed hunk: a file path + every post-image line range the diff
/// touches inside that file. Pure value, Display-free — the caller joins
/// with the graph to get symbol-level resolution.
#[derive(Debug)]
struct DiffHunk {
    path: String,
    /// `(start, end)` inclusive line ranges, 1-based. A pure-deletion
    /// hunk at post-image line N is represented as `(N, N)` so it still
    /// overlaps any symbol whose declaration spans N.
    ranges: Vec<(u32, u32)>,
}

/// Parse a git unified diff into per-file line-range hunks.
///
/// Accepts the output of `git diff` / `git diff --staged` as well as
/// rustfmt-style patches. Binary / rename-only entries are skipped.
/// Paths are taken from the `+++ b/...` header (falling back to `--- a/...`
/// for pure deletions where the `+++` is `/dev/null`).
fn parse_unified_diff(diff: &str) -> Vec<DiffHunk> {
    let mut out: Vec<DiffHunk> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_ranges: Vec<(u32, u32)> = Vec::new();

    fn flush(out: &mut Vec<DiffHunk>, path: &mut Option<String>, ranges: &mut Vec<(u32, u32)>) {
        if let Some(p) = path.take() {
            if !ranges.is_empty() {
                out.push(DiffHunk {
                    path: p,
                    ranges: std::mem::take(ranges),
                });
            } else {
                // Pure rename / binary — drop silently.
                ranges.clear();
            }
        }
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            flush(&mut out, &mut current_path, &mut current_ranges);
            // `+++ b/path` or `+++ /dev/null` — tolerate both.
            let raw = rest.trim();
            let path = raw.strip_prefix("b/").unwrap_or(raw);
            if path != "/dev/null" {
                current_path = Some(path.to_string());
            }
        } else if line.starts_with("--- ") {
            // Handle the fallback where the post-image is /dev/null
            // (pure deletion) — we still want to emit a "file touched"
            // record so the caller sees it, but we have no post-image
            // lines. Record the pre-image path against an empty range
            // list; `flush` will drop it cleanly because `ranges` stays
            // empty.
            if current_path.is_none()
                && let Some(rest) = line.strip_prefix("--- ")
            {
                let raw = rest.trim();
                let path = raw.strip_prefix("a/").unwrap_or(raw);
                if path != "/dev/null" {
                    current_path = Some(path.to_string());
                }
            }
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            // `@@ -a,b +c,d @@ ...` — we only care about the `+c,d` half.
            // `d` defaults to `1` if omitted (per unified-diff spec).
            if let Some(end) = rest.find(" @@")
                && let Some((start, count)) = parse_post_image_range(&rest[..end])
                && count > 0
                && current_path.is_some()
            {
                current_ranges.push((start, start + count - 1));
            }
        }
    }
    flush(&mut out, &mut current_path, &mut current_ranges);
    out
}

/// Parse the `+c,d` half of a `@@ -a,b +c,d @@` hunk header. `d` is
/// optional and defaults to `1` per the unified-diff spec.
fn parse_post_image_range(header: &str) -> Option<(u32, u32)> {
    let plus = header.split_whitespace().find(|p| p.starts_with('+'))?;
    let body = plus.trim_start_matches('+');
    let (start_str, count_str) = match body.split_once(',') {
        Some((s, c)) => (s, c),
        None => (body, "1"),
    };
    Some((start_str.parse().ok()?, count_str.parse().ok()?))
}

// ===========================================================================
// Tool catalog
// ===========================================================================

fn tools_catalog() -> Vec<Tool> {
    vec![
        Tool {
            name: "thoth_recall".to_string(),
            description: "Hybrid recall (symbol + BM25 + graph + markdown) over the code memory. \
                          Returns ranked chunks with path, line span, and preview."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language or keyword query." },
                    "top_k": { "type": "integer", "minimum": 1, "maximum": 64, "default": 8 },
                    "log_event": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether to persist this call as a `query_issued` event in \
                                        episodes.db. Agent-initiated recalls (default true) MUST log \
                                        — that's how `thoth-gate` proves the agent consulted memory \
                                        before mutating. Automated hooks that auto-recall for context \
                                        injection (e.g. UserPromptSubmit) pass `false` so their \
                                        ceremonial recall doesn't satisfy the gate on the agent's behalf."
                    }
                },
                "required": ["query"]
            }),
        },
        Tool {
            name: "thoth_index".to_string(),
            description: "Walk a source tree, parse every supported file, and populate the \
                          indexes (symbols, call graph, BM25, chunks)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Source path. Defaults to '.'." }
                }
            }),
        },
        Tool {
            name: "thoth_remember_fact".to_string(),
            description: "Append a semantic fact to MEMORY.md. Use this when you learn \
                          something about the codebase that should survive across sessions."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The fact itself. First line becomes the heading." },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for later filtering."
                    }
                },
                "required": ["text"]
            }),
        },
        Tool {
            name: "thoth_remember_lesson".to_string(),
            description: "Append a reflective lesson to LESSONS.md. Use this after a mistake \
                          or surprise so future sessions can avoid the trap."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "trigger": { "type": "string", "description": "When this lesson should be recalled." },
                    "advice":  { "type": "string", "description": "The lesson / rule itself." }
                },
                "required": ["trigger", "advice"]
            }),
        },
        Tool {
            name: "thoth_skills_list".to_string(),
            description: "List every installed skill under .thoth/skills/.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "thoth_memory_show".to_string(),
            description: "Return the current MEMORY.md and LESSONS.md as plain text.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "thoth_memory_forget".to_string(),
            description: "Run the deterministic forget pass over the episodic log: drops \
                          events older than the configured TTL and caps the log to max_episodes."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "thoth_episode_append".to_string(),
            description: "Append one observed event (file edit, query, outcome, ...) to the \
                          episodic log. Call this from hooks so future reflect passes have \
                          accurate timeline data."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["file_changed", "file_deleted", "query_issued",
                                 "answer_returned", "outcome_observed", "nudge_invoked"]
                    },
                    "path": { "type": "string" },
                    "commit": { "type": "string" },
                    "text": { "type": "string" },
                    "outcome": { "type": "object" },
                    "related_to": { "type": "string", "description": "UUID of a prior event." }
                },
                "required": ["kind"]
            }),
        },
        Tool {
            name: "thoth_lesson_outcome".to_string(),
            description: "Record that a set of active lessons was followed successfully or \
                          violated. Bumps their confidence counters and writes an outcome \
                          event to the episodic log."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "signal":   { "type": "string", "enum": ["success", "failure"] },
                    "triggers": { "type": "array", "items": { "type": "string" } },
                    "note":     { "type": "string" }
                },
                "required": ["signal", "triggers"]
            }),
        },
        Tool {
            name: "thoth_memory_pending".to_string(),
            description: "List every fact and lesson staged but not yet promoted. \
                          Pending entries live in MEMORY.pending.md / LESSONS.pending.md \
                          and are created automatically when `memory_mode = \"review\"`."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "thoth_memory_promote".to_string(),
            description: "Promote a staged fact or lesson into the canonical MEMORY.md / \
                          LESSONS.md. Use `thoth_memory_pending` first to see indices."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind":  { "type": "string", "enum": ["fact", "lesson"] },
                    "index": { "type": "integer", "minimum": 0 }
                },
                "required": ["kind", "index"]
            }),
        },
        Tool {
            name: "thoth_memory_reject".to_string(),
            description: "Drop a staged fact or lesson without promoting it. Optional \
                          `reason` is recorded in memory-history.jsonl."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind":   { "type": "string", "enum": ["fact", "lesson"] },
                    "index":  { "type": "integer", "minimum": 0 },
                    "reason": { "type": "string" }
                },
                "required": ["kind", "index"]
            }),
        },
        Tool {
            name: "thoth_defer_reflect".to_string(),
            description: "Create `<root>/.reflect-defer` to bypass the reflection-debt gate \
                          for ~30 minutes. In-session escape hatch when every mutation is \
                          blocked and you can't restart to set `THOTH_DEFER_REFLECT=1`. \
                          Prefer `thoth_remember_fact`/`thoth_remember_lesson` when there is \
                          real insight to persist — defer is for the case where there is \
                          genuinely nothing durable to remember yet."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "thoth_memory_history".to_string(),
            description: "Return the memory-history.jsonl log — every stage / promote / \
                          reject / quarantine / propose operation with timestamps."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "description": "Return the latest N entries only." }
                }
            }),
        },
        Tool {
            name: "thoth_request_review".to_string(),
            description: "Flag an uncertain fact, lesson, or skill for the human to review. \
                          Use when you're about to remember something but aren't sure — this \
                          writes a `request_review` entry to memory-history.jsonl so the user \
                          can act on it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind":   { "type": "string", "enum": ["fact", "lesson", "skill"] },
                    "title":  { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["kind", "title"]
            }),
        },
        Tool {
            name: "thoth_impact".to_string(),
            description: "Blast-radius analysis over the code graph. Given a symbol FQN, \
                          returns every reachable symbol grouped by distance. Use \
                          `direction=\"up\"` (default) to answer \"what breaks if I change \
                          this?\" (callers / references / subtypes); `\"down\"` for \
                          \"what does this depend on?\" (callees / parent types)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "fqn": { "type": "string", "description": "Fully qualified name (module::symbol)." },
                    "direction": {
                        "type": "string",
                        "enum": ["up", "down", "both"],
                        "default": "up"
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8,
                        "default": 3
                    }
                },
                "required": ["fqn"]
            }),
        },
        Tool {
            name: "thoth_symbol_context".to_string(),
            description: "360-degree view of a single symbol: callers, callees, parent types, \
                          subtypes, references, siblings, and unresolved imports. Use this \
                          when you already know the FQN of a symbol and want structured context \
                          around it (post-`thoth_recall` drill-down)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "fqn": { "type": "string", "description": "Fully qualified name (module::symbol)." },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 128,
                        "default": 32,
                        "description": "Per-section cap on the returned neighbours."
                    }
                },
                "required": ["fqn"]
            }),
        },
        Tool {
            name: "thoth_detect_changes".to_string(),
            description: "Parse a unified diff (e.g. `git diff`), find every indexed symbol \
                          whose declaration span overlaps a changed hunk, and return their \
                          upstream blast radius. Ideal as a PR pre-check — answers \"which \
                          code is downstream of my edit and should be re-tested?\"."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "diff":  { "type": "string", "description": "Unified diff text (`git diff` output)." },
                    "depth": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 6,
                        "default": 2,
                        "description": "Blast-radius depth (BFS levels of callers / references / subtypes)."
                    }
                },
                "required": ["diff"]
            }),
        },
        Tool {
            name: "thoth_skill_propose".to_string(),
            description: "Draft a new SKILL.md under .thoth/skills/<slug>.draft/ — used when \
                          you've noticed ≥5 related lessons and want to consolidate them into \
                          a reusable skill. The user promotes via `thoth skills install`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "slug":            { "type": "string", "description": "kebab-case slug for the draft directory." },
                    "body":            { "type": "string", "description": "Full SKILL.md body starting with `---` frontmatter." },
                    "source_triggers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Triggers of the lessons this skill consolidates."
                    }
                },
                "required": ["slug", "body"]
            }),
        },
    ]
}

// ===========================================================================
// Prompts catalog
// ===========================================================================

/// Descriptors advertised by `prompts/list`. Each maps to a renderer in
/// [`Server::prompts_get`]; rendering is pure string substitution so the
/// server stays deterministic and dependency-free.
fn prompts_catalog() -> Vec<Prompt> {
    vec![
        Prompt {
            name: "thoth.reflect".to_string(),
            description:
                "End-of-step self-reflection: decide whether to save a lesson or fact based \
                 on what just happened."
                    .to_string(),
            arguments: vec![
                PromptArgument {
                    name: "summary".to_string(),
                    description: "One-paragraph summary of what the agent just did.".to_string(),
                    required: true,
                },
                PromptArgument {
                    name: "outcome".to_string(),
                    description: "What went right or wrong (tests, user feedback, etc.)."
                        .to_string(),
                    required: false,
                },
            ],
        },
        Prompt {
            name: "thoth.nudge".to_string(),
            description:
                "Pre-action nudge: surface the most relevant lessons and force the agent to \
                 acknowledge them before proceeding."
                    .to_string(),
            arguments: vec![PromptArgument {
                name: "intent".to_string(),
                description: "What the agent is about to do.".to_string(),
                required: true,
            }],
        },
        Prompt {
            name: "thoth.grounding_check".to_string(),
            description: "Ask the agent to verify a factual claim against the indexed code before \
                 asserting it to the user."
                .to_string(),
            arguments: vec![PromptArgument {
                name: "claim".to_string(),
                description: "The claim to verify.".to_string(),
                required: true,
            }],
        },
    ]
}

fn arg_str<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn render_reflect_prompt(args: &serde_json::Map<String, Value>) -> String {
    let summary = arg_str(args, "summary");
    let outcome = arg_str(args, "outcome");
    format!(
        "You just finished a step. Reflect on it before moving on.\n\
         \n\
         ## What you did\n\
         {summary}\n\
         \n\
         ## Outcome observed\n\
         {outcome}\n\
         \n\
         ## Decide\n\
         1. Is there a durable FACT worth saving about this codebase?\n\
            If yes, call `thoth_remember_fact` with a one-line summary.\n\
         2. Is there a LESSON — a non-obvious pattern a future session would miss?\n\
            If yes, call `thoth_remember_lesson` with a crisp `trigger` and `advice`.\n\
         3. If neither, reply `no memory needed` and continue.\n\
         \n\
         Be conservative: only save memory that is useful, specific, and not \
         already obvious from the code itself.",
        summary = if summary.is_empty() {
            "(not provided)"
        } else {
            summary
        },
        outcome = if outcome.is_empty() {
            "(not provided)"
        } else {
            outcome
        },
    )
}

fn render_nudge_prompt(args: &serde_json::Map<String, Value>) -> String {
    let intent = arg_str(args, "intent");
    format!(
        "Before you act, recall what past sessions learned.\n\
         \n\
         ## Intended action\n\
         {intent}\n\
         \n\
         ## Required checks\n\
         1. Call `thoth_recall` with a short query derived from the intent above.\n\
         2. Read LESSONS.md via `resources/read thoth://memory/LESSONS.md` and pick \
            every lesson whose `trigger` plausibly applies.\n\
         3. Restate the plan in one paragraph, naming each lesson you're honouring.\n\
         4. Only then execute. If a lesson advises against the plan, STOP and ask \
            the user before proceeding.",
        intent = if intent.is_empty() {
            "(not provided)"
        } else {
            intent
        },
    )
}

fn render_grounding_prompt(args: &serde_json::Map<String, Value>) -> String {
    let claim = arg_str(args, "claim");
    format!(
        "Verify the following claim against the indexed codebase BEFORE asserting it.\n\
         \n\
         ## Claim\n\
         {claim}\n\
         \n\
         ## Procedure\n\
         1. Call `thoth_recall` with the most load-bearing nouns from the claim.\n\
         2. Read the returned chunks and decide: supported, contradicted, or \
            insufficient evidence.\n\
         3. If supported, cite at least one chunk id when you answer the user.\n\
         4. If contradicted or insufficient, say so honestly — do not hedge.",
        claim = if claim.is_empty() {
            "(not provided)"
        } else {
            claim
        },
    )
}

// ===========================================================================
// Rendering helpers
// ===========================================================================

async fn render_retrieval(r: &thoth_core::Retrieval, root: &Path) -> String {
    // The rendering lives on `Retrieval::render_with()` so the CLI and
    // the MCP-text surface stay byte-for-byte identical. Budgets come
    // from `<root>/config.toml [output]` (max_body_lines, max_total_bytes),
    // so operators can tune the context cost of recall without rebuilding.
    let cfg = thoth_retrieve::OutputConfig::load_or_default(root).await;
    r.render_with(&cfg.render_options())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

// ===========================================================================
// Background file watcher
// ===========================================================================

/// Watch `src` for file changes and reindex through `inner.indexer`.
///
/// Mirrors the debounce + batch logic in `cmd_watch` but runs in-process
/// alongside the MCP daemon, sharing the same `Indexer` (and therefore the
/// same redb write lock). This avoids the "daemon is running" conflict
/// that blocks the standalone `thoth watch`.
async fn run_watcher(
    inner: Arc<Inner>,
    src: PathBuf,
    debounce: std::time::Duration,
) -> anyhow::Result<()> {
    use thoth_parse::watch::Watcher;

    let mut w = Watcher::watch(&src, 1024)?;
    debug!(path = %src.display(), "background watcher started");

    loop {
        let Some(ev) = w.recv().await else {
            debug!("watcher channel closed");
            break;
        };

        // Debounce: drain events arriving within the window.
        let mut batch = vec![ev];
        let deadline = tokio::time::Instant::now() + debounce;
        while let Ok(Some(extra)) = tokio::time::timeout_at(deadline, w.recv()).await {
            batch.push(extra);
        }

        let mut changed = std::collections::HashSet::new();
        let mut deleted = std::collections::HashSet::new();
        for ev in batch {
            match ev {
                thoth_core::Event::FileChanged { path, .. } => {
                    deleted.remove(&path);
                    changed.insert(path);
                }
                thoth_core::Event::FileDeleted { path, .. } => {
                    changed.remove(&path);
                    deleted.insert(path);
                }
                _ => {}
            }
        }

        let changed_n = changed.len();
        let deleted_n = deleted.len();

        for path in deleted {
            if let Err(e) = inner.indexer.purge_path(&path).await {
                warn!(?path, error = %e, "watcher: purge failed");
            }
        }
        for path in changed {
            if let Err(e) = inner.indexer.index_file(&path).await {
                warn!(?path, error = %e, "watcher: re-index failed");
            }
        }

        if changed_n + deleted_n > 0 {
            if let Err(e) = inner.indexer.commit().await {
                warn!(error = %e, "watcher: fts commit failed");
            }
            debug!(
                changed = changed_n,
                deleted = deleted_n,
                "watcher: reindexed"
            );
        }
    }
    Ok(())
}

// ===========================================================================
// Stdio transport
// ===========================================================================

/// Run the server on stdin/stdout until EOF or ctrl-c.
///
/// Each JSON-RPC message is expected on its own line. Responses are emitted
/// as newline-terminated JSON on stdout; all logging goes to stderr via
/// `tracing`.
pub async fn run_stdio(server: Server) -> anyhow::Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let n = tokio::select! {
            res = reader.read_line(&mut line) => res?,
            _ = tokio::signal::ctrl_c() => {
                debug!("ctrl-c; shutting down mcp");
                0
            }
        };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcIncoming>(trimmed) {
            Ok(msg) => server.handle(msg).await,
            Err(e) => Some(RpcResponse::err(
                Value::Null,
                RpcError::new(error_codes::PARSE_ERROR, format!("parse error: {e}")),
            )),
        };

        if let Some(resp) = response {
            let text = serde_json::to_string(&resp)?;
            stdout.write_all(text.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// Canonical path for the Unix domain socket that the CLI connects to.
pub fn socket_path(root: &Path) -> std::path::PathBuf {
    root.join("mcp.sock")
}

/// Run a Unix-socket sidecar alongside the stdio transport.
///
/// Binds `.thoth/mcp.sock` and accepts connections in a loop. Each
/// connection is a short-lived JSON-RPC session (one line in → one line
/// out, then close). The socket is removed on clean shutdown.
///
/// This is the "thin-client" entry point: when the CLI detects the socket
/// it forwards requests here instead of opening the store directly,
/// avoiding the redb exclusive-lock conflict.
pub async fn run_socket(server: Server) -> anyhow::Result<()> {
    use tokio::net::{UnixListener, UnixStream};

    let sock = socket_path(&server.inner.root);

    // Try binding first. Only if it fails with `AddrInUse` do we probe
    // the existing socket and, if nothing is listening, unlink and retry.
    // This avoids the race where two daemons start at the same time, and
    // the "remove stale and rebind" pattern of the previous version would
    // happily overwrite an actively-used socket.
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Peer responsive? Then another daemon owns the socket — bail.
            if UnixStream::connect(&sock).await.is_ok() {
                return Err(anyhow::anyhow!(
                    "another thoth-mcp is already listening on {}",
                    sock.display()
                ));
            }
            // Stale socket file — safe to remove and retry.
            let _ = std::fs::remove_file(&sock);
            UnixListener::bind(&sock)?
        }
        Err(e) => return Err(e.into()),
    };
    debug!(path = %sock.display(), "mcp socket listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_socket_conn(server, stream).await {
                debug!(error = %e, "socket connection error");
            }
        });
    }
}

/// Handle one Unix-socket connection: read lines, dispatch, respond.
async fn handle_socket_conn(server: Server, stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcIncoming>(trimmed) {
            Ok(msg) => server.handle(msg).await,
            Err(e) => Some(RpcResponse::err(
                Value::Null,
                RpcError::new(error_codes::PARSE_ERROR, format!("parse error: {e}")),
            )),
        };

        if let Some(resp) = response {
            let text = serde_json::to_string(&resp)?;
            writer.write_all(text.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }
    Ok(())
}
