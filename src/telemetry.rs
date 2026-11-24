//! Opt-in diagnostics written to stderr, using only request metadata.

use std::fmt;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogLevel {
    Off,
    Info,
    Debug,
}

#[derive(Clone, Copy)]
struct Request {
    id: u64,
    started: Instant,
    level: LogLevel,
}

impl Request {
    fn log(self, level: &str, event: fmt::Arguments<'_>) {
        let _ = writeln!(
            io::stderr().lock(),
            "{level} request={} elapsed_ms={} {event}",
            self.id,
            self.started.elapsed().as_millis()
        );
    }
}

/// A lightweight correlation ID and clock for opt-in request logs.
#[derive(Clone, Copy, Default)]
pub(super) struct Trace(Option<Request>);

impl Trace {
    pub(super) fn start(level: LogLevel) -> Self {
        Self((level != LogLevel::Off).then(|| Request {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            level,
        }))
    }

    pub(super) fn info(self, event: fmt::Arguments<'_>) {
        if let Some(request) = self.0 {
            request.log("info", event);
        }
    }

    pub(super) fn event(self, event: fmt::Arguments<'_>) {
        if let Some(request) = self.0
            && request.level == LogLevel::Debug
        {
            request.log("debug", event);
        }
    }
}
