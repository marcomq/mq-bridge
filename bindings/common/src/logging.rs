//! Shared tracing → host-logging bridge helpers for the bindings.
//!
//! The core library emits through the `tracing` facade but installs no
//! subscriber (correct library behaviour). Each binding is the application
//! boundary for pip/npm users, so it installs a subscriber that forwards events
//! into the host language's native logging (Python `logging`, a Node callback).
//! This module holds the transport-agnostic pieces: flattening a `tracing::Event`
//! into a plain record and building the level filter. The actual `Layer` that
//! delivers into Python/JS lives in each binding.

use std::fmt::Write as _;

use tracing::field::{Field, Visit};
use tracing::{Event, Level};
use tracing_subscriber::EnvFilter;

/// A `tracing` event flattened to plain strings, ready to hand to a host logger.
#[derive(Debug)]
pub struct LogEvent {
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogEvent {
    /// Lowercase level name (`error`/`warn`/`info`/`debug`/`trace`).
    pub fn level_str(&self) -> &'static str {
        match self.level {
            Level::ERROR => "error",
            Level::WARN => "warn",
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        }
    }

    /// The `logging` module's numeric level. TRACE has no stdlib equivalent, so
    /// it maps below DEBUG (5).
    pub fn python_levelno(&self) -> i32 {
        match self.level {
            Level::ERROR => 40,
            Level::WARN => 30,
            Level::INFO => 20,
            Level::DEBUG => 10,
            Level::TRACE => 5,
        }
    }

    /// Target as a `logging`-style dotted logger name so the Python hierarchy
    /// works (`mq_bridge::route` → `mq_bridge.route`, a child of `mq_bridge`).
    pub fn python_logger_name(&self) -> String {
        self.target.replace("::", ".")
    }
}

/// Collects the event's `message` field and appends any structured fields.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn push_field(&mut self, name: &str, value: &dyn std::fmt::Debug) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{name}={value:?}");
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.push_field(field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.push_field(field.name(), &value);
        }
    }
}

/// Flatten a `tracing` event into a [`LogEvent`].
pub fn record_from_event(event: &Event<'_>) -> LogEvent {
    let mut visitor = MessageVisitor::default();
    event.record(&mut visitor);
    let meta = event.metadata();
    let message = match (visitor.message.is_empty(), visitor.fields.is_empty()) {
        (false, true) => visitor.message,
        (true, false) => visitor.fields,
        (true, true) => String::new(),
        (false, false) => format!("{} {}", visitor.message, visitor.fields),
    };
    LogEvent {
        level: *meta.level(),
        target: meta.target().to_string(),
        message,
    }
}

/// Build the level filter. Precedence: `MQ_BRIDGE_LOG` env var, then `RUST_LOG`,
/// then the caller-supplied level, then `warn` (libraries stay quiet by default).
pub fn env_filter(default_level: Option<&str>) -> EnvFilter {
    for var in ["MQ_BRIDGE_LOG", "RUST_LOG"] {
        match std::env::var(var) {
            // An empty value (`MQ_BRIDGE_LOG=`) means "unset", not "silence all".
            Ok(directives) if !directives.trim().is_empty() => {
                if let Ok(filter) = EnvFilter::try_new(&directives) {
                    return filter;
                }
            }
            _ => {}
        }
    }
    let level = default_level.unwrap_or("warn");
    EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("warn"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;

    /// A layer that records every flattened event, so tests can assert on what
    /// `record_from_event` produces through the real `tracing` macro path.
    #[derive(Clone, Default)]
    struct CaptureLayer {
        events: Arc<Mutex<Vec<LogEvent>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.events.lock().unwrap().push(record_from_event(event));
        }
    }

    /// Run `f` with a scoped (thread-local) subscriber and return what it caught.
    /// `with_default` avoids the global-once install, so tests stay independent.
    fn capture(filter: &str, f: impl FnOnce()) -> Vec<LogEvent> {
        let layer = CaptureLayer::default();
        let events = layer.events.clone();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(filter))
            .with(layer);
        tracing::subscriber::with_default(subscriber, f);
        Arc::try_unwrap(events).unwrap().into_inner().unwrap()
    }

    #[test]
    fn flattens_a_plain_message() {
        let events = capture("trace", || tracing::info!("hello world"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::INFO);
        assert_eq!(events[0].message, "hello world");
        assert_eq!(events[0].target, module_path!());
    }

    #[test]
    fn appends_structured_fields_after_the_message() {
        let events = capture("trace", || {
            tracing::warn!(topic = "out", n = 5, "creating channel")
        });
        assert_eq!(events[0].level, Level::WARN);
        assert_eq!(events[0].message, "creating channel topic=\"out\" n=5");
    }

    #[test]
    fn renders_fields_only_events() {
        let events = capture("trace", || tracing::error!(code = 42));
        assert_eq!(events[0].message, "code=42");
    }

    #[test]
    fn filter_suppresses_events_below_the_level() {
        let events = capture("warn", || {
            tracing::info!("suppressed");
            tracing::warn!("kept");
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "kept");
    }

    #[test]
    fn level_mapping_matches_python_and_string_forms() {
        let cases = [
            (Level::ERROR, 40, "error"),
            (Level::WARN, 30, "warn"),
            (Level::INFO, 20, "info"),
            (Level::DEBUG, 10, "debug"),
            (Level::TRACE, 5, "trace"),
        ];
        for (level, levelno, name) in cases {
            let event = LogEvent {
                level,
                target: String::new(),
                message: String::new(),
            };
            assert_eq!(event.python_levelno(), levelno);
            assert_eq!(event.level_str(), name);
        }
    }

    #[test]
    fn target_becomes_a_dotted_python_logger_name() {
        let event = LogEvent {
            level: Level::INFO,
            target: "mq_bridge::endpoints::memory".to_string(),
            message: String::new(),
        };
        assert_eq!(event.python_logger_name(), "mq_bridge.endpoints.memory");
    }

    #[test]
    fn env_filter_falls_back_to_a_valid_filter() {
        // Explicit level parses; a bogus level degrades to the default rather
        // than panicking. (Env-var precedence is covered by the binding tests,
        // which control the process environment.)
        assert!(std::panic::catch_unwind(|| {
            let _ = env_filter(Some("info"));
            let _ = env_filter(Some("not a level"));
            let _ = env_filter(None);
        })
        .is_ok());
    }
}
