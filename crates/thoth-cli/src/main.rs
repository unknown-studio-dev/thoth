//! The `thoth` command-line interface.
//!
//! ```text
//! thoth init                        # create .thoth/ in the current directory
//! thoth setup                       # interactive config wizard (config.toml)
//! thoth setup --show                # print current config, no writes
//! thoth index [PATH]                # walk + parse + index (optionally embed)
//! thoth query <TEXT>                # hybrid recall (Mode::Zero by default)
//! thoth watch [PATH]                # stay resident, reindex on change
//! thoth memory show                 # cat MEMORY.md + LESSONS.md
//! thoth memory edit                 # $EDITOR on MEMORY.md
//! thoth memory fact <TEXT>          # append a fact
//! thoth memory lesson <WHEN> <DO>   # append a lesson
//! thoth memory forget               # run TTL + capacity eviction pass
//! thoth memory nudge                # Mode::Full: synth-driven lesson proposals
//! thoth skills list
//! thoth skills install [--scope project|user]
//! thoth hooks install [--scope project|user]
//! thoth hooks uninstall [--scope project|user]
//! thoth hooks exec <event>              # runtime dispatcher (Claude Code)
//! thoth mcp install [--scope project|user]
//! thoth mcp uninstall [--scope project|user]
//! thoth install [--scope project|user]  # skill + hooks + mcp in one shot
//! thoth uninstall [--scope project|user]
//! ```
//!
//! Mode::Full flags (enabled by the matching Cargo feature):
//!
//! ```text
//! --embedder voyage|openai|cohere   # semantic search provider
//! --synth    anthropic              # LLM synthesizer
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use thoth_core::{Embedder, Fact, Lesson, MemoryKind, MemoryMeta, Query, Synthesizer};
use thoth_memory::MemoryManager;
use thoth_parse::{LanguageRegistry, watch::Watcher};
use thoth_retrieve::{IndexProgress, Indexer, Retriever};
use thoth_store::markdown::MarkdownStore;
use thoth_store::{StoreRoot, VectorStore};
use tracing::warn;

mod daemon;
mod hooks;
mod setup;

// ------------------------------------------------------------------ CLI spec

#[derive(Parser, Debug)]
#[command(name = "thoth", version, about = "Long-term memory for coding agents.")]
struct Cli {
    /// Path to the `.thoth/` directory. Defaults to `./.thoth`.
    #[arg(long, global = true, default_value = ".thoth")]
    root: PathBuf,

    /// Emit machine-readable JSON for subcommands that support it.
    #[arg(long, global = true)]
    json: bool,

    /// Mode::Full: semantic search provider. Requires the matching Cargo
    /// feature (`voyage`, `openai`, `cohere`). The API key is read from the
    /// provider's standard env var.
    #[arg(long, global = true, value_enum)]
    embedder: Option<EmbedderKind>,

    /// Mode::Full: LLM synthesizer. Requires the `anthropic` Cargo feature.
    /// The API key is read from `ANTHROPIC_API_KEY`.
    #[arg(long, global = true, value_enum)]
    synth: Option<SynthKind>,

    /// Show internal debug logs. Without this the CLI only prints
    /// user-facing output; `tracing` events are hidden. Overrides `RUST_LOG`
    /// when passed. Repeat for more detail (`-v` = debug, `-vv` = trace).
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum EmbedderKind {
    Voyage,
    Openai,
    Cohere,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum SynthKind {
    Anthropic,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialize a new `.thoth/` directory.
    Init,

    /// Interactive setup wizard — writes `<root>/config.toml`.
    ///
    /// In CI or other non-TTY contexts it falls back to defaults without
    /// prompting. Use `--show` to print the current config without writing.
    Setup {
        /// Print current config and exit. Does not modify anything.
        #[arg(long)]
        show: bool,
        /// Skip prompts and write defaults (useful for CI / bootstrap).
        #[arg(long)]
        accept_defaults: bool,
    },

    /// Parse + index a source tree. With `--embedder` set, also writes
    /// semantic vectors into `<root>/vectors.db`.
    Index {
        /// Source path to scan (defaults to `.`).
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Query the memory. With `--embedder` and/or `--synth` set, runs
    /// Mode::Full recall.
    Query {
        /// Maximum number of chunks to return.
        #[arg(short = 'k', long, default_value_t = 8)]
        top_k: usize,

        /// Query text. Joined with spaces if multiple words.
        #[arg(required = true)]
        text: Vec<String>,
    },

    /// Watch a source tree and re-index on change.
    Watch {
        /// Source path to watch (defaults to `.`).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Debounce window in milliseconds.
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },

    /// Inspect or edit memory files.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },

    /// Manage installed skills.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },

    /// Install, remove, or dispatch Claude Code hooks.
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },

    /// Register the Thoth MCP server in `settings.json`.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },

    /// One-shot: install skill + hooks + MCP server (the whole integration).
    /// Idempotent — safe to re-run.
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },

    /// One-shot: remove skill + hooks + MCP server.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },

    /// Run a precision@k evaluation over a gold query set (TOML).
    ///
    /// See `eval/gold.toml` for the expected schema.
    Eval {
        /// Path to a gold set TOML file.
        #[arg(long)]
        gold: PathBuf,

        /// Top-k considered "answered correctly" if any expected hit lands.
        #[arg(short = 'k', long, default_value_t = 8)]
        top_k: usize,
    },

    /// Domain memory: ingest business rules from remote sources.
    Domain {
        #[command(subcommand)]
        cmd: DomainCmd,
    },
}

#[derive(Subcommand, Debug)]
enum DomainCmd {
    /// Pull rules from a source and upsert snapshots under
    /// `<root>/domain/<context>/_remote/<source>/`.
    Sync {
        /// Which adapter to use.
        #[arg(long, value_enum)]
        source: DomainSource,

        /// Local directory for `--source file`.
        #[arg(long, required_if_eq("source", "file"))]
        from: Option<PathBuf>,

        /// Remote project / database id. Meaning depends on `--source`:
        /// Notion database id, Asana project gid, ... Ignored for `file`.
        #[arg(long)]
        project_id: Option<String>,

        /// Only pull rules modified since this RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,

        /// Per-sync cap on rules returned. Default 500.
        #[arg(long, default_value_t = 500)]
        max_items: usize,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum DomainSource {
    /// Local TOML directory — always available, useful for tests and bootstrap.
    File,
    /// Notion database (requires `--features notion` and `NOTION_TOKEN`).
    Notion,
    /// Asana project (requires `--features asana` and `ASANA_TOKEN`).
    Asana,
    /// NotebookLM (stub — see `docs/adr/0001-domain-memory.md`).
    Notebooklm,
}

#[derive(Subcommand, Debug)]
enum MemoryCmd {
    /// Print `MEMORY.md` and `LESSONS.md`.
    Show,
    /// Open `MEMORY.md` in `$EDITOR`.
    Edit,
    /// Append a new fact to `MEMORY.md`.
    Fact {
        /// Optional comma-separated tags (`--tags a,b,c`).
        #[arg(long)]
        tags: Option<String>,
        /// Fact text (joined with spaces).
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Append a new lesson to `LESSONS.md`.
    Lesson {
        /// Trigger pattern — when this lesson should fire.
        #[arg(long, required = true)]
        when: String,
        /// Advice / rule / warning (joined with spaces).
        #[arg(required = true)]
        advice: Vec<String>,
    },
    /// Run the forgetting pass (TTL + capacity eviction over the episodic log).
    Forget,
    /// Mode::Full: ask the synthesizer to critique recent outcomes and
    /// append any high-value proposed lessons to `LESSONS.md`.
    Nudge {
        /// How many recent episodes to consider. `0` uses the default (64).
        #[arg(long, default_value_t = 0)]
        window: usize,
    },
    /// List entries staged in `MEMORY.pending.md` / `LESSONS.pending.md`.
    Pending,
    /// Promote a staged fact or lesson into the canonical markdown file.
    Promote {
        /// `fact` or `lesson`.
        #[arg(value_parser = ["fact", "lesson"])]
        kind: String,
        /// 0-based index shown by `thoth memory pending`.
        index: usize,
    },
    /// Drop a staged fact or lesson without promoting it.
    Reject {
        /// `fact` or `lesson`.
        #[arg(value_parser = ["fact", "lesson"])]
        kind: String,
        /// 0-based index shown by `thoth memory pending`.
        index: usize,
        /// Optional reason — recorded in memory-history.jsonl.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Tail the memory-history.jsonl audit log.
    Log {
        /// Return only the latest N entries.
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    /// List installed skills.
    List,
    /// Install the bundled `thoth` skill so Claude Code can discover it.
    Install {
        /// Where to install. `project` drops it into `./.claude/skills/thoth/`,
        /// `user` drops it into `~/.claude/skills/thoth/`.
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
}

#[derive(Subcommand, Debug)]
enum McpCmd {
    /// Register `thoth-mcp` under `mcpServers.thoth` in `settings.json`.
    /// Idempotent.
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
    /// Remove the `mcpServers.thoth` entry. Other MCP servers are preserved.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
}

#[derive(Subcommand, Debug)]
enum HooksCmd {
    /// Install Thoth's Claude Code hook block into `settings.json`.
    Install {
        /// `project` writes to `./.claude/settings.json`; `user` to
        /// `~/.claude/settings.json`.
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
    /// Remove every hook whose command invokes `thoth hooks exec`.
    /// Leaves user-owned hooks untouched.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
    /// Runtime dispatcher — called by Claude Code itself with a JSON payload
    /// on stdin. Not intended for interactive use.
    Exec {
        #[arg(value_enum)]
        event: hooks::HookEvent,
    },
}

// --------------------------------------------------------------------- entry

/// Install a `tracing` subscriber tuned for a CLI.
///
/// By default, we're *silent* — only `error` propagates, so a clean run of
/// `thoth init` or `thoth index` shows only the commands own `println!`
/// output (no `INFO thoth_retrieve::indexer: …` noise). `-v` opens the tap
/// to `info`, `-vv` to `debug`, `-vvv` to `trace`. If `RUST_LOG` is set
/// *and* no `-v` flag was passed we honor it so power users keep their
/// usual workflow.
fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        1 => tracing_subscriber::EnvFilter::new("info"),
        2 => tracing_subscriber::EnvFilter::new("debug"),
        _ => tracing_subscriber::EnvFilter::new("trace"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.cmd {
        Cmd::Init => cmd_init(&cli.root).await?,
        Cmd::Setup {
            show,
            accept_defaults,
        } => setup::run(&cli.root, show, accept_defaults).await?,
        Cmd::Index { path } => cmd_index(&cli.root, &path, cli.embedder).await?,
        Cmd::Query { text, top_k } => {
            cmd_query(
                &cli.root,
                text.join(" "),
                top_k,
                cli.json,
                cli.embedder,
                cli.synth,
            )
            .await?
        }
        Cmd::Watch { path, debounce_ms } => {
            cmd_watch(
                &cli.root,
                &path,
                Duration::from_millis(debounce_ms),
                cli.embedder,
            )
            .await?
        }
        Cmd::Memory { cmd } => match cmd {
            MemoryCmd::Show => cmd_memory_show(&cli.root).await?,
            MemoryCmd::Edit => cmd_memory_edit(&cli.root).await?,
            MemoryCmd::Fact { tags, text } => {
                cmd_memory_fact(&cli.root, text.join(" "), tags).await?
            }
            MemoryCmd::Lesson { when, advice } => {
                cmd_memory_lesson(&cli.root, when, advice.join(" ")).await?
            }
            MemoryCmd::Forget => cmd_memory_forget(&cli.root, cli.json).await?,
            MemoryCmd::Nudge { window } => {
                cmd_memory_nudge(&cli.root, window, cli.json, cli.synth).await?
            }
            MemoryCmd::Pending => cmd_memory_pending(&cli.root, cli.json).await?,
            MemoryCmd::Promote { kind, index } => {
                cmd_memory_promote(&cli.root, &kind, index, cli.json).await?
            }
            MemoryCmd::Reject {
                kind,
                index,
                reason,
            } => cmd_memory_reject(&cli.root, &kind, index, reason.as_deref(), cli.json).await?,
            MemoryCmd::Log { limit } => cmd_memory_log(&cli.root, limit, cli.json).await?,
        },
        Cmd::Skills { cmd } => match cmd {
            SkillsCmd::List => cmd_skills_list(&cli.root, cli.json).await?,
            SkillsCmd::Install { scope } => hooks::skills_install(scope, &cli.root).await?,
        },
        Cmd::Hooks { cmd } => match cmd {
            HooksCmd::Install { scope } => hooks::install(scope).await?,
            HooksCmd::Uninstall { scope } => hooks::uninstall(scope).await?,
            HooksCmd::Exec { event } => hooks::exec(event, &cli.root).await?,
        },
        Cmd::Mcp { cmd } => match cmd {
            McpCmd::Install { scope } => hooks::mcp_install(scope, &cli.root).await?,
            McpCmd::Uninstall { scope } => hooks::mcp_uninstall(scope).await?,
        },
        Cmd::Install { scope } => hooks::install_all(scope, &cli.root).await?,
        Cmd::Uninstall { scope } => hooks::uninstall_all(scope, &cli.root).await?,
        Cmd::Eval { gold, top_k } => cmd_eval(&cli.root, &gold, top_k, cli.json).await?,
        Cmd::Domain { cmd } => match cmd {
            DomainCmd::Sync {
                source,
                from,
                project_id,
                since,
                max_items,
            } => {
                cmd_domain_sync(
                    &cli.root,
                    source,
                    from.as_deref(),
                    project_id.as_deref(),
                    since.as_deref(),
                    max_items,
                    cli.json,
                )
                .await?
            }
        },
    }

    Ok(())
}

// ------------------------------------------------------- provider constructors

/// Build an embedder from the CLI flag. Returns `Ok(None)` when no flag is
/// passed. Returns an error if the requested provider isn't compiled in.
fn build_embedder(kind: Option<EmbedderKind>) -> anyhow::Result<Option<Arc<dyn Embedder>>> {
    let Some(kind) = kind else {
        return Ok(None);
    };
    match kind {
        #[cfg(feature = "voyage")]
        EmbedderKind::Voyage => {
            let e = thoth_embed::voyage::VoyageEmbedder::from_env()?;
            Ok(Some(Arc::new(e)))
        }
        #[cfg(not(feature = "voyage"))]
        EmbedderKind::Voyage => Err(anyhow::anyhow!(
            "--embedder voyage requires `--features voyage` at build time"
        )),

        #[cfg(feature = "openai")]
        EmbedderKind::Openai => {
            let e = thoth_embed::openai::OpenAiEmbedder::from_env()?;
            Ok(Some(Arc::new(e)))
        }
        #[cfg(not(feature = "openai"))]
        EmbedderKind::Openai => Err(anyhow::anyhow!(
            "--embedder openai requires `--features openai` at build time"
        )),

        #[cfg(feature = "cohere")]
        EmbedderKind::Cohere => {
            let e = thoth_embed::cohere::CohereEmbedder::from_env()?;
            Ok(Some(Arc::new(e)))
        }
        #[cfg(not(feature = "cohere"))]
        EmbedderKind::Cohere => Err(anyhow::anyhow!(
            "--embedder cohere requires `--features cohere` at build time"
        )),
    }
}

/// Build a synthesizer from the CLI flag. Returns `Ok(None)` when no flag
/// is passed.
fn build_synth(kind: Option<SynthKind>) -> anyhow::Result<Option<Arc<dyn Synthesizer>>> {
    let Some(kind) = kind else {
        return Ok(None);
    };
    match kind {
        #[cfg(feature = "anthropic")]
        SynthKind::Anthropic => {
            let s = thoth_synth::anthropic::AnthropicSynthesizer::from_env()?;
            Ok(Some(Arc::new(s)))
        }
        #[cfg(not(feature = "anthropic"))]
        SynthKind::Anthropic => Err(anyhow::anyhow!(
            "--synth anthropic requires `--features anthropic` at build time"
        )),
    }
}

async fn open_vectors(store: &StoreRoot) -> anyhow::Result<VectorStore> {
    let path = StoreRoot::vectors_path(&store.path);
    Ok(VectorStore::open(&path).await?)
}

// -------------------------------------------------------------- subcommands

async fn cmd_init(root: &std::path::Path) -> anyhow::Result<()> {
    let existed = root.exists();
    let store = StoreRoot::open(root).await?;
    // Seed empty source-of-truth files so `memory show` isn't confused.
    let mut seeded = Vec::new();
    for name in ["MEMORY.md", "LESSONS.md"] {
        let p = store.path.join(name);
        if !p.exists() {
            tokio::fs::write(&p, format!("# {name}\n")).await?;
            seeded.push(name);
        }
    }
    // Scaffold a documented config.toml on first run. Every knob is
    // commented-out so the file documents defaults without overriding
    // them — users uncomment only what they want to change. Never
    // overwrite an existing config.
    let cfg_path = store.path.join("config.toml");
    if !cfg_path.exists() {
        tokio::fs::write(&cfg_path, DEFAULT_CONFIG_TOML).await?;
        seeded.push("config.toml");
    }
    let verb = if existed { "refreshed" } else { "created" };
    println!("✓ {verb} {}", store.path.display());
    if !seeded.is_empty() {
        println!("  seeded: {}", seeded.join(", "));
    }
    println!("  next:   thoth index .");
    Ok(())
}

/// Scaffold written on `thoth init`. Every setting is commented out so the
/// file only documents defaults — uncomment to override. Kept inline (not
/// a separate `include_str!` fixture) so the CLI binary has no external
/// runtime dependency on the repo layout.
const DEFAULT_CONFIG_TOML: &str = r#"# Thoth config. All fields are optional; defaults shown.
# Uncomment the ones you want to change.

[index]
# Gitignore-syntax patterns. Applied on top of `.gitignore`, `.ignore`, and
# any `.thothignore` found in the project. Supports re-including with `!`.
#
# ignore = [
#     "target/",
#     "node_modules/",
#     "dist/",
#     "build/",
#     "*.generated.rs",
#     "docs/internal/",
#     "!docs/internal/README.md",
# ]

# Max file size (bytes) considered for indexing. Files larger than this
# are skipped with a debug log. Default: 2 MiB.
# max_file_size = 2097152

# Descend into hidden dirs (e.g. `.github`). Default: false.
# include_hidden = false

# Follow symlinks. Default: false — prevents indexing sibling projects.
# follow_symlinks = false

[memory]
# How many days an episode survives before TTL eviction. Default: 30.
# episodic_ttl_days = 30

# Hard cap on episode count before capacity-based eviction. Default: 50_000.
# max_episodes = 50000

# Lessons with a success ratio below this floor (and at least
# `lesson_min_attempts` attempts) are dropped by the forget pass.
# lesson_floor = 0.2
# lesson_min_attempts = 3

# Exponential decay rate per day for the retention score, and the floor
# below which an episode is dropped. Set `decay_floor = 0.0` to disable
# decay-based eviction entirely (Mode::Zero deterministic).
# decay_lambda = 0.02
# decay_floor  = 0.05

# Whether to run the LLM nudge at session end (Mode::Full only).
# enable_nudge = true
"#;

async fn cmd_index(
    root: &std::path::Path,
    src: &std::path::Path,
    embedder_kind: Option<EmbedderKind>,
) -> anyhow::Result<()> {
    // Try forwarding to a running MCP daemon first (avoids redb lock).
    // Note: `--embedder` is silently ignored here because the running
    // daemon was configured at launch time and we can't retrofit an
    // embedder into it from the outside. Cold indexing (no daemon)
    // honours `--embedder` as before.
    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d
            .call(
                "thoth_index",
                serde_json::json!({ "path": src.to_string_lossy() }),
            )
            .await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        println!("{}", daemon::tool_text(&result));
        return Ok(());
    }

    let store = StoreRoot::open(root).await?;
    // Honour `[index]` in `<root>/config.toml` — ignore patterns, max file
    // size, hidden-dir / symlink toggles. Missing file → defaults.
    let cfg = thoth_retrieve::IndexConfig::load_or_default(root).await;
    let mut idx = Indexer::new(store.clone(), LanguageRegistry::new()).with_config(&cfg);
    if let Some(embedder) = build_embedder(embedder_kind)? {
        let vectors = open_vectors(&store).await?;
        idx = idx.with_embedding(embedder, vectors);
    }
    idx = idx.with_progress(make_progress_bar());

    let stats = idx.index_path(src).await?;
    println!("✓ indexed {}", src.display());
    println!(
        "  {} files · {} chunks · {} symbols · {} calls · {} imports",
        stats.files, stats.chunks, stats.symbols, stats.calls, stats.imports,
    );
    if stats.embedded > 0 {
        println!("  {} chunks embedded", stats.embedded);
    }
    Ok(())
}

/// Build a closure that drives an `indicatif::ProgressBar` from
/// [`IndexProgress`] events. The bar is lazily allocated on the first `walk`
/// event so the total is known, and finished when the commit stage fires.
fn make_progress_bar() -> impl for<'a> Fn(IndexProgress<'a>) + Send + Sync + 'static {
    use std::sync::Mutex;
    let bar: Mutex<Option<ProgressBar>> = Mutex::new(None);
    move |ev: IndexProgress<'_>| {
        let mut slot = bar.lock().unwrap();
        match ev.stage {
            "walk" => {
                let pb = ProgressBar::new(ev.total as u64);
                // Template: [00:12] [#######>---] 42/128 path/to/file.rs
                let style = ProgressStyle::with_template(
                    "{elapsed_precise} [{bar:30.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-");
                pb.set_style(style);
                *slot = Some(pb);
            }
            "file" => {
                if let Some(pb) = slot.as_ref() {
                    pb.set_position(ev.done as u64);
                    if let Some(p) = ev.path {
                        pb.set_message(p.display().to_string());
                    }
                }
            }
            "embed" => {
                if let Some(pb) = slot.as_ref() {
                    // First embed event resets the bar to chunk-scale.
                    if ev.done == 0 {
                        pb.set_length(ev.total as u64);
                        pb.set_position(0);
                        pb.set_message("embedding chunks");
                    } else {
                        pb.set_position(ev.done as u64);
                    }
                }
            }
            "commit" => {
                if let Some(pb) = slot.take() {
                    pb.set_message("committing…");
                    pb.finish_and_clear();
                }
            }
            _ => {}
        }
    }
}

async fn cmd_query(
    root: &std::path::Path,
    text: String,
    top_k: usize,
    json: bool,
    embedder_kind: Option<EmbedderKind>,
    synth_kind: Option<SynthKind>,
) -> anyhow::Result<()> {
    // Try forwarding to a running MCP daemon first (avoids redb lock).
    // The daemon always runs in Mode::Zero (no embedder/synth), so
    // `--embedder` / `--synth` force a direct-store fallback below.
    let wants_full = embedder_kind.is_some() || synth_kind.is_some();
    if !wants_full && let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d
            .call(
                "thoth_recall",
                serde_json::json!({ "query": text, "top_k": top_k }),
            )
            .await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        if json {
            // The daemon's `data` is a full `Retrieval` — same shape
            // as the direct path below.
            println!(
                "{}",
                serde_json::to_string_pretty(&daemon::tool_data(&result))?
            );
        } else {
            println!("{}", daemon::tool_text(&result));
        }
        return Ok(());
    }

    let store = StoreRoot::open(root).await?;

    let embedder = build_embedder(embedder_kind)?;
    let synth = build_synth(synth_kind)?;
    let is_full = embedder.is_some() || synth.is_some();

    let vectors = if embedder.is_some() {
        Some(open_vectors(&store).await?)
    } else {
        None
    };

    let r = if is_full {
        Retriever::with_full(store, vectors, embedder, synth)
    } else {
        Retriever::new(store)
    };

    let q = Query {
        text,
        top_k,
        ..Query::text("")
    };
    let out = if is_full {
        r.recall_full(&q).await?
    } else {
        r.recall(&q).await?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    print!("{}", out.render());
    Ok(())
}

async fn cmd_watch(
    root: &std::path::Path,
    src: &std::path::Path,
    debounce: Duration,
    embedder_kind: Option<EmbedderKind>,
) -> anyhow::Result<()> {
    // `watch` is long-running and needs its own Indexer + redb handle. We
    // can't multiplex that through the daemon's socket (each request is a
    // short-lived call). So if the daemon is up — meaning the store is
    // locked — fail fast with a useful message rather than exploding on
    // `StoreRoot::open`.
    if daemon::DaemonClient::try_connect(root).await.is_some() {
        anyhow::bail!(
            "thoth-mcp daemon is running on {}; stop it before running `thoth watch` \
             (they would fight for the redb exclusive lock). Either close Claude Code \
             or run the re-index through the daemon with `thoth index .`.",
            root.display()
        );
    }

    let store = StoreRoot::open(root).await?;
    let cfg = thoth_retrieve::IndexConfig::load_or_default(root).await;
    let mut idx = Indexer::new(store.clone(), LanguageRegistry::new()).with_config(&cfg);
    if let Some(embedder) = build_embedder(embedder_kind)? {
        let vectors = open_vectors(&store).await?;
        idx = idx.with_embedding(embedder, vectors);
    }
    idx = idx.with_progress(make_progress_bar());

    // Do an initial full index so subsequent deltas matter.
    let stats = idx.index_path(src).await?;
    println!(
        "✓ initial index: {} files · {} chunks · {} symbols",
        stats.files, stats.chunks, stats.symbols,
    );

    let mut w = Watcher::watch(src, 1024)?;
    println!("… watching {} (ctrl-c to stop)", src.display());

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n✓ stopped");
                break;
            }
            ev = w.recv() => {
                let Some(ev) = ev else {
                    warn!("watcher channel closed");
                    break;
                };
                // Simple debounce: after the first event, drain anything that
                // arrives within `debounce` then batch-reindex affected files.
                let mut batch = vec![ev];
                let deadline = tokio::time::Instant::now() + debounce;
                while let Ok(Some(extra)) = tokio::time::timeout_at(deadline, w.recv()).await {
                    batch.push(extra);
                }

                // Split into change vs. delete sets so deletions only
                // purge (no reparse on a missing file).
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
                    if let Err(e) = idx.purge_path(&path).await {
                        warn!(?path, error = %e, "purge failed");
                    }
                }
                for path in changed {
                    if let Err(e) = idx.index_file(&path).await {
                        warn!(?path, error = %e, "re-index failed");
                    }
                }

                // Flush BM25 writes so the next `query` (or hook pull) sees
                // them — both deletes and adds need to be committed.
                if changed_n + deleted_n > 0 {
                    if let Err(e) = idx.commit().await {
                        warn!(error = %e, "fts commit failed");
                    }
                    if changed_n > 0 {
                        println!(
                            "  ↻ reindexed {changed_n} file{}",
                            if changed_n == 1 { "" } else { "s" }
                        );
                    }
                    if deleted_n > 0 {
                        println!(
                            "  🗑 purged {deleted_n} file{}",
                            if deleted_n == 1 { "" } else { "s" }
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

async fn cmd_memory_show(root: &std::path::Path) -> anyhow::Result<()> {
    // `memory show` is pure-filesystem (no redb) so strictly it doesn't
    // need the daemon to avoid lock conflicts. We still prefer the daemon
    // when available because the MCP server is the single writer — reading
    // through it guarantees we see the same view Claude Code sees.
    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d.call("thoth_memory_show", serde_json::json!({})).await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        println!("{}", daemon::tool_text(&result));
        return Ok(());
    }

    // No daemon — read the files directly. We deliberately do NOT call
    // `StoreRoot::open` here: that would acquire the redb lock just to
    // read two markdown files, and collide with a daemon that raced us.
    for name in ["MEMORY.md", "LESSONS.md"] {
        let p = root.join(name);
        println!("─── {name} ───");
        match tokio::fs::read_to_string(&p).await {
            Ok(s) => println!("{s}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("(not found)");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn cmd_memory_edit(root: &std::path::Path) -> anyhow::Result<()> {
    // `memory edit` only touches MEMORY.md on disk — no redb access needed.
    // We intentionally skip `StoreRoot::open` so it can run even when the
    // MCP daemon owns the database lock.
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    // Ensure the root exists; otherwise the editor would create the file
    // in a non-existent parent and fail confusingly.
    if !root.exists() {
        anyhow::bail!("{} not found — run `thoth init` first", root.display());
    }
    let path = root.join("MEMORY.md");
    if !path.exists() {
        tokio::fs::write(&path, "# MEMORY.md\n").await?;
    }
    let status = tokio::process::Command::new(&editor)
        .arg(&path)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(())
}

async fn cmd_memory_fact(
    root: &std::path::Path,
    text: String,
    tags: Option<String>,
) -> anyhow::Result<()> {
    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("fact text must not be empty");
    }
    let tags = tags
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d
            .call(
                "thoth_remember_fact",
                serde_json::json!({ "text": text, "tags": tags }),
            )
            .await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        println!("{}", daemon::tool_text(&result));
        return Ok(());
    }

    let store = StoreRoot::open(root).await?;
    let fact = Fact {
        meta: MemoryMeta::new(MemoryKind::Semantic),
        text,
        tags,
    };
    store.markdown.append_fact(&fact).await?;
    println!(
        "fact appended to {}",
        store.path.join("MEMORY.md").display()
    );
    Ok(())
}

async fn cmd_memory_lesson(
    root: &std::path::Path,
    when: String,
    advice: String,
) -> anyhow::Result<()> {
    let when = when.trim().to_string();
    let advice = advice.trim().to_string();
    if when.is_empty() || advice.is_empty() {
        anyhow::bail!("both --when and advice text must be non-empty");
    }

    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d
            .call(
                "thoth_remember_lesson",
                serde_json::json!({ "trigger": when, "advice": advice }),
            )
            .await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        println!("{}", daemon::tool_text(&result));
        return Ok(());
    }

    let store = StoreRoot::open(root).await?;
    let lesson = Lesson {
        meta: MemoryMeta::new(MemoryKind::Reflective),
        trigger: when,
        advice,
        success_count: 0,
        failure_count: 0,
    };
    store.markdown.append_lesson(&lesson).await?;
    println!(
        "lesson appended to {}",
        store.path.join("LESSONS.md").display()
    );
    Ok(())
}

async fn cmd_memory_forget(root: &std::path::Path, json: bool) -> anyhow::Result<()> {
    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d.call("thoth_memory_forget", serde_json::json!({})).await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&daemon::tool_data(&result))?
            );
        } else {
            println!("{}", daemon::tool_text(&result));
        }
        return Ok(());
    }

    let mm = MemoryManager::open(root).await?;
    let report = mm.forget_pass().await?;
    if json {
        let v = serde_json::json!({
            "episodes_ttl": report.episodes_ttl,
            "episodes_cap": report.episodes_cap,
            "lessons_dropped": report.lessons_dropped,
            "lessons_quarantined": report.lessons_quarantined,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "forget pass: episodes_ttl={} episodes_cap={} lessons_dropped={} lessons_quarantined={}",
            report.episodes_ttl,
            report.episodes_cap,
            report.lessons_dropped,
            report.lessons_quarantined
        );
    }
    Ok(())
}

async fn cmd_memory_pending(root: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let md = MarkdownStore::open(root).await?;
    let facts = md.read_pending_facts().await?;
    let lessons = md.read_pending_lessons().await?;
    if json {
        let v = serde_json::json!({
            "facts": facts.iter().enumerate().map(|(i, f)| {
                serde_json::json!({ "index": i, "text": f.text, "tags": f.tags })
            }).collect::<Vec<_>>(),
            "lessons": lessons.iter().enumerate().map(|(i, l)| {
                serde_json::json!({ "index": i, "trigger": l.trigger, "advice": l.advice })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("── pending facts ({}) ──", facts.len());
        for (i, f) in facts.iter().enumerate() {
            let first = f.text.lines().next().unwrap_or("").trim();
            println!("[{i}] {first}");
        }
        println!("\n── pending lessons ({}) ──", lessons.len());
        for (i, l) in lessons.iter().enumerate() {
            println!("[{i}] {}", l.trigger);
        }
        if facts.is_empty() && lessons.is_empty() {
            println!("(no pending entries)");
        }
    }
    Ok(())
}

async fn cmd_memory_promote(
    root: &std::path::Path,
    kind: &str,
    index: usize,
    json: bool,
) -> anyhow::Result<()> {
    let md = MarkdownStore::open(root).await?;
    let (title, ok) = match kind {
        "fact" => match md.promote_pending_fact(index).await? {
            Some(f) => (f.text.lines().next().unwrap_or("").trim().to_string(), true),
            None => (String::new(), false),
        },
        "lesson" => match md.promote_pending_lesson(index).await? {
            Some(l) => (l.trigger, true),
            None => (String::new(), false),
        },
        other => anyhow::bail!("unknown kind: {other} (expected `fact` or `lesson`)"),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "kind": kind,
                "index": index,
                "promoted": ok,
                "title": title,
            }))?
        );
    } else if ok {
        println!("promoted {kind} [{index}]: {title}");
    } else {
        println!("no pending {kind} at index {index}");
    }
    Ok(())
}

async fn cmd_memory_reject(
    root: &std::path::Path,
    kind: &str,
    index: usize,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let md = MarkdownStore::open(root).await?;
    let (title, ok) = match kind {
        "fact" => match md.reject_pending_fact(index, reason).await? {
            Some(f) => (f.text.lines().next().unwrap_or("").trim().to_string(), true),
            None => (String::new(), false),
        },
        "lesson" => match md.reject_pending_lesson(index, reason).await? {
            Some(l) => (l.trigger, true),
            None => (String::new(), false),
        },
        other => anyhow::bail!("unknown kind: {other} (expected `fact` or `lesson`)"),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "kind": kind,
                "index": index,
                "rejected": ok,
                "title": title,
                "reason": reason,
            }))?
        );
    } else if ok {
        println!("rejected {kind} [{index}]: {title}");
    } else {
        println!("no pending {kind} at index {index}");
    }
    Ok(())
}

async fn cmd_memory_log(
    root: &std::path::Path,
    limit: Option<usize>,
    json: bool,
) -> anyhow::Result<()> {
    let md = MarkdownStore::open(root).await?;
    let mut entries = md.read_history().await?;
    if let Some(n) = limit
        && entries.len() > n
    {
        let skip = entries.len() - n;
        entries.drain(..skip);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for e in &entries {
            println!("{}  {:<14} {:<7} {}", e.at_rfc3339, e.op, e.kind, e.title);
        }
        if entries.is_empty() {
            println!("(no history yet)");
        }
    }
    Ok(())
}

async fn cmd_memory_nudge(
    root: &std::path::Path,
    window: usize,
    json: bool,
    synth_kind: Option<SynthKind>,
) -> anyhow::Result<()> {
    let Some(synth) = build_synth(synth_kind)? else {
        anyhow::bail!(
            "`thoth memory nudge` requires --synth <provider> (and the matching feature)"
        );
    };
    let mm = MemoryManager::open(root).await?;
    let report = mm.nudge(synth.as_ref(), window).await?;
    if json {
        let v = serde_json::json!({
            "facts_added": report.facts_added,
            "lessons_added": report.lessons_added,
            "skills_added": report.skills_added,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "nudge: facts_added={} lessons_added={} skills_added={}",
            report.facts_added, report.lessons_added, report.skills_added
        );
    }
    Ok(())
}

/// Gold set file schema (TOML):
///
/// ```toml
/// [[query]]
/// q = "hybrid recall RRF fusion"
/// # any of: substring matches against the *path* of returned chunks…
/// expect_path  = ["retrieve/src/hybrid"]
/// # …or against the rendered preview/body.
/// expect_text  = ["reciprocal rank fusion"]
/// ```
#[derive(Debug, serde::Deserialize)]
struct GoldSet {
    query: Vec<GoldQuery>,
}

#[derive(Debug, serde::Deserialize)]
struct GoldQuery {
    q: String,
    #[serde(default)]
    expect_path: Vec<String>,
    #[serde(default)]
    expect_text: Vec<String>,
}

async fn cmd_eval(
    root: &std::path::Path,
    gold_path: &std::path::Path,
    top_k: usize,
    json: bool,
) -> anyhow::Result<()> {
    let raw = tokio::fs::read_to_string(gold_path).await?;
    let gold: GoldSet = toml::from_str(&raw)?;
    if gold.query.is_empty() {
        anyhow::bail!("gold set is empty");
    }

    // Prefer the running daemon so `thoth eval` works while Claude Code
    // holds the redb lock. Each gold query becomes one `thoth_recall`
    // call; the structured `data` half of `ToolOutput` is the same
    // `Retrieval` shape we'd get from the direct path below.
    let daemon_client = daemon::DaemonClient::try_connect(root).await;
    let direct = if daemon_client.is_some() {
        None
    } else {
        let store = StoreRoot::open(root).await?;
        Some(Retriever::new(store))
    };

    let mut hits = 0usize;
    let total = gold.query.len();
    let mut per_query = Vec::with_capacity(total);

    // One shared client across the loop — cheap, and reusing the
    // connection avoids a connect-per-query round trip.
    let mut client = daemon_client;

    for gq in &gold.query {
        let out: thoth_core::Retrieval = if let Some(c) = client.as_mut() {
            let result = c
                .call(
                    "thoth_recall",
                    serde_json::json!({ "query": gq.q, "top_k": top_k }),
                )
                .await?;
            if daemon::tool_is_error(&result) {
                anyhow::bail!("{}", daemon::tool_text(&result));
            }
            serde_json::from_value(daemon::tool_data(&result))?
        } else {
            direct
                .as_ref()
                .unwrap()
                .recall(&Query {
                    text: gq.q.clone(),
                    top_k,
                    ..Query::text("")
                })
                .await?
        };

        let got = out.chunks.iter().any(|c| {
            let p = c.path.to_string_lossy().to_lowercase();
            // Match against the full body too — previews are just a few
            // lines, which is often too narrow for a keyword probe.
            let body = format!("{} {}", c.preview, c.body).to_lowercase();
            let path_ok = gq.expect_path.is_empty()
                || gq.expect_path.iter().any(|s| p.contains(&s.to_lowercase()));
            let text_ok = gq.expect_text.is_empty()
                || gq
                    .expect_text
                    .iter()
                    .any(|s| body.contains(&s.to_lowercase()));
            // Any-of semantics across the two expect_* buckets — if *either*
            // bucket matches we count the query as answered.
            (!gq.expect_path.is_empty() && path_ok) || (!gq.expect_text.is_empty() && text_ok)
        });

        if got {
            hits += 1;
        }
        per_query.push((gq.q.clone(), got, out.chunks.len()));
    }

    let p_at_k = hits as f64 / total as f64;

    if json {
        let v = serde_json::json!({
            "top_k": top_k,
            "total": total,
            "hits": hits,
            "precision_at_k": p_at_k,
            "queries": per_query
                .iter()
                .map(|(q, ok, n)| serde_json::json!({"q": q, "hit": ok, "returned": n}))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    for (q, ok, n) in &per_query {
        let mark = if *ok { "✓" } else { "✗" };
        println!("{mark}  [{n:>2}]  {q}");
    }
    println!("\nprecision@{} = {}/{} = {:.3}", top_k, hits, total, p_at_k);
    if hits < total {
        // Non-zero exit so CI can gate on eval regressions.
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_skills_list(root: &std::path::Path, json: bool) -> anyhow::Result<()> {
    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d.call("thoth_skills_list", serde_json::json!({})).await?;
        if daemon::tool_is_error(&result) {
            anyhow::bail!("{}", daemon::tool_text(&result));
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&daemon::tool_data(&result))?
            );
        } else {
            // `text` already handles the empty-list message for us.
            print!("{}", daemon::tool_text(&result));
        }
        return Ok(());
    }

    let store = StoreRoot::open(root).await?;
    let skills = store.markdown.list_skills().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&skills)?);
        return Ok(());
    }
    if skills.is_empty() {
        println!(
            "(no skills installed — drop a folder into {}/skills/)",
            store.path.display()
        );
        return Ok(());
    }
    for s in skills {
        println!("{:<28}  {}", s.slug, s.description);
    }
    Ok(())
}

// -------------------------------------------------------- domain subcommand

async fn cmd_domain_sync(
    root: &std::path::Path,
    source: DomainSource,
    from: Option<&std::path::Path>,
    project_id: Option<&str>,
    since: Option<&str>,
    max_items: usize,
    json: bool,
) -> anyhow::Result<()> {
    use thoth_domain::{IngestFilter, SnapshotStore, file::FileIngestor, sync_source};

    tokio::fs::create_dir_all(root).await?;
    let snap = SnapshotStore::new(root);

    let filter = IngestFilter {
        since: since
            .map(|s| {
                time::OffsetDateTime::parse(
                    s,
                    &time::format_description::well_known::Rfc3339,
                )
            })
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid --since: {e}"))?,
        max_items,
    };

    let ingestor: Arc<dyn thoth_domain::DomainIngestor> = match source {
        DomainSource::File => {
            let dir = from.ok_or_else(|| anyhow::anyhow!("--from required for --source file"))?;
            Arc::new(FileIngestor::new(dir))
        }
        #[cfg(feature = "notion")]
        DomainSource::Notion => {
            let db = project_id
                .ok_or_else(|| anyhow::anyhow!("--project-id required for --source notion"))?;
            Arc::new(thoth_domain::notion::NotionIngestor::new(db)?)
        }
        #[cfg(not(feature = "notion"))]
        DomainSource::Notion => {
            anyhow::bail!("--source notion requires `--features notion` at build time")
        }
        #[cfg(feature = "asana")]
        DomainSource::Asana => {
            let gid = project_id
                .ok_or_else(|| anyhow::anyhow!("--project-id required for --source asana"))?;
            Arc::new(thoth_domain::asana::AsanaIngestor::new(gid)?)
        }
        #[cfg(not(feature = "asana"))]
        DomainSource::Asana => {
            anyhow::bail!("--source asana requires `--features asana` at build time")
        }
        #[cfg(feature = "notebooklm")]
        DomainSource::Notebooklm => Arc::new(thoth_domain::notebooklm::NotebookLmIngestor::new()?),
        #[cfg(not(feature = "notebooklm"))]
        DomainSource::Notebooklm => anyhow::bail!(
            "--source notebooklm requires `--features notebooklm` at build time (stub)"
        ),
    };
    // `project_id` is only consumed by remote adapters — silence the unused
    // warning for builds without those features.
    let _ = project_id;

    let rep = sync_source(ingestor, &snap, &filter).await?;

    if json {
        let payload = serde_json::json!({
            "source": rep.source,
            "created": rep.stats.created,
            "updated": rep.stats.updated,
            "unchanged": rep.stats.unchanged,
            "unmapped": rep.stats.unmapped,
            "redacted": rep.stats.redacted,
            "errors": rep.errors.iter()
                .map(|(id, e)| serde_json::json!({"id": id, "error": e}))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print!("{rep}");
    }

    Ok(())
}
