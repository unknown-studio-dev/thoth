//! Mock-HTTP tests for the provider adapters. Each provider test is
//! feature-gated so the test file compiles in every feature combination.
//!
//! Run the whole surface with:
//! `cargo test -p thoth-embed --features voyage,openai,cohere`

#![cfg(any(feature = "voyage", feature = "openai", feature = "cohere"))]

use serde_json::json;
use thoth_core::Embedder;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---- voyage ---------------------------------------------------------------

#[cfg(feature = "voyage")]
mod voyage_tests {
    use super::*;
    use thoth_embed::voyage::VoyageEmbedder;

    fn embedder(server: &MockServer) -> VoyageEmbedder {
        VoyageEmbedder::new("test-key", "voyage-code-3", 4).with_base_url(server.uri())
    }

    #[tokio::test]
    async fn empty_input_short_circuits_without_hitting_server() {
        // No mock registered; if the adapter calls out, this panics.
        let server = MockServer::start().await;
        let emb = embedder(&server);
        assert!(emb.embed_batch(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn happy_path_restores_order_by_index() {
        let server = MockServer::start().await;
        // Respond with items OUT of index order — adapter must sort by index.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"index": 1, "embedding": [0.1, 0.2, 0.3, 0.4]},
                    {"index": 0, "embedding": [1.0, 1.1, 1.2, 1.3]},
                ],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let emb = embedder(&server);
        let out = emb.embed_batch(&["a", "b"]).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![1.0, 1.1, 1.2, 1.3]);
        assert_eq!(out[1], vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(emb.dim(), 4);
        assert_eq!(emb.model_id(), "voyage-code-3");
    }

    #[tokio::test]
    async fn error_status_maps_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let err = embedder(&server)
            .embed_batch(&["oops"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("voyage"), "err={err}");
        assert!(err.contains("401"), "err={err}");
    }

    #[tokio::test]
    async fn mismatched_count_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                // Asked for 2 texts, server returns 1 — adapter must error.
                "data": [{"index": 0, "embedding": [0.0, 0.0, 0.0, 0.0]}],
            })))
            .mount(&server)
            .await;

        let err = embedder(&server)
            .embed_batch(&["a", "b"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected 2"), "err={err}");
    }

    #[tokio::test]
    async fn from_env_rejects_missing_and_empty_key() {
        // SAFETY: tests in the same file share a process; this sets/unsets
        // an env var only for keys this module is aware of. No other test in
        // this crate reads VOYAGE_API_KEY, so there is no cross-test race.
        unsafe {
            std::env::remove_var("VOYAGE_API_KEY");
        }
        assert!(VoyageEmbedder::from_env().is_err());

        unsafe {
            std::env::set_var("VOYAGE_API_KEY", "   ");
        }
        assert!(VoyageEmbedder::from_env().is_err());

        unsafe {
            std::env::remove_var("VOYAGE_API_KEY");
        }
    }
}

// ---- openai ---------------------------------------------------------------

#[cfg(feature = "openai")]
mod openai_tests {
    use super::*;
    use thoth_embed::openai::OpenAiEmbedder;

    fn embedder(server: &MockServer) -> OpenAiEmbedder {
        OpenAiEmbedder::new("sk-test", "text-embedding-3-small", 3).with_base_url(server.uri())
    }

    #[tokio::test]
    async fn happy_path_sorts_by_index() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"index": 2, "embedding": [2.0, 2.1, 2.2]},
                    {"index": 0, "embedding": [0.0, 0.1, 0.2]},
                    {"index": 1, "embedding": [1.0, 1.1, 1.2]},
                ],
            })))
            .mount(&server)
            .await;

        let out = embedder(&server)
            .embed_batch(&["a", "b", "c"])
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![0.0, 0.1, 0.2]);
        assert_eq!(out[1], vec![1.0, 1.1, 1.2]);
        assert_eq!(out[2], vec![2.0, 2.1, 2.2]);
    }

    #[tokio::test]
    async fn error_status_surfaces_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"error":"rate_limited"}"#))
            .mount(&server)
            .await;

        let err = embedder(&server)
            .embed_batch(&["x"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("openai"), "err={err}");
        assert!(err.contains("429"), "err={err}");
        assert!(err.contains("rate_limited"), "err={err}");
    }

    #[tokio::test]
    async fn empty_input_does_not_hit_server() {
        let server = MockServer::start().await;
        let out = embedder(&server).embed_batch(&[]).await.unwrap();
        assert!(out.is_empty());
    }
}

// ---- cohere ---------------------------------------------------------------

#[cfg(feature = "cohere")]
mod cohere_tests {
    use super::*;
    use thoth_embed::cohere::CohereEmbedder;

    fn embedder(server: &MockServer) -> CohereEmbedder {
        CohereEmbedder::new("cohere-test", "embed-english-v3.0", 2).with_base_url(server.uri())
    }

    #[tokio::test]
    async fn happy_path_preserves_input_order() {
        let server = MockServer::start().await;
        // Cohere returns embeddings in input order inside embeddings.float —
        // adapter must NOT sort (unlike openai/voyage).
        Mock::given(method("POST"))
            .and(path("/embed"))
            .and(header("authorization", "Bearer cohere-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "embeddings": {
                    "float": [
                        [0.1, 0.2],
                        [0.3, 0.4],
                    ]
                },
            })))
            .mount(&server)
            .await;

        let out = embedder(&server).embed_batch(&["x", "y"]).await.unwrap();
        assert_eq!(out, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    }

    #[tokio::test]
    async fn missing_float_bucket_defaults_to_empty_and_mismatch_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "embeddings": {}
            })))
            .mount(&server)
            .await;

        let err = embedder(&server)
            .embed_batch(&["x"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cohere"), "err={err}");
        assert!(err.contains("expected 1"), "err={err}");
    }

    #[tokio::test]
    async fn server_error_surfaces_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embed"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;

        let err = embedder(&server)
            .embed_batch(&["x"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"));
        assert!(err.contains("internal"));
    }
}
