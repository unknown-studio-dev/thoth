//! End-to-end smoke tests for `thoth-parse`.
//!
//! Run with `cargo test -p thoth-parse`.

use std::time::Duration;
use tempfile::tempdir;
use thoth_parse::{
    LanguageRegistry,
    walk::{WalkOptions, walk_sources},
};

const RUST_SAMPLE: &str = r#"
use std::collections::HashMap;

pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

pub struct User {
    pub id: u64,
    pub name: String,
}

impl User {
    pub fn new(id: u64, name: String) -> Self {
        Self { id, name }
    }
}

pub trait Greeter {
    fn greet(&self) -> String;
}
"#;

const PYTHON_SAMPLE: &str = r#"
import os
from typing import List

def add(a: int, b: int) -> int:
    return a + b

class Box:
    def __init__(self, items: List[int]):
        self.items = items

    def total(self) -> int:
        return sum(self.items)
"#;

#[tokio::test]
async fn parses_rust_and_extracts_symbols() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.rs");
    tokio::fs::write(&path, RUST_SAMPLE).await.unwrap();

    let reg = LanguageRegistry::new();
    let (chunks, table) = thoth_parse::parse_file(&reg, &path).await.unwrap();

    // At least: greet, User, impl User, Greeter → 4 chunks.
    assert!(
        chunks.len() >= 4,
        "expected >=4 chunks, got {}",
        chunks.len()
    );

    let names: Vec<_> = table
        .symbols
        .iter()
        .map(|s| s.fqn.rsplit("::").next().unwrap())
        .collect();
    assert!(names.contains(&"greet"), "missing greet in {names:?}");
    assert!(names.contains(&"User"), "missing User in {names:?}");
    assert!(names.contains(&"Greeter"), "missing Greeter in {names:?}");

    assert!(
        table.imports.iter().any(|i| i.contains("HashMap")),
        "imports missing HashMap: {:?}",
        table.imports
    );
}

#[tokio::test]
async fn parses_python_and_extracts_symbols() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.py");
    tokio::fs::write(&path, PYTHON_SAMPLE).await.unwrap();

    let reg = LanguageRegistry::new();
    let (chunks, table) = thoth_parse::parse_file(&reg, &path).await.unwrap();

    assert!(
        chunks.len() >= 2,
        "expected >=2 chunks, got {}",
        chunks.len()
    );

    let names: Vec<_> = table
        .symbols
        .iter()
        .map(|s| s.fqn.rsplit("::").next().unwrap())
        .collect();
    assert!(names.contains(&"add"), "missing add");
    assert!(names.contains(&"Box"), "missing Box");
}

#[tokio::test]
async fn walk_respects_gitignore_and_extensions() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "fn main() {}")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("b.py"), "def f(): pass")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("README.md"), "# hi")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.rs\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.rs"), "fn x() {}")
        .await
        .unwrap();

    let reg = LanguageRegistry::new();
    let files = walk_sources(dir.path(), &reg, &WalkOptions::default());

    let names: Vec<_> = files
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
        .collect();
    assert!(names.contains(&"a.rs".to_string()));
    assert!(names.contains(&"b.py".to_string()));
    assert!(
        !names.contains(&"README.md".to_string()),
        "no grammar for md"
    );
    assert!(
        !names.contains(&"ignored.rs".to_string()),
        "should be gitignored"
    );
}

#[tokio::test]
async fn watcher_emits_events_on_change() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("watched.rs");
    tokio::fs::write(&file, "fn a() {}").await.unwrap();

    let mut w = thoth_parse::watch::Watcher::watch(dir.path(), 64).unwrap();

    // Give notify a moment to register the watch before we mutate.
    tokio::time::sleep(Duration::from_millis(150)).await;

    tokio::fs::write(&file, "fn a() {} fn b() {}")
        .await
        .unwrap();

    // Receive with a timeout so the test can't hang on a flaky FS.
    let ev = tokio::time::timeout(Duration::from_secs(3), w.recv())
        .await
        .expect("no event within 3s")
        .expect("channel closed unexpectedly");

    let ev_dbg = format!("{ev:?}");
    use thoth_core::Event::*;
    match ev {
        FileChanged { path, .. } | FileDeleted { path, .. } => {
            assert!(path.ends_with("watched.rs"), "unexpected path: {path:?}");
        }
        _ => panic!("unexpected event: {ev_dbg}"),
    }
}
