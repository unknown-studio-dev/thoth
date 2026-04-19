use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug)]
pub enum ProjectsCmd {
    /// List all registered projects with their slugs and paths.
    List,
    /// Show which root the current directory resolves to.
    Which,
    /// Move `./.thoth/` to `~/.thoth/projects/{slug}/` and update
    /// hooks + MCP to point to the new location.
    Migrate {
        /// Print what would happen without modifying anything.
        #[arg(long)]
        dry_run: bool,
        /// Delete the local `.thoth/` after a successful copy.
        #[arg(long)]
        rm_local: bool,
    },
}

/// Resolve the `.thoth/` data root via a 4-step chain:
///
/// 1. Explicit `--root` flag (highest priority)
/// 2. `$THOTH_ROOT` env var
/// 3. Project-local `./.thoth/` (backwards compat)
/// 4. Global `~/.thoth/projects/{slug}/`
pub fn resolve_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(root) = explicit {
        return root.to_path_buf();
    }
    if let Ok(env) = std::env::var("THOTH_ROOT") {
        let p = PathBuf::from(env);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    let local = PathBuf::from(".thoth");
    if local.is_dir() {
        return local;
    }
    if let Some(home) = home_dir()
        && let Ok(cwd) = std::env::current_dir()
    {
        let slug = project_slug(&cwd);
        return home.join(".thoth").join("projects").join(slug);
    }
    local
}

/// Deterministic 12-char hex slug from a project path.
pub fn project_slug(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    hash.to_hex()[..12].to_string()
}

/// Register a project in `~/.thoth/projects.json` so `thoth projects list`
/// can map slugs back to paths.
pub fn register_project(slug: &str, project_path: &Path) -> anyhow::Result<()> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("$HOME not set"))?;
    let global_root = home.join(".thoth");
    std::fs::create_dir_all(&global_root)?;

    let registry_path = global_root.join("projects.json");
    let mut map: serde_json::Map<String, serde_json::Value> = if registry_path.is_file() {
        let content = std::fs::read_to_string(&registry_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let canonical = project_path
        .canonicalize()
        .unwrap_or_else(|_| project_path.to_path_buf());
    map.insert(
        slug.to_string(),
        serde_json::Value::String(canonical.to_string_lossy().into_owned()),
    );

    let json = serde_json::to_string_pretty(&serde_json::Value::Object(map))?;
    std::fs::write(&registry_path, json)?;
    Ok(())
}

/// Returns true when the resolved root lives under `~/.thoth/projects/`.
pub fn is_global_root(root: &Path) -> bool {
    if let Some(home) = home_dir() {
        let global_prefix = home.join(".thoth").join("projects");
        root.starts_with(&global_prefix)
    } else {
        false
    }
}

/// Compute the global root for the current working directory.
/// Used by `thoth setup --global`.
pub fn global_root_for_cwd() -> anyhow::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("$HOME not set"))?;
    let cwd = std::env::current_dir()?;
    let slug = project_slug(&cwd);
    Ok(home.join(".thoth").join("projects").join(slug))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ---------------------------------------------------------- projects commands

pub fn cmd_projects_which(root: &std::path::Path) -> anyhow::Result<()> {
    println!("{}", root.display());
    if is_global_root(root) {
        let cwd = std::env::current_dir()?;
        let slug = project_slug(&cwd);
        println!("  slug: {slug}");
        println!("  project: {}", cwd.display());
        println!("  mode: global");
    } else {
        println!("  mode: local");
    }
    Ok(())
}

pub fn cmd_projects_list() -> anyhow::Result<()> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("$HOME not set"))?;
    let registry_path = PathBuf::from(home).join(".thoth").join("projects.json");
    if !registry_path.is_file() {
        println!("No projects registered yet. Run `thoth setup` in a project directory.");
        return Ok(());
    }
    let content = std::fs::read_to_string(&registry_path)?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content)?;
    if map.is_empty() {
        println!("No projects registered.");
        return Ok(());
    }
    for (slug, path) in &map {
        let path_str = path.as_str().unwrap_or("?");
        let exists = std::path::Path::new(path_str).is_dir();
        let marker = if exists { " " } else { "!" };
        println!("{marker} {slug}  {path_str}");
    }
    Ok(())
}

pub async fn cmd_projects_migrate(dry_run: bool, rm_local: bool) -> anyhow::Result<()> {
    let local = PathBuf::from(".thoth");
    if !local.is_dir() {
        anyhow::bail!("No local .thoth/ directory found in the current project.");
    }

    let dest = global_root_for_cwd()?;
    let cwd = std::env::current_dir()?;
    let slug = project_slug(&cwd);

    println!("migrate: .thoth/ → {}", dest.display());
    println!("  slug: {slug}");

    if dest.is_dir() {
        anyhow::bail!(
            "Destination already exists: {}\n  \
             Remove it first or use `thoth setup --global` for a fresh install.",
            dest.display()
        );
    }

    if dry_run {
        println!("  (dry run — no changes made)");
        return Ok(());
    }

    // Copy the entire .thoth/ tree to the global location.
    std::fs::create_dir_all(dest.parent().unwrap())?;
    copy_dir_recursive(&local, &dest)?;
    println!("  copied {} → {}", local.display(), dest.display());

    // Register in projects.json.
    register_project(&slug, &cwd)?;
    println!("  registered in ~/.thoth/projects.json");

    // Re-run hook + MCP install so THOTH_ROOT points to the new location.
    crate::hooks::install_all(crate::hooks::Scope::Project, &dest).await?;
    println!("  updated hooks + MCP to point to {}", dest.display());

    if rm_local {
        std::fs::remove_dir_all(&local)?;
        println!("  removed local .thoth/");
    } else {
        println!("  local .thoth/ kept (remove with `rm -rf .thoth/` or rerun with --rm-local)");
    }

    println!("done. Run `thoth projects which` to verify.");
    Ok(())
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ty.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
        // Skip symlinks, sockets (mcp.sock), etc.
    }
    Ok(())
}
