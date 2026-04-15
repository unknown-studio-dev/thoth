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
        append_atomic(&path, &render_fact(f)).await
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
        append_atomic(&path, &render_lesson(l)).await
    }

    /// Rewrite `MEMORY.md` from scratch with the given facts in order.
    /// Used by the forget pass to drop decayed facts. Atomic: writes to a
    /// sibling temp file then renames.
    pub async fn rewrite_facts(&self, facts: &[Fact]) -> Result<()> {
        let path = self.root.join(MEMORY_MD);
        let mut body = String::from("# MEMORY.md\n");
        for f in facts {
            body.push_str(&render_fact(f));
        }
        write_atomic(&path, &body).await
    }

    /// Rewrite `LESSONS.md` from scratch with the given lessons in order.
    /// Used by the forget pass to drop low-confidence lessons and by the
    /// confidence-evolution flow to update success/failure counts.
    pub async fn rewrite_lessons(&self, lessons: &[Lesson]) -> Result<()> {
        let path = self.root.join(LESSONS_MD);
        let mut body = String::from("# LESSONS.md\n");
        for l in lessons {
            body.push_str(&render_lesson(l));
        }
        write_atomic(&path, &body).await
    }

    /// Increment `success_count` on every lesson whose `trigger` matches
    /// one of `triggers` (case-insensitive). No-op for unknown triggers.
    /// Returns the number of lessons bumped.
    pub async fn bump_lesson_success(&self, triggers: &[String]) -> Result<usize> {
        self.bump_lesson_counters(triggers, true).await
    }

    /// Increment `failure_count` on every lesson whose `trigger` matches
    /// one of `triggers`. Same contract as [`Self::bump_lesson_success`].
    pub async fn bump_lesson_failure(&self, triggers: &[String]) -> Result<usize> {
        self.bump_lesson_counters(triggers, false).await
    }

    async fn bump_lesson_counters(
        &self,
        triggers: &[String],
        success: bool,
    ) -> Result<usize> {
        if triggers.is_empty() {
            return Ok(0);
        }
        let wanted: std::collections::HashSet<String> = triggers
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if wanted.is_empty() {
            return Ok(0);
        }
        let mut lessons = self.read_lessons().await?;
        let mut bumped = 0usize;
        for l in lessons.iter_mut() {
            let key = l.trigger.trim().to_ascii_lowercase();
            if wanted.contains(&key) {
                if success {
                    l.success_count = l.success_count.saturating_add(1);
                } else {
                    l.failure_count = l.failure_count.saturating_add(1);
                }
                bumped += 1;
            }
        }
        if bumped > 0 {
            self.rewrite_lessons(&lessons).await?;
        }
        Ok(bumped)
    }

    // -- skills/ --------------------------------------------------------

    /// Copy a skill directory into `<root>/skills/<slug>/`.
    ///
    /// `src` must contain a `SKILL.md` at its top level. The slug is taken
    /// from the frontmatter's `name:` if present, otherwise from the
    /// source directory's file name. Overwrites any existing skill at the
    /// same slug (so re-running is idempotent).
    pub async fn install_from_directory(
        &self,
        src: impl AsRef<Path>,
    ) -> Result<Skill> {
        let src = src.as_ref();
        let skill_md_src = src.join(SKILL_MD);
        if !skill_md_src.is_file() {
            return Err(thoth_core::Error::Config(format!(
                "{} does not contain a SKILL.md",
                src.display()
            )));
        }
        let text = tokio::fs::read_to_string(&skill_md_src).await?;
        let (name, description) = parse_skill_frontmatter(&text);
        let fallback_slug = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let slug = name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if fallback_slug.is_empty() {
                    "skill".to_string()
                } else {
                    fallback_slug.clone()
                }
            });
        let dest = self.root.join(SKILLS_DIR).join(&slug);
        if dest.exists() {
            tokio::fs::remove_dir_all(&dest).await?;
        }
        copy_dir_recursive(src, &dest).await?;

        Ok(Skill {
            meta: MemoryMeta::new(MemoryKind::Procedural),
            slug,
            description: description.unwrap_or_default(),
            path: dest
                .strip_prefix(&self.root)
                .unwrap_or(&dest)
                .to_path_buf(),
        })
    }

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
        let mut success_count = 0u64;
        let mut failure_count = 0u64;

        while let Some(next) = iter.peek() {
            if next.starts_with("### ") || next.starts_with("## ") || next.starts_with("# ") {
                break;
            }
            let n = iter.next().unwrap();
            if let Some((s, f)) = parse_counter_footer(n) {
                success_count = s;
                failure_count = f;
                continue;
            }
            advice.push_str(n);
            advice.push('\n');
        }

        out.push(Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger.trim().to_string(),
            advice: advice.trim_end().to_string(),
            success_count,
            failure_count,
        });
    }
    out
}

/// Parse the hidden `<!-- success: N / failure: N -->` counter footer that
/// `render_lesson` emits. Returns `None` if the line isn't a counter footer.
fn parse_counter_footer(line: &str) -> Option<(u64, u64)> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix("<!--")
        .and_then(|s| s.strip_suffix("-->"))?
        .trim();
    // Expected shape: `success: N / failure: N`.
    let mut success = None;
    let mut failure = None;
    for part in inner.split('/') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("success:") {
            success = v.trim().parse::<u64>().ok();
        } else if let Some(v) = part.strip_prefix("failure:") {
            failure = v.trim().parse::<u64>().ok();
        }
    }
    match (success, failure) {
        (Some(s), Some(f)) => Some((s, f)),
        _ => None,
    }
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

/// Rewrite a file by writing to a sibling `.tmp` and renaming atomically.
/// The final byte sequence on disk is exactly `body`.
async fn write_atomic(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, body.as_bytes()).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Render a single [`Fact`] as a level-3 heading block.
///
/// The first line of `f.text` becomes the heading, any remainder becomes the
/// body, and tags (if any) are appended on a `tags:` line. A trailing blank
/// line separates records so successive calls compose cleanly.
fn render_fact(f: &Fact) -> String {
    let title = first_line(&f.text);
    let body = remainder(&f.text);
    let mut out = String::new();
    out.push_str("### ");
    out.push_str(&title);
    out.push('\n');
    if !body.trim().is_empty() {
        out.push_str(body.trim_end());
        out.push('\n');
    }
    if !f.tags.is_empty() {
        out.push_str("tags: ");
        out.push_str(&f.tags.join(", "));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Render a single [`Lesson`] as a level-3 heading block. When either
/// counter is non-zero, a hidden `<!-- success: N / failure: N -->` footer
/// is emitted so the counts survive round-trips through the file.
fn render_lesson(l: &Lesson) -> String {
    let mut out = String::new();
    out.push_str("### ");
    out.push_str(l.trigger.trim());
    out.push('\n');
    if !l.advice.trim().is_empty() {
        out.push_str(l.advice.trim_end());
        out.push('\n');
    }
    if l.success_count > 0 || l.failure_count > 0 {
        out.push_str(&format!(
            "<!-- success: {} / failure: {} -->\n",
            l.success_count, l.failure_count
        ));
    }
    out.push('\n');
    out
}

/// Recursively copy `src` to `dest`. Creates `dest` (and parents) if needed.
/// Uses a breadth-first walk to avoid recursion-in-async-fn headaches.
async fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dest).await?;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&from).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            let child_from = entry.path();
            let child_to = to.join(entry.file_name());
            if ft.is_dir() {
                tokio::fs::create_dir_all(&child_to).await?;
                stack.push((child_from, child_to));
            } else if ft.is_file() {
                tokio::fs::copy(&child_from, &child_to).await?;
            }
            // Symlinks are skipped — skills are expected to be plain trees.
        }
    }
    Ok(())
}
