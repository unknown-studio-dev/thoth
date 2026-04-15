//! Markdown-backed memory surface.
//!
//! The files below are the **source of truth** for all user-visible memory.
//! Every binary index (redb, tantivy, sqlite, lance) is derivable from them,
//! so users can edit these files by hand and Thoth will rebuild its indexes.
//!
//! Layout under `<root>/`:
//!
//! ```text
//! MEMORY.md          # declarative facts — one per bullet
//! LESSONS.md         # reflective memory — lessons learned
//! skills/
//!   <slug>/SKILL.md  # procedural memory — agentskills.io compatible
//! ```
//!
//! ### MEMORY.md format
//!
//! Each fact is a level-3 heading followed by free text and an optional
//! `tags:` line:
//!
//! ```markdown
//! ### auth uses JWT with RS256
//! The `thoth-auth` crate signs tokens via RS256. Keys live in Vault.
//! tags: auth, security
//! ```
//!
//! ### LESSONS.md format
//!
//! Lessons are level-3 headings where the heading is the trigger and the
//! body is the advice:
//!
//! ```markdown
//! ### when editing migrations
//! Always run `sqlx prepare` after changing SQL. Failing to do so breaks CI.
//! ```
//!
//! ### SKILL.md format (agentskills.io frontmatter)
//!
//! ```markdown
//! ---
//! name: auth-jwt-pattern
//! description: How to wire a JWT-based auth flow through thoth-auth.
//! ---
//! # steps
//! 1. ...
//! ```

use std::path::{Path, PathBuf};

use thoth_core::{Fact, Lesson, MemoryKind, MemoryMeta, Result, Skill};
use tokio::io::AsyncWriteExt;

const MEMORY_MD: &str = "MEMORY.md";
const LESSONS_MD: &str = "LESSONS.md";
const SKILLS_DIR: &str = "skills";
const SKILL_MD: &str = "SKILL.md";

/// Reader/writer for the markdown memory surface under `root`.
#[derive(Clone)]
pub struct MarkdownStore {
    /// Root folder (typically `.thoth/`).
    pub root: PathBuf,
}

impl MarkdownStore {
    /// Open (or create) the markdown store at `root`.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        tokio::fs::create_dir_all(root.join(SKILLS_DIR)).await?;
        Ok(Self { root })
    }

    // -- MEMORY.md ------------------------------------------------------

    /// Read every fact currently on disk.
    pub async fn read_facts(&self) -> Result<Vec<Fact>> {
        let path = self.root.join(MEMORY_MD);
        let text = read_or_empty(&path).await?;
        Ok(parse_facts(&text))
    }

    /// Case-insensitive substring filter over [`Self::read_facts`].
    ///
    /// Matches either the fact text or any of its tags. Returns the facts
    /// in the same order they appear on disk.
    pub async fn grep_facts(&self, needle: impl AsRef<str>) -> Result<Vec<Fact>> {
        let needle = needle.as_ref().to_lowercase();
        let all = self.read_facts().await?;
        Ok(all
            .into_iter()
            .filter(|f| {
                f.text.to_lowercase().contains(&needle)
                    || f.tags.iter().any(|t| t.to_lowercase().contains(&needle))
            })
            .collect())
    }

    /// Append a fact to `MEMORY.md`. File is created if missing.
    pub async fn append_fact(&self, f: &Fact) -> Result<()> {
        let path = self.root.join(MEMORY_MD);
        let mut out = String::new();
        out.push_str("\n### ");
        out.push_str(&first_line(&f.text));
        out.push('\n');
        let body_rest = remainder(&f.text);
        if !body_rest.is_empty() {
            out.push_str(&body_rest);
            if !body_rest.ends_with('\n') {
                out.push('\n');
            }
        }
        if !f.tags.is_empty() {
            out.push_str("tags: ");
            out.push_str(&f.tags.join(", "));
            out.push('\n');
        }
        append_atomic(&path, &out).await
    }

    // -- LESSONS.md -----------------------------------------------------

    /// Read every lesson currently on disk.
    pub async fn read_lessons(&self) -> Result<Vec<Lesson>> {
        let path = self.root.join(LESSONS_MD);
        let text = read_or_empty(&path).await?;
        Ok(parse_lessons(&text))
    }

    /// Append a lesson. File is created if missing.
    pub async fn append_lesson(&self, l: &Lesson) -> Result<()> {
        let path = self.root.join(LESSONS_MD);
        let mut out = String::new();
        out.push_str("\n### ");
        out.push_str(l.trigger.trim());
        out.push('\n');
        out.push_str(l.advice.trim_end());
        out.push('\n');
        append_atomic(&path, &out).await
    }

    // -- skills/ --------------------------------------------------------

    /// List every installed skill.
    pub async fn list_skills(&self) -> Result<Vec<Skill>> {
        let skills_root = self.root.join(SKILLS_DIR);
        if !skills_root.exists() {
            return Ok(Vec::new());
        }
        let mut rd = tokio::fs::read_dir(&skills_root).await?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let skill_md = p.join(SKILL_MD);
            if !skill_md.exists() {
                continue;
            }
            let text = tokio::fs::read_to_string(&skill_md).await?;
            let (name, description) = parse_skill_frontmatter(&text);
            let slug = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let meta = MemoryMeta::new(MemoryKind::Procedural);
            out.push(Skill {
                meta,
                slug: name.unwrap_or(slug.clone()),
                description: description.unwrap_or_default(),
                path: PathBuf::from(SKILLS_DIR).join(&slug),
            });
        }
        Ok(out)
    }
}

// ---- parsing helpers -------------------------------------------------------

fn parse_facts(text: &str) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut iter = text.lines().peekable();

    while let Some(line) = iter.next() {
        let Some(title) = line.strip_prefix("### ") else {
            continue;
        };
        let mut body = String::new();
        let mut tags = Vec::new();

        while let Some(next) = iter.peek() {
            if next.starts_with("### ") || next.starts_with("## ") || next.starts_with("# ") {
                break;
            }
            let n = iter.next().unwrap();
            if let Some(rest) = n.strip_prefix("tags:") {
                tags = rest
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
            } else if !n.trim().is_empty() || !body.is_empty() {
                body.push_str(n);
                body.push('\n');
            }
        }

        let fact_text = if body.trim().is_empty() {
            title.trim().to_string()
        } else {
            format!("{}\n{}", title.trim(), body.trim_end())
        };

        out.push(Fact {
            meta: MemoryMeta::new(MemoryKind::Semantic),
            text: fact_text,
            tags,
        });
    }
    out
}

fn parse_lessons(text: &str) -> Vec<Lesson> {
    let mut out = Vec::new();
    let mut iter = text.lines().peekable();

    while let Some(line) = iter.next() {
        let Some(trigger) = line.strip_prefix("### ") else {
            continue;
        };
        let mut advice = String::new();

        while let Some(next) = iter.peek() {
            if next.starts_with("### ") || next.starts_with("## ") || next.starts_with("# ") {
                break;
            }
            let n = iter.next().unwrap();
            advice.push_str(n);
            advice.push('\n');
        }

        out.push(Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger.trim().to_string(),
            advice: advice.trim_end().to_string(),
            success_count: 0,
            failure_count: 0,
        });
    }
    out
}

/// Parse the `---`-fenced YAML-ish frontmatter of a SKILL.md.
///
/// Recognised keys: `name`, `description`. Anything else is ignored.
fn parse_skill_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, None);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, None);
    };
    let block = &rest[..end];
    let mut name = None;
    let mut desc = None;
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = Some(v.trim().to_string());
        }
    }
    (name, desc)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn remainder(s: &str) -> String {
    let mut it = s.lines();
    it.next();
    let mut out = String::new();
    for l in it {
        out.push_str(l);
        out.push('\n');
    }
    out
}

async fn read_or_empty(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Append `chunk` to `path`, creating the file if it does not exist.
async fn append_atomic(path: &Path, chunk: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(chunk.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}
