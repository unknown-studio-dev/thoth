//! Shared HTTP plumbing for the provider adapters.
//!
//! Each provider (`voyage`, `openai`, `cohere`) speaks a slightly different
//! wire format but they all bearer-auth a JSON POST, fail the same way, and
//! keep the same `(api_key, model, dim, base_url)` configuration surface.
//! This module centralises that plumbing so provider files only encode the
//! details that actually differ: the endpoint path, request body shape,
//! response shape, and any post-processing (index sort, bucket flattening).
//!
//! Feature-gated on the set of providers that pull in `reqwest`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thoth_core::{Error, Result};

/// Shared handle holding the reqwest client, credentials, model id, and
/// base URL. Providers compose this — they do not inherit from it.
#[derive(Debug, Clone)]
pub(crate) struct HttpEmbedderBase {
    pub client: reqwest::Client,
    pub api_key: String,
    pub model: String,
    pub dim: usize,
    pub base_url: String,
}

impl HttpEmbedderBase {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
        default_base_url: &str,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            dim,
            base_url: default_base_url.to_string(),
        }
    }

    /// Read `env_var` and return `Err(Error::Config)` if unset or
    /// whitespace-only. Centralises the provider `from_env()` guard so the
    /// two failure messages stay consistent.
    pub fn key_from_env(env_var: &str) -> Result<String> {
        let key =
            std::env::var(env_var).map_err(|_| Error::Config(format!("{env_var} not set")))?;
        if key.trim().is_empty() {
            return Err(Error::Config(format!("{env_var} is empty")));
        }
        Ok(key)
    }

    /// Bearer-auth JSON POST `{base_url}{path}`. Maps any reqwest error or
    /// non-2xx status into `Error::Provider("<provider> <status>: <body>")`.
    /// `Resp` is fully deserialised from the response body; providers pick
    /// their own shape, keeping this layer schema-agnostic.
    pub async fn post_json<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        provider: &'static str,
    ) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("{provider} {status}: {b}")));
        }
        resp.json::<Resp>()
            .await
            .map_err(|e| Error::Provider(e.to_string()))
    }
}
