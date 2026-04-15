//! MCP server core: request dispatch and tool implementations.
//!
//! The transport layer (stdio) lives at the bottom of this file in
//! [`run_stdio`]; the rest is pure logic driven by a [`Server`] handle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use thoth_core::{Fact, Lesson, MemoryKind, MemoryMeta, Query};
use thoth_memory::MemoryManager;
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, Retriever};
use thoth_store::StoreRoot;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

use crate::proto::{
    CallToolResult, Capabilities, ContentBlock, InitializeResult, MCP_PROTOCOL_VERSION, Resource,
    ResourceContents, RpcError, RpcIncoming, RpcResponse, ServerInfo, Tool, ToolOutput,
    error_codes,
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
}

impl Server {
    /// Open a server rooted at `path` (the `.thoth/` directory).
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = path.as_ref().to_path_buf();
        let store = StoreRoot::open(&root).await?;
        let indexer = Indexer::new(store.clone(), LanguageRegistry::new());
        let retriever = Retriever::new(store.clone());
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                store,
                indexer,
                retriever,
            }),
        })
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
        }
        let Args { query, top_k } = serde_json::from_value(args)?;
        let q = Query {
            text: query,
            top_k: top_k.unwrap_or(8).max(1),
            ..Query::text("")
        };
        let out = self.inner.retriever.recall(&q).await?;
        let text = render_retrieval(&out);
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
        }
        let Args { text, tags } = serde_json::from_value(args)?;
        let fact = Fact {
            meta: MemoryMeta::new(MemoryKind::Semantic),
            text: text.trim().to_string(),
            tags,
        };
        self.inner.store.markdown.append_fact(&fact).await?;
        let text = format!("remembered fact: {}", first_line(&fact.text));
        let data = json!({
            "text": fact.text,
            "tags": fact.tags,
            "path": self.inner.root.join("MEMORY.md").display().to_string(),
        });
        Ok(ToolOutput::new(data, text))
    }

    async fn tool_remember_lesson(&self, args: Value) -> anyhow::Result<ToolOutput> {
        #[derive(Deserialize)]
        struct Args {
            trigger: String,
            advice: String,
        }
        let Args { trigger, advice } = serde_json::from_value(args)?;
        let lesson = Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger.trim().to_string(),
            advice: advice.trim().to_string(),
            success_count: 0,
            failure_count: 0,
        };
        self.inner.store.markdown.append_lesson(&lesson).await?;
        let text = format!("recorded lesson for trigger: {}", lesson.trigger);
        let data = json!({
            "trigger": lesson.trigger,
            "advice": lesson.advice,
            "path": self.inner.root.join("LESSONS.md").display().to_string(),
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
        let text = format!(
            "forget pass: episodes_ttl={} episodes_cap={} lessons_dropped={}",
            report.episodes_ttl, report.episodes_cap, report.lessons_dropped
        );
        let data = json!({
            "episodes_ttl": report.episodes_ttl,
            "episodes_cap": report.episodes_cap,
            "lessons_dropped": report.lessons_dropped,
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
                    "top_k": { "type": "integer", "minimum": 1, "maximum": 64, "default": 8 }
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
    ]
}

// ===========================================================================
// Rendering helpers
// ===========================================================================

fn render_retrieval(r: &thoth_core::Retrieval) -> String {
    // The rendering lives on `Retrieval::render()` so the CLI and the
    // MCP-text surface stay byte-for-byte identical.
    r.render()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
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
async fn handle_socket_conn(
    server: Server,
    stream: tokio::net::UnixStream,
) -> anyhow::Result<()> {
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
