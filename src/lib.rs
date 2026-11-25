//! JSON protocol translation used by the `codex_ai` gateway.

use std::fmt;

pub mod chat;
pub mod responses;

/// A malformed or unsupported request that cannot be sent to Codex.
#[derive(Debug)]
pub struct InvalidRequest(String);

impl fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidRequest {}
