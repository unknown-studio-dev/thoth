//! # thoth-graph
//!
//! Symbol, call, import, and reference graph built on top of
//! [`thoth_store::KvStore`]. This is the spine of Mode::Zero retrieval: it
//! answers "who calls X", "what does X call", "which modules import Y"
//! without any LLM or embedding.
//!
//! Design:
//!
//! - Every parsed symbol becomes a [`Node`] keyed by its fully qualified
//!   name (FQN). Nodes carry the path + line of their declaration.
//! - Every call, import, extends, references relationship becomes an
//!   [`Edge`]. Edges are stored with the underlying KV as
//!   `"<src>|<kind>|<dst>"`, so outgoing-edge lookups are a prefix scan.
//! - Traversal is plain BFS bounded by `depth`; fine at indexing scale.
//!
//! See `DESIGN.md` §4 and §5.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thoth_core::Result;
use thoth_store::{EdgeRow, KvStore, NodeRow};

/// A node in the code graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    /// Fully qualified name (primary key).
    pub fqn: String,
    /// Coarse kind (`"function"`, `"type"`, `"trait"`, `"module"`,
    /// `"binding"`).
    pub kind: String,
    /// Source path.
    pub path: PathBuf,
    /// 1-based declaration line.
    pub line: u32,
}

/// Edge kinds tracked by the graph.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `A` calls `B`.
    Calls,
    /// `A` imports module `B`.
    Imports,
    /// `A` references symbol `B`.
    References,
    /// `A` extends / implements `B`.
    Extends,
    /// `A` is declared in module `B`.
    DeclaredIn,
}

impl EdgeKind {
    /// Canonical on-disk tag.
    pub fn tag(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::References => "references",
            EdgeKind::Extends => "extends",
            EdgeKind::DeclaredIn => "declared_in",
        }
    }

    /// Parse a tag back into an [`EdgeKind`].
    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "calls" => EdgeKind::Calls,
            "imports" => EdgeKind::Imports,
            "references" => EdgeKind::References,
            "extends" => EdgeKind::Extends,
            "declared_in" => EdgeKind::DeclaredIn,
            _ => return None,
        })
    }
}

/// An edge between two nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    /// Source FQN.
    pub from: String,
    /// Destination FQN.
    pub to: String,
    /// Edge kind.
    pub kind: EdgeKind,
}

/// Graph handle — cheap to clone (wraps a shared [`KvStore`]).
#[derive(Clone)]
pub struct Graph {
    kv: KvStore,
}

impl Graph {
    /// Wrap an existing KV store.
    pub fn new(kv: KvStore) -> Self {
        Self { kv }
    }

    /// Insert or update a node.
    pub async fn upsert_node(&self, n: Node) -> Result<()> {
        let payload = serde_json::json!({
            "path": n.path,
            "line": n.line,
        });
        self.kv
            .put_node(NodeRow {
                id: n.fqn,
                kind: n.kind,
                payload,
            })
            .await
    }

    /// Insert or update an edge.
    pub async fn upsert_edge(&self, e: Edge) -> Result<()> {
        self.kv
            .put_edge(EdgeRow {
                src: e.from,
                dst: e.to,
                kind: e.kind.tag().to_string(),
                payload: serde_json::Value::Null,
            })
            .await
    }

    /// Fetch a node by FQN.
    pub async fn get(&self, fqn: &str) -> Result<Option<Node>> {
        Ok(self.kv.get_node(fqn).await?.map(row_to_node))
    }

    /// BFS callees: `fqn` → what `fqn` calls, transitively, up to `depth`.
    pub async fn callees(&self, fqn: &str, depth: usize) -> Result<Vec<Node>> {
        self.bfs(fqn, depth, Direction::Out, Some(EdgeKind::Calls))
            .await
    }

    /// BFS callers: who calls `fqn`, transitively, up to `depth`.
    pub async fn callers(&self, fqn: &str, depth: usize) -> Result<Vec<Node>> {
        self.bfs(fqn, depth, Direction::In, Some(EdgeKind::Calls))
            .await
    }

    /// BFS over every edge kind in both directions — useful for "related
    /// code" fan-outs in retrieval.
    pub async fn neighbors(&self, fqn: &str, depth: usize) -> Result<Vec<Node>> {
        self.bfs(fqn, depth, Direction::Both, None).await
    }

    /// Direct outgoing edges of any kind.
    pub async fn outgoing(&self, fqn: &str) -> Result<Vec<Edge>> {
        Ok(self
            .kv
            .edges_from(fqn)
            .await?
            .into_iter()
            .filter_map(row_to_edge)
            .collect())
    }

    /// Direct incoming edges of any kind.
    pub async fn incoming(&self, fqn: &str) -> Result<Vec<Edge>> {
        Ok(self
            .kv
            .edges_to(fqn)
            .await?
            .into_iter()
            .filter_map(row_to_edge)
            .collect())
    }

    // ---- internal --------------------------------------------------------

    async fn bfs(
        &self,
        start: &str,
        depth: usize,
        dir: Direction,
        only: Option<EdgeKind>,
    ) -> Result<Vec<Node>> {
        if depth == 0 {
            return Ok(Vec::new());
        }
        let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
        let mut frontier: VecDeque<(String, usize)> = VecDeque::from([(start.to_string(), 0)]);
        let mut out = Vec::new();

        while let Some((cur, d)) = frontier.pop_front() {
            if d >= depth {
                continue;
            }
            let next_ids = self.step(&cur, dir, only).await?;
            for nid in next_ids {
                if !seen.insert(nid.clone()) {
                    continue;
                }
                if let Some(node) = self.get(&nid).await? {
                    out.push(node);
                }
                frontier.push_back((nid, d + 1));
            }
        }
        Ok(out)
    }

    async fn step(&self, cur: &str, dir: Direction, only: Option<EdgeKind>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if matches!(dir, Direction::Out | Direction::Both) {
            for e in self.outgoing(cur).await? {
                if only.is_none_or(|k| k == e.kind) {
                    out.push(e.to);
                }
            }
        }
        if matches!(dir, Direction::In | Direction::Both) {
            for e in self.incoming(cur).await? {
                if only.is_none_or(|k| k == e.kind) {
                    out.push(e.from);
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Out,
    In,
    Both,
}

// ---- helpers ---------------------------------------------------------------

fn row_to_node(row: NodeRow) -> Node {
    let path = row
        .payload
        .get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_default();
    let line = row
        .payload
        .get("line")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Node {
        fqn: row.id,
        kind: row.kind,
        path,
        line,
    }
}

fn row_to_edge(row: EdgeRow) -> Option<Edge> {
    Some(Edge {
        from: row.src,
        to: row.dst,
        kind: EdgeKind::from_tag(&row.kind)?,
    })
}
