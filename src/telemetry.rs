//! Opt-in diagnostics written to stderr, using only request metadata.

use std::fmt;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Request {
    id: u64,
    started: Instant,
}

/// A lightweight correlation ID and clock; disabled unless debug mode is on.
#[derive(Clone, Copy, Default)]
pub(super) struct Trace(Option<Request>);

impl Trace {
    pub(super) fn start(enabled: bool) -> Self {
        Self(enabled.then(|| Request {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
        }))
    }

    pub(super) fn event(self, event: fmt::Arguments<'_>) {
        if let Some(request) = self.0 {
            let _ = writeln!(
                io::stderr().lock(),
                "debug request={} elapsed_ms={} {event}",
                request.id,
                request.started.elapsed().as_millis()
            );
        }
    }
}
