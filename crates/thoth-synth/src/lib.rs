//! # thoth-synth
//!
//! Feature-gated [`Synthesizer`] adapters. Enable providers via Cargo
//! features: `anthropic`.
//!
//! Used by Mode::Full for three things:
//! - answer synthesis with citations,
//! - query rewriting,
//! - self-critique on failed outcomes (the nudge / lesson loop).

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

#[cfg(feature = "anthropic")]
pub mod anthropic;

/// Re-export of the core trait for convenience.
pub use thoth_core::Synthesizer;
