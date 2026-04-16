//! Asana ingestor — feature-gated under `asana`.
//!
//! Pulls tasks from an Asana project. Tasks are expected to carry
//! custom fields (or name prefixes) that Thoth uses for routing.
//!
//! Expected shape:
//!
//! - `name` → [`RemoteRule::title`].
//! - `notes` → [`RemoteRule::body`].
//! - A custom field named `Thoth.Context` (enum or text) → `context`.
//! - A custom field named `Thoth.Kind` (enum, `invariant|workflow|glossary|policy`).
//!
//! Tasks without `Thoth.Context` are ingested with empty context and
//! then dropped by `map_to_context` — the ADR 0001 rule that PMs must
//! opt a task in is enforced at this boundary.
//!
//! Auth: `ASANA_TOKEN` (personal access token).
//!
//! API docs: <https://developers.asana.com/reference/gettasks>.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

use crate::error::{DomainError, Result};
use crate::ingest::{DomainIngestor, IngestFilter};
use crate::types::{RemoteRule, RuleKind};

const ASANA_BASE: &str = "https://app.asana.com/api/1.0";
const OPT_FIELDS: &str = "gid,name,notes,modified_at,permalink_url,custom_fields.name,\
    custom_fields.type,custom_fields.enum_value.name,custom_fields.text_value,tags.name";

/// Asana ingestor. Scoped to one project.
pub struct AsanaIngestor {
    token: String,
    project_gid: String,
    http: reqwest::Client,
}

impl AsanaIngestor {
    /// Create an ingestor for `project_gid`. Reads `ASANA_TOKEN` from env.
    pub fn new(project_gid: impl Into<String>) -> Result<Self> {
        let token = std::env::var("ASANA_TOKEN").map_err(|_| {
            DomainError::MissingConfig(
                "asana".into(),
                "set ASANA_TOKEN to an Asana personal access token".into(),
            )
        })?;
        Ok(Self {
            token,
            project_gid: project_gid.into(),
            http: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl DomainIngestor for AsanaIngestor {
    fn source_id(&self) -> &str {
        "asana"
    }

    async fn list(&self, filter: &IngestFilter) -> Result<Vec<RemoteRule>> {
        let mut out = Vec::new();
        let mut offset: Option<String> = None;

        loop {
            let mut url =
                reqwest::Url::parse(&format!("{ASANA_BASE}/projects/{}/tasks", self.project_gid))
                    .map_err(|e| DomainError::Source {
                    source_id: "asana".into(),
                    message: format!("bad URL: {e}"),
                })?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("opt_fields", OPT_FIELDS);
                q.append_pair("limit", "100");
                if let Some(m) = filter.since {
                    q.append_pair("modified_since", &format_rfc3339(m));
                }
                if let Some(o) = offset.as_ref() {
                    q.append_pair("offset", o);
                }
            }

            let resp = self
                .http
                .get(url)
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(|e| DomainError::Source {
                    source_id: "asana".into(),
                    message: format!("request failed: {e}"),
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(DomainError::Source {
                    source_id: "asana".into(),
                    message: format!("HTTP {status}: {text}"),
                });
            }

            let page: TasksResp = resp.json().await.map_err(|e| DomainError::Source {
                source_id: "asana".into(),
                message: format!("decode error: {e}"),
            })?;

            for t in page.data {
                if let Some(rule) = to_remote_rule(&t) {
                    out.push(rule);
                    if out.len() >= filter.max_items {
                        return Ok(out);
                    }
                }
            }

            match page.next_page.and_then(|p| p.offset) {
                Some(o) => offset = Some(o),
                None => break,
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct TasksResp {
    data: Vec<Value>,
    next_page: Option<NextPage>,
}

#[derive(Debug, Deserialize)]
struct NextPage {
    offset: Option<String>,
}

fn to_remote_rule(task: &Value) -> Option<RemoteRule> {
    let gid = task.get("gid")?.as_str()?.to_string();
    let name = task
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let notes = task
        .get("notes")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let url = task
        .get("permalink_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let updated_at = task
        .get("modified_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or_else(OffsetDateTime::now_utc);

    let (context, kind) = read_custom_fields(task);
    let tags = read_tags(task);

    Some(RemoteRule {
        id: gid,
        source: "asana".into(),
        source_uri: url,
        context,
        kind,
        title: name,
        body: notes,
        updated_at,
        tags,
    })
}

fn read_custom_fields(task: &Value) -> (String, RuleKind) {
    let mut context = String::new();
    let mut kind = RuleKind::Policy;
    let Some(cfs) = task.get("custom_fields").and_then(Value::as_array) else {
        return (context, kind);
    };
    for cf in cfs {
        let Some(name) = cf.get("name").and_then(Value::as_str) else {
            continue;
        };
        let text = cf
            .get("enum_value")
            .and_then(|e| e.get("name"))
            .and_then(Value::as_str)
            .or_else(|| cf.get("text_value").and_then(Value::as_str))
            .unwrap_or("");
        match name {
            "Thoth.Context" => context = text.to_string(),
            "Thoth.Kind" => kind = parse_kind(text),
            _ => {}
        }
    }
    (context, kind)
}

fn read_tags(task: &Value) -> Vec<String> {
    let Some(arr) = task.get("tags").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn parse_kind(s: &str) -> RuleKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "invariant" => RuleKind::Invariant,
        "workflow" => RuleKind::Workflow,
        "glossary" => RuleKind::Glossary,
        _ => RuleKind::Policy,
    }
}

fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

fn format_rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_custom_fields() {
        let task = serde_json::json!({
            "gid": "1",
            "name": "R-1",
            "notes": "body",
            "modified_at": "2026-04-16T08:00:00Z",
            "permalink_url": "https://asana/1",
            "custom_fields": [
                { "name": "Thoth.Context", "type": "enum",
                  "enum_value": { "name": "billing" } },
                { "name": "Thoth.Kind", "type": "enum",
                  "enum_value": { "name": "invariant" } }
            ]
        });
        let rule = to_remote_rule(&task).unwrap();
        assert_eq!(rule.context, "billing");
        assert_eq!(rule.kind, RuleKind::Invariant);
        assert_eq!(rule.title, "R-1");
    }

    #[test]
    fn missing_fields_default_gracefully() {
        let task = serde_json::json!({ "gid": "2", "name": "no-meta" });
        let rule = to_remote_rule(&task).unwrap();
        assert_eq!(rule.context, "");
        assert_eq!(rule.kind, RuleKind::Policy);
    }
}
