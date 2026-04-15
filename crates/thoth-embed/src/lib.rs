//! # thoth-embed
//!
//! Feature-gated [`Embedder`] adapters. Enable at most the providers you
//! need via Cargo features: `voyage`, `openai`, `cohere`.
//!
//! All adapters require an API key supplied by the caller. Thoth does not
//! ship default credentials.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

#[cfg(feature = "voyage")]
pub mod voyage;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "cohere")]
pub mod cohere;

/// Re-export of the core trait for convenience.
pub use thoth_core::Embedder;
