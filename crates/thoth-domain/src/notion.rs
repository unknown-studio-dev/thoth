//! Notion ingestor — feature-gated under `notion`.
//!
//! Pulls pages from a Notion database whose schema includes:
//!
//! - `Title` (title) — becomes [`RemoteRule::title`].
//! - `Thoth.Context` (rich_text or select) — becomes `context`. Rows
//!   without this property are skipped (returned with empty context
//!   so `map_to_context` filters them out).
//! - `Thoth.Kind` (select) — one of `invariant|workflow|glossary|policy`.
//!   Defaults to `policy` when absent or unrecognized.
//! - `Thoth.Status` (select, optional) — `proposed|accepted|deprecated`.
//!   Informational only; snapshot writer always writes `Proposed`
//!   (human promotes via PR).
//!
//! Auth: `NOTION_TOKEN` (integration secret, `secret_…` or `ntn_…`).
//!
//! Note: this is an MVP adapter. It paginates but does not honour every
//! Notion rate-limit nuance; production callers should wrap it with a
//! governor.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::error::{DomainError, Result};
use crate::ingest::{DomainIngestor, IngestFilter};
use crate::types::{RemoteRule, RuleKind};

const NOTION_VERSION: &str = "2022-06-28";
const NOTION_BASE: &str = "https://api.notion.com/v1";

/// Notion ingestor. Construct via [`NotionIngestor::new`].
pub struct NotionIngestor {
    token: String,
    database_id: String,
    http: reqwest::Client,
}

impl NotionIngestor {
    /// Create a new ingestor targeting `database_id`.
    ///
    /// Reads `NOTION_TOKEN` from the environment. Fails with
    /// [`DomainError::MissingConfig`] if not set.
    pub fn new(database_id: impl Into<String>) -> Result<Self> {
        let token = std::env::var("NOTION_TOKEN").map_err(|_| {
            DomainError::MissingConfig(
                "notion".into(),
                "set NOTION_TOKEN to a Notion integration secret".into(),
            )
        })?;
        Ok(Self {
            token,
            database_id: database_id.into(),
            http: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl DomainIngestor for NotionIngestor {
    fn source_id(&self) -> &str {
        "notion"
    }

    async fn list(&self, filter: &IngestFilter) -> Result<Vec<RemoteRule>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut body = json!({ "page_size": 100 });
            if let Some(c) = cursor.as_ref() {
                body["start_cursor"] = json!(c);
            }
            if let Some(since) = filter.since {
                // Notion allows filtering by last_edited_time.
                body["filter"] = json!({
                    "timestamp": "last_edited_time",
                    "last_edited_time": { "on_or_after": format_rfc3339(since) }
                });
            }

            let url = format!("{NOTION_BASE}/databases/{}/query", self.database_id);
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.token)
                .header("Notion-Version", NOTION_VERSION)
                .json(&body)
                .send()
                .await
                .map_err(|e| DomainError::Source {
                    source_id: "notion".into(),
                    message: format!("request failed: {e}"),
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(DomainError::Source {
                    source_id: "notion".into(),
                    message: format!("HTTP {status}: {text}"),
                });
            }

            let page: DbQueryResp = resp.json().await.map_err(|e| DomainError::Source {
                source_id: "notion".into(),
                message: format!("decode error: {e}"),
            })?;

            for row in page.results {
                if let Some(rule) = to_remote_rule(&row) {
                    out.push(rule);
                    if out.len() >= filter.max_items {
                        return Ok(out);
                    }
                }
            }

            if page.has_more {
                cursor = page.next_cursor;
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn map_to_context(&self, rule: &RemoteRule) -> Option<String> {
        // Context already extracted from `Thoth.Context` property during
        // conversion. Empty string means "skip this page".
        if rule.context.trim().is_empty() {
            None
        } else {
            Some(rule.context.clone())
        }
    }
}

#[derive(Debug, Deserialize)]
struct DbQueryResp {
    results: Vec<Value>,
    has_more: bool,
    next_cursor: Option<String>,
}

fn to_remote_rule(row: &Value) -> Option<RemoteRule> {
    let id = row.get("id")?.as_str()?.to_string();
    let url = row
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let last_edited = row
        .get("last_edited_time")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or_else(OffsetDateTime::now_utc);

    let props = row.get("properties")?;
    let title = extract_title(props).unwrap_or_default();
    let context = extract_text_or_select(props, "Thoth.Context").unwrap_or_default();
    let kind_str = extract_text_or_select(props, "Thoth.Kind").unwrap_or_default();
    let kind = parse_kind(&kind_str);
    let tags = extract_multiselect(props, "Thoth.Tags");

    // Notion page bodies live in a separate /blocks endpoint. For an MVP
    // we surface just the title + a pointer; full body fetch is a
    // follow-up to keep request volume bounded in this first cut.
    let body = format!(
        "Title: {title}\n\nOpen in Notion: {url}\n\n(Body fetch is deferred — set up follow-up ingestion to populate.)"
    );

    Some(RemoteRule {
        id,
        source: "notion".into(),
        source_uri: url,
        context,
        kind,
        title,
        body,
        updated_at: last_edited,
        tags,
    })
}

fn extract_title(props: &Value) -> Option<String> {
    let obj = props.as_object()?;
    for (_, v) in obj {
        if v.get("type").and_then(Value::as_str) == Some("title") {
            let arr = v.get("title")?.as_array()?;
            let combined: String = arr
                .iter()
                .filter_map(|t| t.get("plain_text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            return Some(combined);
        }
    }
    None
}

fn extract_text_or_select(props: &Value, name: &str) -> Option<String> {
    let v = props.get(name)?;
    let ty = v.get("type")?.as_str()?;
    match ty {
        "select" => v
            .get("select")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        "rich_text" => {
            let arr = v.get("rich_text")?.as_array()?;
            Some(
                arr.iter()
                    .filter_map(|t| t.get("plain_text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
            )
        }
        _ => None,
    }
}

fn extract_multiselect(props: &Value, name: &str) -> Vec<String> {
    let Some(v) = props.get(name) else {
        return Vec::new();
    };
    let Some(arr) = v.get("multi_select").and_then(Value::as_array) else {
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
    fn parses_kind_leniently() {
        assert_eq!(parse_kind("invariant"), RuleKind::Invariant);
        assert_eq!(parse_kind("Workflow"), RuleKind::Workflow);
        assert_eq!(parse_kind(""), RuleKind::Policy);
        assert_eq!(parse_kind("weird"), RuleKind::Policy);
    }

    #[test]
    fn extracts_title_from_notion_shape() {
        let props = serde_json::json!({
            "Name": {
                "type": "title",
                "title": [
                    { "plain_text": "Refund " },
                    { "plain_text": "limit" }
                ]
            }
        });
        assert_eq!(extract_title(&props), Some("Refund limit".into()));
    }

    #[test]
    fn extracts_select_property() {
        let props = serde_json::json!({
            "Thoth.Kind": { "type": "select", "select": { "name": "invariant" } }
        });
        assert_eq!(
            extract_text_or_select(&props, "Thoth.Kind"),
            Some("invariant".into())
        );
    }

    #[test]
    fn missing_context_yields_empty() {
        let props = serde_json::json!({});
        assert_eq!(extract_text_or_select(&props, "Thoth.Context"), None);
    }
}
