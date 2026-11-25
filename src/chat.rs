//! Chat Completions text, images, and function tools over Responses.

mod completion;
mod request;

use serde_json::Value;

pub use completion::{ChatStream, completion};
pub use request::{ChatOptions, into_responses};

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.trim().is_empty())
}
