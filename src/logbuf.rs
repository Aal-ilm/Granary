//! Keeps the most recent log events in memory so the web UI can show them.

use std::{
    collections::VecDeque,
    fmt::Write,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context};

const CAPACITY: usize = 1000;

#[derive(Clone, Serialize)]
pub struct LogEntry {
    /// Unix time in milliseconds.
    pub ts: u64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<LogEntry>>>);

impl LogBuffer {
    /// Newest first.
    pub fn entries(&self) -> Vec<LogEntry> {
        self.0.lock().unwrap().iter().rev().cloned().collect()
    }
}

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for LogBuffer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let entry = LogEntry {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            level: meta.level().as_str(),
            target: meta.target().to_string(),
            message: v.0.trim().to_string(),
        };
        let mut buf = self.0.lock().unwrap();
        if buf.len() == CAPACITY {
            buf.pop_front();
        }
        buf.push_back(entry);
    }
}
