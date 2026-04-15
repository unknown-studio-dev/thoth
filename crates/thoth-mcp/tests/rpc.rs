//! End-to-end smoke tests for the MCP server: we hand-craft JSON-RPC
//! messages, drive `Server::handle` directly, and assert on the result
//! payload shape. No real stdio involved.

use serde_json::{Value, json};
use thoth_mcp::{Server, proto::RpcIncoming};

async fn open(tmp: &tempfile::TempDir) -> Server {
    Server::open(tmp.path()).await.expect("server opens")
}

fn req(id: i64, method: &str, params: Value) -> RpcIncoming {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .unwrap()
}

#[tokio::test]
async fn initialize_advertises_server_info_and_capabilities() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = open(&tmp).await;

    let resp = srv
        .handle(req(1, "initialize", json!({})))
        .await
        .expect("response");
    let result = resp.result.expect("ok");

    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "thoth-mcp");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["capabilities"]["resources"].is_object());
}

#[tokio::test]
async fn tools_list_includes_recall_and_memory_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = open(&tmp).await;

    let resp = srv
        .handle(req(2, "tools/list", json!({})))
        .await
        .expect("response");
    let tools = resp.result.unwrap()["tools"].clone();
    let names: Vec<String> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();

    for expected in [
        "thoth_recall",
        "thoth_index",
        "thoth_remember_fact",
        "thoth_remember_lesson",
        "thoth_skills_list",
        "thoth_memory_show",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool {expected}"
        );
    }
}

#[tokio::test]
async fn remember_fact_then_memory_show_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = open(&tmp).await;

    // remember a fact
    let resp = srv
        .handle(req(
            3,
            "tools/call",
            json!({
                "name": "thoth_remember_fact",
                "arguments": { "text": "auth uses RS256 JWTs", "tags": ["auth"] }
            }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.result.as_ref().unwrap()["isError"], false);

    // then read it back via memory_show
    let resp = srv
        .handle(req(4, "tools/call", json!({ "name": "thoth_memory_show" })))
        .await
        .expect("response");
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("auth uses RS256 JWTs"), "got: {text}");
}

#[tokio::test]
async fn resources_list_and_read_markdown_files() {
    let tmp = tempfile::tempdir().unwrap();
    // seed MEMORY.md
    tokio::fs::write(tmp.path().join("MEMORY.md"), "### fact\nhello world\n")
        .await
        .unwrap();

    let srv = open(&tmp).await;

    let resp = srv
        .handle(req(5, "resources/list", json!({})))
        .await
        .expect("response");
    let uris: Vec<String> = resp.result.unwrap()["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.iter().any(|u| u == "thoth://memory/MEMORY.md"));
    assert!(uris.iter().any(|u| u == "thoth://memory/LESSONS.md"));

    let resp = srv
        .handle(req(
            6,
            "resources/read",
            json!({ "uri": "thoth://memory/MEMORY.md" }),
        ))
        .await
        .expect("response");
    let text = resp.result.unwrap()["contents"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("hello world"));
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = open(&tmp).await;

    let resp = srv
        .handle(req(7, "nonexistent/method", json!({})))
        .await
        .expect("response");
    let err = resp.error.expect("error");
    assert_eq!(err.code, -32601);
}

#[tokio::test]
async fn initialized_notification_returns_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = open(&tmp).await;

    // No `id` → notification.
    let msg: RpcIncoming = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }))
    .unwrap();

    assert!(srv.handle(msg).await.is_none());
}
