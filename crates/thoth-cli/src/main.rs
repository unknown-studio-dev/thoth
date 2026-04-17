//! The `thoth` command-line interface.
//!
//! Primary surface — what a user actually runs:
//!
//! ```text
//! thoth setup                       # one-shot bootstrap (config + install)
//! thoth setup --status              # show what's already configured
//! thoth index [PATH]                # walk + parse + index (optionally embed)
//! thoth query <TEXT>                # hybrid recall (Mode::Zero by default)
//! thoth watch [PATH]                # stay resident, reindex on change
//! thoth memory show                 # cat MEMORY.md + LESSONS.md
//! thoth memory edit                 # $EDITOR on MEMORY.md
//! thoth memory fact <TEXT>          # append a fact
//! thoth memory lesson <WHEN> <DO>   # append a lesson
//! thoth memory forget               # run TTL + capacity eviction pass
//! thoth memory pending              # list staged facts/lessons
//! thoth memory promote <kind> <ix>  # promote a staged entry
//! thoth memory reject  <kind> <ix>  # drop a staged entry
//! thoth memory log                  # tail memory-history.jsonl
//! thoth skills list                 # list installed skills
//! thoth uninstall                   # remove the Claude Code integration
//! ```
//!
//! Primitives (hidden from `--help`; `thoth setup` drives them for you):
//!
//! ```text
//! thoth init                        # just create .thoth/ + seed markdown
//! thoth install                     # just wire the integration
//! thoth hooks  {install|uninstall|exec}
//! thoth mcp    {install|uninstall}
//! thoth skills install
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
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use thoth_core::{Embedder, Fact, Lesson, MemoryKind, MemoryMeta, Query, Synthesizer};
use thoth_memory::MemoryManager;
use thoth_parse::{LanguageRegistry, watch::Watcher};
use thoth_retrieve::{IndexProgress, Indexer, RetrieveConfig, Retriever};
use thoth_store::markdown::MarkdownStore;
use thoth_store::{StoreRoot, VectorStore};
use tracing::warn;

mod daemon;
mod hooks;
mod review;
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

/// Which retrieval mode(s) `thoth eval` should exercise. `both` runs the
/// same gold set under Mode::Zero and Mode::Full so the ablation between
/// lexical-only and vector/synth-augmented recall is directly comparable.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
enum EvalMode {
    /// Lexical-only recall (BM25 + symbol + graph + markdown, RRF-fused).
    #[default]
    Zero,
    /// Full recall: also runs the vector stage and (if `--synth` is set) the
    /// synthesizer. Requires `--embedder` and/or `--synth`.
    Full,
    /// Run the gold set under both modes and print a side-by-side report.
    Both,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// One-shot bootstrap — run this first. Walks a short wizard, writes
    /// `<root>/config.toml`, seeds MEMORY.md + LESSONS.md, and installs
    /// hooks + skills + MCP into `.claude/settings.json`. Re-run any time
    /// to reconfigure or self-heal the install.
    Setup {
        /// Show the detected install state and exit. Does not modify
        /// anything.
        #[arg(long)]
        status: bool,
        /// Skip prompts and write defaults (for CI / scripted bootstrap).
        #[arg(long, alias = "accept-defaults")]
        yes: bool,
    },

    /// Initialize a bare `.thoth/` directory (seed `MEMORY.md`,
    /// `LESSONS.md`, default `config.toml`). Prefer `thoth setup` for
    /// interactive users; this is kept as a primitive for scripts.
    #[command(hide = true)]
    Init,

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

    /// Install, remove, or dispatch Claude Code hooks. Hidden primitive —
    /// prefer `thoth setup` / `thoth uninstall`.
    #[command(hide = true)]
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },

    /// Register the Thoth MCP server in `settings.json`. Hidden
    /// primitive — prefer `thoth setup` / `thoth uninstall`.
    #[command(hide = true)]
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },

    /// Install skill + hooks + MCP server in one go. Hidden primitive —
    /// `thoth setup` already does this as part of the bootstrap.
    #[command(hide = true)]
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },

    /// Remove skill + hooks + MCP server from `settings.json`.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },

    /// Run a precision@k evaluation over a gold query set (TOML).
    ///
    /// Reports P@k, MRR, and per-query latency (p50 / p95). With
    /// `--mode both` the same gold set runs through Mode::Zero and
    /// Mode::Full side-by-side so you can see what the vector / synth
    /// stages buy you.
    ///
    /// See `eval/gold.toml` for the expected schema.
    Eval {
        /// Path to a gold set TOML file.
        #[arg(long)]
        gold: PathBuf,

        /// Top-k considered "answered correctly" if any expected hit lands.
        #[arg(short = 'k', long, default_value_t = 8)]
        top_k: usize,

        /// Which retrieval mode(s) to evaluate. `full` and `both` require
        /// `--embedder` and/or `--synth` (and a clean redb lock — stop the
        /// daemon first).
        #[arg(long, value_enum, default_value_t = EvalMode::Zero)]
        mode: EvalMode,
    },

    /// Domain memory: ingest business rules from remote sources.
    Domain {
        #[command(subcommand)]
        cmd: DomainCmd,
    },

    /// Blast-radius analysis. Given an FQN, walk the graph to find every
    /// symbol reachable within `--depth` steps. Use this to answer
    /// "what breaks if I change X?" (default, `--direction up`) or
    /// "what does X depend on?" (`--direction down`).
    Impact {
        /// Fully qualified name (`module::symbol`). Match the exact FQN
        /// that appears in `thoth_recall` output.
        #[arg(required = true)]
        fqn: String,

        /// BFS direction. `up` = callers / references / subtypes;
        /// `down` = callees / parent types; `both` = union.
        #[arg(long, value_enum, default_value_t = ImpactDir::Up)]
        direction: ImpactDir,

        /// Maximum BFS depth. Clamped to `[1, 8]` server-side.
        #[arg(short = 'd', long, default_value_t = 3)]
        depth: usize,
    },

    /// 360-degree context for a single symbol: callers, callees, extends,
    /// extended_by, references, siblings, and unresolved imports. Use
    /// after a `thoth_recall` once you've picked a specific FQN to drill
    /// into.
    Context {
        /// Fully qualified name (`module::symbol`).
        #[arg(required = true)]
        fqn: String,

        /// Per-section cap on returned neighbours.
        #[arg(long, default_value_t = 32)]
        limit: usize,
    },

    /// Change-impact analysis over a unified diff. Reads the diff from
    /// `--from` (file or `-` for stdin) or runs `git diff` in the
    /// working tree.
    Changes {
        /// Path to a diff file, or `-` for stdin. Omit to run
        /// `git diff HEAD` inside the current working tree.
        #[arg(long)]
        from: Option<String>,

        /// Blast-radius depth for the upstream walk.
        #[arg(short = 'd', long, default_value_t = 2)]
        depth: usize,
    },

    /// Maintenance pass over memory. Runs the forget pass (TTL +
    /// capacity + quarantine), reports reflection debt, and — with
    /// `--quiet` — stays silent unless something actionable turned
    /// up. Also invoked by the `SessionStart` hook so findings land
    /// in the agent's first context injection.
    Curate {
        /// Hide output when there's nothing to flag. Used by the
        /// SessionStart hook so a clean session emits no banner noise.
        #[arg(long)]
        quiet: bool,
    },

    /// Run a background memory review. Spawns an LLM call (via
    /// `claude` CLI or the Anthropic API) to analyze the session's
    /// recent activity and auto-persist durable facts, lessons, and
    /// skill proposals. Normally triggered automatically by the
    /// PostToolUse hook when `background_review = true` in config.
    Review {
        /// Backend for the LLM call: `auto` (default), `cli`, or `api`.
        #[arg(long, default_value = "auto")]
        backend: String,
    },
}

/// CLI-facing subset of [`thoth_graph::BlastDir`] so clap can derive
/// ValueEnum without leaking the dependency across crate boundaries.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ImpactDir {
    /// Reverse edges — who depends on this symbol.
    Up,
    /// Forward edges — what this symbol depends on.
    Down,
    /// Union of both directions.
    Both,
}

impl ImpactDir {
    fn as_str(self) -> &'static str {
        match self {
            ImpactDir::Up => "up",
            ImpactDir::Down => "down",
            ImpactDir::Both => "both",
        }
    }
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
    /// Install skills into `.claude/skills/`.
    ///
    /// With no `PATH`: installs the bundled skills (`memory-discipline`,
    /// `thoth-reflect`, `thoth-guide`, `thoth-exploring`, `thoth-debugging`,
    /// `thoth-impact-analysis`, `thoth-refactoring`, `thoth-cli`) — this is
    /// the primitive `thoth setup` drives.
    ///
    /// With a `PATH` pointing at a `<slug>.draft/` directory (produced by
    /// the agent's `thoth_skill_propose` MCP tool): promotes the draft
    /// into a live skill Claude Code will load on the next session, then
    /// removes the draft.
    Install {
        /// Path to a `<slug>.draft/` skill directory to promote. If
        /// omitted, the bundled skills are installed instead.
        path: Option<PathBuf>,
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
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
    /// Remove thoth-managed hooks from `settings.json`. Leaves user-owned
    /// hooks untouched.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: hooks::Scope,
    },
    /// Runtime dispatcher — called by Claude Code itself with a JSON
    /// payload on stdin. Not intended for interactive use.
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
        Cmd::Setup { status, yes } => setup::run(&cli.root, status, yes).await?,
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
            SkillsCmd::Install { path, scope } => match path {
                Some(p) => hooks::promote_skill_draft(scope, &cli.root, &p).await?,
                None => hooks::skills_install(scope, &cli.root).await?,
            },
        },
        Cmd::Hooks { cmd } => match cmd {
            HooksCmd::Install { scope } => hooks::install(scope, &cli.root).await?,
            HooksCmd::Uninstall { scope } => hooks::uninstall(scope).await?,
            HooksCmd::Exec { event } => hooks::exec(event, &cli.root).await?,
        },
        Cmd::Mcp { cmd } => match cmd {
            McpCmd::Install { scope } => hooks::mcp_install(scope, &cli.root).await?,
            McpCmd::Uninstall { scope } => hooks::mcp_uninstall(scope).await?,
        },
        Cmd::Install { scope } => hooks::install_all(scope, &cli.root).await?,
        Cmd::Uninstall { scope } => hooks::uninstall_all(scope, &cli.root).await?,
        Cmd::Eval {
            gold,
            top_k,
            mode,
        } => {
            cmd_eval(
                &cli.root,
                &gold,
                top_k,
                mode,
                cli.embedder,
                cli.synth,
                cli.json,
            )
            .await?
        }
        Cmd::Impact {
            fqn,
            direction,
            depth,
        } => cmd_impact(&cli.root, &fqn, direction, depth, cli.json).await?,
        Cmd::Context { fqn, limit } => cmd_context(&cli.root, &fqn, limit, cli.json).await?,
        Cmd::Changes { from, depth } => {
            cmd_changes(&cli.root, from.as_deref(), depth, cli.json).await?
        }
        Cmd::Curate { quiet } => cmd_curate(&cli.root, quiet).await?,
        Cmd::Review { backend } => cmd_review(&cli.root, &backend).await?,
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

[discipline]
# Gate master switch — set `false` to disable discipline entirely.
# nudge_before_write = true

# Gate verdict on a miss:
#   "off"    — disable the gate (pass silently).
#   "nudge"  — pass + stderr warning. [default]
#   "strict" — block.
# mode = "nudge"

# Recency shortcut — recall within this window passes without a relevance
# check. Keep short; long windows enable ritual recall.
# gate_window_short_secs = 60

# Relevance pool — how far back the gate looks for a topical recall.
# gate_window_long_secs = 1800

# Relevance threshold [0.0, 1.0] — containment score of edit tokens vs
# the best-matching recent recall. Guidance:
#   0.0  — disable relevance (time-only legacy behavior).
#   0.15 — permissive; catches only clear mismatch.
#   0.30 — balanced. [default]
#   0.50 — strict; forces strong token overlap.
#   0.70+ — very strict; expect friction.
# gate_relevance_threshold = 0.30

# Append decisions to `.thoth/gate.jsonl` for calibration.
# gate_telemetry_enabled = false

# Reflection-debt thresholds. Debt = mutations (successful Write/Edit/
# NotebookEdit passes, read from gate.jsonl) minus remembers (append
# ops in memory-history.jsonl), windowed to the current session.
#
# Above the nudge threshold, the Stop + UserPromptSubmit hooks inject
# a reminder into agent context. Above the block threshold, the gate
# hard-blocks new mutations until the agent calls thoth_remember_fact
# or thoth_remember_lesson. Set `THOTH_DEFER_REFLECT=1` in the env to
# bypass for one session.
#
# Set either to `0` to disable that tier.
# reflect_debt_nudge = 10
# reflect_debt_block = 20

# Additional Bash prefixes that bypass the gate (additive with built-ins
# like `cargo test`, `git status`, `grep`).
# gate_bash_readonly_prefixes = ["pnpm lint", "just check"]

# Actor-specific policy overrides. `THOTH_ACTOR` env var selects the
# policy; first matching glob wins. Omit `[[discipline.policies]]`
# entirely to apply the default policy to every actor.
#
# [[discipline.policies]]
# actor = "hoangsa/*"
# mode = "nudge"
# window_short_secs = 300
# relevance_threshold = 0.20
#
# [[discipline.policies]]
# actor = "ci-*"
# mode = "off"

[output]
# Recall/impact text-rendering budgets. Structured JSON (`--json` /
# MCP `data`) is never truncated — only the human-readable text
# surface honours these caps.

# Maximum body lines rendered per recall chunk. Excess lines become
# a `[… truncated, M more lines. Read <path>:L<a>-L<b> for full
# body]` marker. Default: 200. Set to 0 to disable.
# max_body_lines = 200

# Soft cap on total rendered bytes per recall. A chunk in progress
# finishes, but no new chunk starts once the budget is crossed.
# Remaining chunks are elided with a footer. Default: 32768.
# Set to 0 to disable.
# max_total_bytes = 32768

# Node count above which `thoth_impact` groups results by file
# rather than listing every node. Default: 50. Set to 0 to disable
# grouping (always flat list).
# impact_group_threshold = 50
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
    // Keep a handle to the episode log before `store` is moved into the
    // retriever — we use it below to log `QueryIssued` so the CLI query
    // pre-satisfies `thoth-gate`, matching the daemon path (which logs
    // implicitly because MCP's `tool_recall` defaults `log_event: true`).
    let episodes = store.episodes.clone();

    let embedder = build_embedder(embedder_kind)?;
    let synth = build_synth(synth_kind)?;
    let is_full = embedder.is_some() || synth.is_some();

    let vectors = if embedder.is_some() {
        Some(open_vectors(&store).await?)
    } else {
        None
    };

    let retrieve_cfg = RetrieveConfig::load_or_default(root).await;
    let r = if is_full {
        Retriever::with_full(store, vectors, embedder, synth)
            .with_markdown_boost(retrieve_cfg.rerank_markdown_boost)
    } else {
        Retriever::new(store).with_markdown_boost(retrieve_cfg.rerank_markdown_boost)
    };

    let q = Query {
        text: text.clone(),
        top_k,
        ..Query::text("")
    };
    let out = if is_full {
        r.recall_full(&q).await?
    } else {
        r.recall(&q).await?
    };

    // Best-effort: a missing log entry would defeat the gate, but a broken
    // log shouldn't block the user from seeing their results.
    if let Err(e) = episodes.log_query_issued(text).await {
        warn!(error = %e, "failed to log QueryIssued event");
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Honour `[output]` in `<root>/config.toml` for body + total-size
    // caps. The daemon path above already got this treatment on the
    // server side, so both routes print the same capped text.
    let output_cfg = thoth_retrieve::OutputConfig::load_or_default(root).await;
    print!("{}", out.render_with(&output_cfg.render_options()));
    Ok(())
}

async fn cmd_watch(
    root: &std::path::Path,
    src: &std::path::Path,
    debounce: Duration,
    embedder_kind: Option<EmbedderKind>,
) -> anyhow::Result<()> {
    // If the MCP daemon is running it holds the redb exclusive lock.
    // Instead of failing, fall back to a log-only mode: watch the
    // filesystem and print what changed, but don't index (the daemon's
    // auto-watch handles that when `[watch] enabled = true`).
    if daemon::DaemonClient::try_connect(root).await.is_some() {
        let watch_cfg = thoth_retrieve::WatchConfig::load_or_default(root).await;
        if watch_cfg.enabled {
            println!(
                "thoth-mcp daemon is running with auto-watch enabled — \
                 showing live file-change log only."
            );
        } else {
            println!(
                "thoth-mcp daemon is running (auto-watch disabled). \
                 Showing live file-change log. Tip: set `[watch] enabled = true` \
                 in config.toml to auto-reindex inside the daemon."
            );
        }
        return cmd_watch_log_only(src, debounce).await;
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

/// Log-only fallback for `thoth watch` when the MCP daemon holds the
/// redb lock. Watches the filesystem and prints changes, but doesn't
/// index — the daemon handles that.
async fn cmd_watch_log_only(
    src: &std::path::Path,
    debounce: Duration,
) -> anyhow::Result<()> {
    let mut w = Watcher::watch(src, 1024)?;
    println!("… watching {} (log only, ctrl-c to stop)", src.display());

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
                let mut batch = vec![ev];
                let deadline = tokio::time::Instant::now() + debounce;
                while let Ok(Some(extra)) = tokio::time::timeout_at(deadline, w.recv()).await {
                    batch.push(extra);
                }

                let mut changed = Vec::new();
                let mut deleted = Vec::new();
                for ev in batch {
                    match ev {
                        thoth_core::Event::FileChanged { path, .. } => changed.push(path),
                        thoth_core::Event::FileDeleted { path, .. } => deleted.push(path),
                        _ => {}
                    }
                }

                for p in &changed {
                    println!("  ✎ {}", p.display());
                }
                for p in &deleted {
                    println!("  ✗ {}", p.display());
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

/// Per-query, per-mode measurement. `rank` is 1-indexed; `0` means no
/// chunk in the top-k matched any of the gold's `expect_*` clauses.
struct QueryRun {
    rank: usize,
    returned: usize,
    elapsed_us: u128,
}

impl QueryRun {
    fn hit(&self) -> bool {
        self.rank > 0
    }
}

/// Aggregate metrics for one mode over the whole gold set. Computed
/// lazily from the per-query `runs` so the raw data is also serializable.
struct ModeReport {
    label: &'static str,
    runs: Vec<QueryRun>,
}

impl ModeReport {
    fn hits(&self) -> usize {
        self.runs.iter().filter(|r| r.hit()).count()
    }

    fn precision_at_k(&self) -> f64 {
        if self.runs.is_empty() {
            0.0
        } else {
            self.hits() as f64 / self.runs.len() as f64
        }
    }

    /// Mean reciprocal rank. Misses contribute `0`, which matches the
    /// standard definition and makes MRR comparable across runs even
    /// when hit rates differ.
    fn mrr(&self) -> f64 {
        if self.runs.is_empty() {
            return 0.0;
        }
        let sum: f64 = self
            .runs
            .iter()
            .map(|r| if r.hit() { 1.0 / r.rank as f64 } else { 0.0 })
            .sum();
        sum / self.runs.len() as f64
    }

    /// `p` in `[0.0, 1.0]`. Uses the nearest-rank method (ceil), which
    /// is deterministic for small N and avoids the interpolation
    /// ambiguity in percentile definitions.
    fn latency_percentile_us(&self, p: f64) -> u128 {
        if self.runs.is_empty() {
            return 0;
        }
        let mut v: Vec<u128> = self.runs.iter().map(|r| r.elapsed_us).collect();
        v.sort_unstable();
        let idx = ((v.len() as f64 * p).ceil().max(1.0) as usize - 1).min(v.len() - 1);
        v[idx]
    }
}

/// 1-indexed rank of the first chunk that satisfies the gold clause, or
/// `0` if none do. Matching rules mirror the original implementation:
/// `expect_path` substring-matches the chunk path, `expect_text`
/// substring-matches `preview + body`; a chunk qualifies if *either*
/// non-empty bucket matches (any-of).
fn match_rank(gold: &GoldQuery, out: &thoth_core::Retrieval) -> usize {
    for (i, c) in out.chunks.iter().enumerate() {
        let p = c.path.to_string_lossy().to_lowercase();
        let body = format!("{} {}", c.preview, c.body).to_lowercase();
        let path_ok = gold.expect_path.iter().any(|s| p.contains(&s.to_lowercase()));
        let text_ok = gold.expect_text.iter().any(|s| body.contains(&s.to_lowercase()));
        if (!gold.expect_path.is_empty() && path_ok) || (!gold.expect_text.is_empty() && text_ok) {
            return i + 1;
        }
    }
    0
}

async fn cmd_eval(
    root: &std::path::Path,
    gold_path: &std::path::Path,
    top_k: usize,
    mode: EvalMode,
    embedder_kind: Option<EmbedderKind>,
    synth_kind: Option<SynthKind>,
    json: bool,
) -> anyhow::Result<()> {
    let raw = tokio::fs::read_to_string(gold_path).await?;
    let gold: GoldSet = toml::from_str(&raw)?;
    if gold.query.is_empty() {
        anyhow::bail!("gold set is empty");
    }

    let want_zero = matches!(mode, EvalMode::Zero | EvalMode::Both);
    let want_full = matches!(mode, EvalMode::Full | EvalMode::Both);

    if want_full && embedder_kind.is_none() && synth_kind.is_none() {
        anyhow::bail!(
            "--mode {} requires --embedder and/or --synth (Mode::Full needs at least one provider)",
            if mode == EvalMode::Full { "full" } else { "both" }
        );
    }

    // For pure-Zero runs we prefer the running daemon so `thoth eval` works
    // even when Claude Code is holding the redb lock. Any Full mode needs
    // direct store access (the daemon is Mode::Zero only), so if the daemon
    // is up we bail with a clear message instead of fighting for the lock.
    let daemon_for_zero = if want_zero && !want_full {
        daemon::DaemonClient::try_connect(root).await
    } else {
        None
    };

    if want_full && daemon::DaemonClient::try_connect(root).await.is_some() {
        anyhow::bail!(
            "thoth-mcp daemon is running; stop it before running `thoth eval --mode {}` \
             (Mode::Full would fight for the redb exclusive lock)",
            if mode == EvalMode::Full { "full" } else { "both" }
        );
    }

    // Open the store once and clone `StoreRoot` into each retriever (it's
    // cheap — the inner handles are Arc'd).
    let store = if want_full || daemon_for_zero.is_none() {
        Some(StoreRoot::open(root).await?)
    } else {
        None
    };

    let retrieve_cfg = RetrieveConfig::load_or_default(root).await;
    let zero_retriever = if want_zero && daemon_for_zero.is_none() {
        Some(
            Retriever::new(store.as_ref().unwrap().clone())
                .with_markdown_boost(retrieve_cfg.rerank_markdown_boost),
        )
    } else {
        None
    };

    let full_retriever = if want_full {
        let embedder = build_embedder(embedder_kind)?;
        let synth = build_synth(synth_kind)?;
        let vectors = if embedder.is_some() {
            Some(open_vectors(store.as_ref().unwrap()).await?)
        } else {
            None
        };
        Some(
            Retriever::with_full(
                store.as_ref().unwrap().clone(),
                vectors,
                embedder,
                synth,
            )
            .with_markdown_boost(retrieve_cfg.rerank_markdown_boost),
        )
    } else {
        None
    };

    let mut daemon_client = daemon_for_zero;
    let mut zero_runs: Vec<QueryRun> = Vec::new();
    let mut full_runs: Vec<QueryRun> = Vec::new();

    for gq in &gold.query {
        if want_zero {
            let (out, elapsed_us) = if let Some(c) = daemon_client.as_mut() {
                let start = Instant::now();
                let result = c
                    .call(
                        "thoth_recall",
                        serde_json::json!({ "query": gq.q, "top_k": top_k }),
                    )
                    .await?;
                let elapsed_us = start.elapsed().as_micros();
                if daemon::tool_is_error(&result) {
                    anyhow::bail!("{}", daemon::tool_text(&result));
                }
                let out: thoth_core::Retrieval =
                    serde_json::from_value(daemon::tool_data(&result))?;
                (out, elapsed_us)
            } else {
                let r = zero_retriever.as_ref().unwrap();
                let start = Instant::now();
                let out = r
                    .recall(&Query {
                        text: gq.q.clone(),
                        top_k,
                        ..Query::text("")
                    })
                    .await?;
                (out, start.elapsed().as_micros())
            };
            zero_runs.push(QueryRun {
                rank: match_rank(gq, &out),
                returned: out.chunks.len(),
                elapsed_us,
            });
        }

        if want_full {
            let r = full_retriever.as_ref().unwrap();
            let start = Instant::now();
            let out = r
                .recall_full(&Query {
                    text: gq.q.clone(),
                    top_k,
                    ..Query::text("")
                })
                .await?;
            let elapsed_us = start.elapsed().as_micros();
            full_runs.push(QueryRun {
                rank: match_rank(gq, &out),
                returned: out.chunks.len(),
                elapsed_us,
            });
        }
    }

    let mut reports: Vec<ModeReport> = Vec::new();
    if want_zero {
        reports.push(ModeReport {
            label: "zero",
            runs: zero_runs,
        });
    }
    if want_full {
        reports.push(ModeReport {
            label: "full",
            runs: full_runs,
        });
    }

    if json {
        let modes_json: Vec<_> = reports
            .iter()
            .map(|rep| {
                serde_json::json!({
                    "mode": rep.label,
                    "total": rep.runs.len(),
                    "hits": rep.hits(),
                    "precision_at_k": rep.precision_at_k(),
                    "mrr": rep.mrr(),
                    "latency_us": {
                        "p50": rep.latency_percentile_us(0.50),
                        "p95": rep.latency_percentile_us(0.95),
                    },
                    "queries": gold.query.iter().zip(rep.runs.iter()).map(|(gq, r)| {
                        serde_json::json!({
                            "q": gq.q,
                            "hit": r.hit(),
                            "rank": r.rank,
                            "returned": r.returned,
                            "elapsed_us": r.elapsed_us,
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let v = serde_json::json!({
            "top_k": top_k,
            "modes": modes_json,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        // Per-query line. In both-mode we prefix with `Z:` / `F:` marks so
        // wins/losses are visually aligned; in single-mode we drop the
        // prefix to stay close to the original terse format.
        let show_prefix = want_zero && want_full;
        let zero_runs = reports.iter().find(|r| r.label == "zero");
        let full_runs = reports.iter().find(|r| r.label == "full");
        for (i, gq) in gold.query.iter().enumerate() {
            let parts: Vec<String> = [("Z", zero_runs), ("F", full_runs)]
                .iter()
                .filter_map(|(tag, rep)| rep.map(|rep| (*tag, &rep.runs[i])))
                .map(|(tag, run)| {
                    let mark = if run.hit() {
                        format!("✓@{}", run.rank)
                    } else {
                        "✗   ".to_string()
                    };
                    if show_prefix {
                        format!("{tag}:{mark}")
                    } else {
                        format!("{mark}  [{:>2}]", run.returned)
                    }
                })
                .collect();
            println!("{}  {}", parts.join(" "), gq.q);
        }
        println!();
        for rep in &reports {
            println!(
                "[{label}] P@{top_k}={hits}/{total}={p:.3}  MRR={mrr:.3}  \
                 latency p50={p50}µs p95={p95}µs",
                label = rep.label,
                hits = rep.hits(),
                total = rep.runs.len(),
                p = rep.precision_at_k(),
                mrr = rep.mrr(),
                p50 = rep.latency_percentile_us(0.50),
                p95 = rep.latency_percentile_us(0.95),
            );
        }
    }

    // Non-zero exit if any active mode missed a query, so CI can gate on
    // eval regressions just like before — now across whichever mode(s)
    // were requested.
    let any_miss = reports
        .iter()
        .any(|rep| rep.runs.iter().any(|r| !r.hit()));
    if any_miss {
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

// -------------------------------------------------------- graph subcommands

/// `thoth impact <fqn>` — forwards to the `thoth_impact` MCP tool.
///
/// The daemon path is preferred (keeps us working when Claude Code is
/// holding the redb lock); if unavailable we fall back to opening the
/// store directly and calling the graph API in-process. Exit code is
/// non-zero when the graph can't find the symbol, so shell pipelines
/// can gate on missing FQNs.
async fn cmd_impact(
    root: &std::path::Path,
    fqn: &str,
    direction: ImpactDir,
    depth: usize,
    json: bool,
) -> anyhow::Result<()> {
    let args = serde_json::json!({
        "fqn": fqn,
        "direction": direction.as_str(),
        "depth": depth,
    });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_impact", args).await?;
    emit_output(text, data, is_error, json)
}

/// `thoth context <fqn>` — forwards to the `thoth_symbol_context` tool.
async fn cmd_context(
    root: &std::path::Path,
    fqn: &str,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let args = serde_json::json!({ "fqn": fqn, "limit": limit });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_symbol_context", args).await?;
    emit_output(text, data, is_error, json)
}

/// `thoth changes` — feed a unified diff through the `thoth_detect_changes`
/// tool. Diff source order of preference: `--from <file>` > `--from -`
/// (stdin) > `git diff HEAD`.
async fn cmd_changes(
    root: &std::path::Path,
    from: Option<&str>,
    depth: usize,
    json: bool,
) -> anyhow::Result<()> {
    let diff = match from {
        Some("-") => {
            use tokio::io::AsyncReadExt;
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            buf
        }
        Some(path) => tokio::fs::read_to_string(path).await?,
        None => {
            // Default: diff of the current working tree against HEAD.
            // `git diff HEAD` includes both staged and unstaged changes,
            // which matches the "what am I about to commit?" intuition.
            let output = tokio::process::Command::new("git")
                .args(["diff", "HEAD"])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run git diff: {e}"))?;
            if !output.status.success() {
                anyhow::bail!(
                    "`git diff HEAD` exited non-zero: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            String::from_utf8(output.stdout)
                .map_err(|e| anyhow::anyhow!("git diff output not UTF-8: {e}"))?
        }
    };
    if diff.trim().is_empty() {
        println!("(no diff — working tree matches HEAD)");
        return Ok(());
    }
    let args = serde_json::json!({ "diff": diff, "depth": depth });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_detect_changes", args).await?;
    emit_output(text, data, is_error, json)
}

/// `thoth curate` — maintenance pass over memory. Composes three
/// cheap checks that together keep the store honest between sessions:
///
/// 1. **Forget pass** (`thoth_memory_forget` via daemon, else direct)
///    — TTL + capacity + quarantine pruning.
/// 2. **Reflection debt report** — same counter the gate blocks on,
///    surfaced here as a heads-up so the user can see where a session
///    landed before the next one starts.
///
/// Designed to be safe to call every SessionStart: short-circuit to a
/// silent exit when `--quiet` is set and nothing actionable turned
/// up. That keeps the banner clean in the healthy case.
async fn cmd_curate(root: &std::path::Path, quiet: bool) -> anyhow::Result<()> {
    if !root.exists() {
        if !quiet {
            println!("(no .thoth/ at {} — nothing to curate)", root.display());
        }
        return Ok(());
    }

    let mut findings: Vec<String> = Vec::new();

    // Forget pass. Prefer the daemon so we don't collide with the
    // MCP server's exclusive redb lock; fall back to opening the
    // store directly.
    let forget_report = if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        match d.call("thoth_memory_forget", serde_json::json!({})).await {
            Ok(res) if !daemon::tool_is_error(&res) => {
                // Suppress no-op passes — a stale daemon binary may still
                // send the legacy always-non-empty text, so we check the
                // structured `data` counters instead of the text.
                if hooks::forget_has_drops(&daemon::tool_data(&res)) {
                    Some(daemon::tool_text(&res).to_string())
                } else {
                    None
                }
            }
            Ok(res) => {
                findings.push(format!("forget failed: {}", daemon::tool_text(&res)));
                None
            }
            Err(e) => {
                findings.push(format!("forget failed: {e}"));
                None
            }
        }
    } else {
        // Direct store access — needed when no daemon is alive.
        // `forget_pass` returns a detailed report; only surface it
        // when something was actually dropped.
        match thoth_memory::MemoryManager::open(root).await {
            Ok(m) => match m.forget_pass().await {
                Ok(r) => {
                    // Include every counter — a quarantine that didn't
                    // touch any episode is still a finding worth
                    // surfacing. Suppress entirely when all four are
                    // zero to match the daemon path's no-op silence.
                    let total = r.episodes_ttl
                        + r.episodes_cap
                        + r.lessons_dropped
                        + r.lessons_quarantined;
                    if total > 0 {
                        Some(format!(
                            "forget pass: episodes_ttl={} episodes_cap={} lessons_dropped={} lessons_quarantined={}",
                            r.episodes_ttl, r.episodes_cap, r.lessons_dropped, r.lessons_quarantined
                        ))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    findings.push(format!("forget failed: {e}"));
                    None
                }
            },
            Err(e) => {
                findings.push(format!("open store failed: {e}"));
                None
            }
        }
    };
    if let Some(s) = forget_report {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            findings.push(trimmed.to_string());
        }
    }

    // Reflection debt. Sync compute keeps this cheap (~1 ms).
    let disc = thoth_memory::DisciplineConfig::load_or_default(root).await;
    let debt = thoth_memory::ReflectionDebt::compute(root).await;
    if debt.should_nudge(&disc) {
        findings.push(debt.render());
    }

    // Lesson-cluster detection. Five or more lessons that share enough
    // trigger tokens (Jaccard ≥ 0.4) probably want to collapse into a
    // single skill — we surface each cluster with a ready-to-paste
    // `thoth_skill_propose` suggestion so the user can act on it.
    // Best-effort: a read failure only degrades this pass; curate must
    // still report debt + forget findings.
    match thoth_store::markdown::MarkdownStore::open(root).await {
        Ok(md) => match md.read_lessons().await {
            Ok(lessons) => {
                let clusters = thoth_memory::detect_clusters(
                    &lessons,
                    thoth_memory::DEFAULT_CLUSTER_MIN_SIZE,
                    thoth_memory::DEFAULT_CLUSTER_JACCARD,
                );
                for c in clusters {
                    // Shortened sample (first 2 triggers) so curate output
                    // stays scannable on a ≥5-member cluster.
                    let sample: Vec<String> = c
                        .triggers
                        .iter()
                        .take(2)
                        .map(|t| format!("\"{t}\""))
                        .collect();
                    let slug_hint = c
                        .shared_tokens
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("-");
                    let slug_hint = if slug_hint.is_empty() {
                        "lesson-cluster".to_string()
                    } else {
                        slug_hint
                    };
                    findings.push(format!(
                        "lesson cluster: {} lessons share triggers (e.g. {}). \
                         Consider `thoth_skill_propose {{slug: \"{slug_hint}\", \
                         source_triggers: [{}, …]}}`.",
                        c.triggers.len(),
                        sample.join(", "),
                        sample.join(", ")
                    ));
                }
            }
            Err(e) => findings.push(format!("lesson-cluster read failed: {e}")),
        },
        Err(e) => findings.push(format!("lesson-cluster open failed: {e}")),
    }

    if findings.is_empty() {
        if !quiet {
            println!("curate: nothing to flag (debt={}, no TTL work)", debt.debt());
        }
        return Ok(());
    }

    println!("### Curator findings");
    for f in &findings {
        println!("- {f}");
    }
    Ok(())
}

async fn cmd_review(root: &std::path::Path, backend: &str) -> anyhow::Result<()> {
    if !root.exists() {
        println!("(no .thoth/ at {} — nothing to review)", root.display());
        return Ok(());
    }
    match review::run_review(root, backend).await {
        Ok(report) => {
            let total = report.facts_added + report.lessons_added + report.skills_proposed;
            if total > 0 {
                eprintln!(
                    "thoth: background review added {} facts, {} lessons, {} skill proposals",
                    report.facts_added, report.lessons_added, report.skills_proposed,
                );
            } else {
                eprintln!("thoth: background review — nothing worth saving");
            }
        }
        Err(e) => eprintln!("thoth: background review failed: {e}"),
    }
    Ok(())
}

/// Invoke an MCP tool over the daemon socket when one is running; else
/// spin up an in-process server and dispatch through it. Returns
/// `(text, data, is_error)` so the caller can pick which to surface
/// based on `--json`.
async fn call_mcp_tool(
    root: &std::path::Path,
    tool: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<(String, serde_json::Value, bool)> {
    if let Some(mut d) = daemon::DaemonClient::try_connect(root).await {
        let result = d.call(tool, arguments).await?;
        let is_error = daemon::tool_is_error(&result);
        let text = daemon::tool_text(&result).to_string();
        let data = daemon::tool_data(&result);
        return Ok((text, data, is_error));
    }
    // In-process: reuse the Server so we don't duplicate tool bodies.
    // This also means the CLI and daemon paths share test coverage.
    let server = thoth_mcp::Server::open(root).await?;
    let params = serde_json::json!({
        "name": tool,
        "arguments": arguments,
    });
    let msg = thoth_mcp::proto::RpcIncoming {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::Value::Number(1.into())),
        method: "thoth.call".to_string(),
        params,
    };
    let resp = server.handle(msg).await;
    let Some(response) = resp else {
        anyhow::bail!("server returned no response for {tool}");
    };
    if let Some(err) = response.error {
        anyhow::bail!("{}: {}", err.code, err.message);
    }
    // `ToolOutput` is Serialize-only (the server never deserialises its
    // own output), so pull fields from the raw JSON instead of
    // round-tripping through from_value.
    let result = response.result.unwrap_or_default();
    let text = result
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let data = result.get("data").cloned().unwrap_or_default();
    Ok((text, data, is_error))
}

/// Emit either the rendered text or a pretty-printed JSON dump of the
/// structured `data` half. When `is_error` is set the process exits
/// non-zero so shell pipelines can gate on missing FQNs / malformed
/// diffs.
fn emit_output(
    text: String,
    data: serde_json::Value,
    is_error: bool,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else if !text.is_empty() {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    if is_error {
        std::process::exit(1);
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
            .map(|s| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339))
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
