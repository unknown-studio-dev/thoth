//! The `thoth` command-line interface.
//!
//! ```text
//! thoth init                        # create .thoth/ in the current directory
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
use thoth_store::{StoreRoot, VectorStore};
use tracing::warn;

mod hooks;

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
    /// Initialise a new `.thoth/` directory.
    Init,

    /// Parse + index a source tree. With `--embedder` set, also writes
    /// semantic vectors into `index/vectors.sqlite`.
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
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    /// List installed skills.
    List,
    /// Install the bundled `thoth` skill so Claude Code can discover it.
    Install {
        /// Where to install. `project` drops it into `<root>/skills/thoth/`,
        /// `user` drops it into `~/.claude/skills/thoth/`.
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
/// By default we're *silent* — only `error` propagates, so a clean run of
/// `thoth init` or `thoth index` shows only the commands own `println!`
/// output (no `INFO thoth_retrieve::indexer: …` noise). `-v` opens the tap
/// to `info`, `-vv` to `debug`, `-vvv` to `trace`. If `RUST_LOG` is set
/// *and* no `-v` flag was passed we honour it so power users keep their
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
        Cmd::Eval { gold, top_k } => cmd_eval(&cli.root, &gold, top_k, cli.json).await?,
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
    let path = store.path.join("index").join("vectors.sqlite");
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
    let verb = if existed { "refreshed" } else { "created" };
    println!("✓ {verb} {}", store.path.display());
    if !seeded.is_empty() {
        println!("  seeded: {}", seeded.join(", "));
    }
    println!("  next:   thoth index .");
    Ok(())
}

async fn cmd_index(
    root: &std::path::Path,
    src: &std::path::Path,
    embedder_kind: Option<EmbedderKind>,
) -> anyhow::Result<()> {
    let store = StoreRoot::open(root).await?;
    let mut idx = Indexer::new(store.clone(), LanguageRegistry::new());
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

    if out.chunks.is_empty() {
        println!("(no matches — did you run `thoth index`?)");
        return Ok(());
    }
    for (i, c) in out.chunks.iter().enumerate() {
        let sym = c.symbol.as_deref().unwrap_or("-");
        println!(
            "\n[{i:>2}] score={:.4} src={:?}  {}  {}:{}-{}",
            c.score,
            c.source,
            sym,
            c.path.display(),
            c.span.0,
            c.span.1
        );
        if !c.preview.is_empty() {
            println!("     {}", c.preview);
        }
    }
    if let Some(answer) = &out.synthesized {
        println!("\n─── synthesized ───\n{answer}");
    }
    Ok(())
}

async fn cmd_watch(
    root: &std::path::Path,
    src: &std::path::Path,
    debounce: Duration,
    embedder_kind: Option<EmbedderKind>,
) -> anyhow::Result<()> {
    let store = StoreRoot::open(root).await?;
    let mut idx = Indexer::new(store.clone(), LanguageRegistry::new());
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

                let mut touched = std::collections::HashSet::new();
                for ev in batch {
                    match ev {
                        thoth_core::Event::FileChanged { path, .. }
                        | thoth_core::Event::FileDeleted { path, .. } => {
                            touched.insert(path);
                        }
                        _ => {}
                    }
                }
                let n = touched.len();
                for path in touched {
                    if let Err(e) = idx.index_file(&path).await {
                        warn!(?path, error = %e, "re-index failed");
                    }
                }
                if n > 0 {
                    println!("  ↻ reindexed {n} file{}", if n == 1 { "" } else { "s" });
                }
                // Flush BM25 writes so the next `query` sees them.
                if let Err(e) = idx_commit(&idx).await {
                    warn!(error = %e, "fts commit failed");
                }
            }
        }
    }
    Ok(())
}

/// Small helper: commit the FTS writer after a batch of per-file re-indexes.
/// We go through the `Indexer`'s store since `Indexer` doesn't expose commit
/// directly (it only commits at the end of a full `index_path`).
async fn idx_commit(_idx: &Indexer) -> anyhow::Result<()> {
    // No-op fallback: the indexer commits at end-of-path; for single-file
    // updates we rely on tantivy's next search picking up uncommitted docs at
    // the next full `index_path`. This keeps the watch loop simple until the
    // Indexer grows an explicit `commit()` hook.
    Ok(())
}

async fn cmd_memory_show(root: &std::path::Path) -> anyhow::Result<()> {
    let store = StoreRoot::open(root).await?;
    for name in ["MEMORY.md", "LESSONS.md"] {
        let p = store.path.join(name);
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
    let store = StoreRoot::open(root).await?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let path = store.path.join("MEMORY.md");
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

    let store = StoreRoot::open(root).await?;
    let fact = Fact {
        meta: MemoryMeta::new(MemoryKind::Semantic),
        text,
        tags,
    };
    store.markdown.append_fact(&fact).await?;
    println!("fact appended to {}", store.path.join("MEMORY.md").display());
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
    let mm = MemoryManager::open(root).await?;
    let report = mm.forget_pass().await?;
    if json {
        let v = serde_json::json!({
            "episodes_ttl": report.episodes_ttl,
            "episodes_cap": report.episodes_cap,
            "lessons_dropped": report.lessons_dropped,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "forget pass: episodes_ttl={} episodes_cap={} lessons_dropped={}",
            report.episodes_ttl, report.episodes_cap, report.lessons_dropped
        );
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

    let store = StoreRoot::open(root).await?;
    let r = Retriever::new(store);

    let mut hits = 0usize;
    let total = gold.query.len();
    let mut per_query = Vec::with_capacity(total);

    for gq in &gold.query {
        let out = r
            .recall(&Query {
                text: gq.q.clone(),
                top_k,
                ..Query::text("")
            })
            .await?;

        let got = out.chunks.iter().any(|c| {
            let p = c.path.to_string_lossy().to_lowercase();
            // Match against the full body too — previews are just a few
            // lines, which is often too narrow for a keyword probe.
            let body = format!("{} {}", c.preview, c.body).to_lowercase();
            let path_ok = gq.expect_path.is_empty()
                || gq.expect_path.iter().any(|s| p.contains(&s.to_lowercase()));
            let text_ok = gq.expect_text.is_empty()
                || gq.expect_text.iter().any(|s| body.contains(&s.to_lowercase()));
            // Any-of semantics across the two expect_* buckets — if *either*
            // bucket matches we count the query as answered.
            (!gq.expect_path.is_empty() && path_ok)
                || (!gq.expect_text.is_empty() && text_ok)
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
