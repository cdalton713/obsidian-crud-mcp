//! Logging to stderr, with credentials redacted from every line.
//!
//! A URL in an error message may embed credentials as `user:pass@host`.
//! Everything written to the log passes through [`redact_credentials`] so a
//! password never lands in container logs.

use std::error::Error;
use std::io::{self, Write};
use std::sync::LazyLock;

use regex::Regex;
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

static URL_USERINFO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/?#]+@").unwrap());

/// Replace the userinfo part of every URL in `text` (`http://u:p@h` becomes `http://***@h`).
///
/// Greedy up to the last `@` before the path, as URL parsers split userinfo,
/// so an unencoded `@` in the password is still fully covered.
pub fn redact_credentials(text: &str) -> String {
    URL_USERINFO.replace_all(text, "${1}***@").into_owned()
}

/// One-line description of an error for logs: the message and up to three
/// causes (`message (cause: a <- b)`), redacted.
pub fn describe_error(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut causes: Vec<String> = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source.filter(|_| causes.len() < 3) {
        let message = cause.to_string();
        if !message.is_empty() && !text.contains(&message) {
            causes.push(message);
        }
        source = cause.source();
    }
    if !causes.is_empty() {
        text.push_str(&format!(" (cause: {})", causes.join(" <- ")));
    }
    redact_credentials(&text)
}

/// Map `LOG_LEVEL` (pino-style names accepted) to a tracing filter directive.
pub fn level_directive(level: Option<&str>) -> &'static str {
    match level.map(str::to_ascii_lowercase).as_deref() {
        Some("silent" | "off") => "off",
        Some("fatal" | "error") => "error",
        Some("warn" | "warning") => "warn",
        Some("debug") => "debug",
        Some("trace") => "trace",
        _ => "info",
    }
}

/// Wraps a [`MakeWriter`] so each formatted log line is redacted before it is written.
#[derive(Clone, Copy, Default)]
pub struct Redacting<M>(pub M);

/// The writer behind [`Redacting`]. The formatter hands over one whole event per
/// write, so a URL is never split between two calls.
pub struct RedactingWriter<W>(W);

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = redact_credentials(&String::from_utf8_lossy(buf));
        self.0.write_all(line.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for Redacting<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self.0.make_writer())
    }
}

/// The log subscriber for `LOG_LEVEL`, writing redacted lines to `writer`.
fn subscriber<W>(level: Option<&str>, writer: W) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let directive = level_directive(level);
    let filter = EnvFilter::new(format!("{directive},aws_config=warn,aws_smithy_runtime=warn,hyper=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Redacting(writer))
        .with_target(false)
        .with_ansi(false)
        .finish()
}

/// Install the global subscriber. Logs go to stderr so stdout stays clean.
pub fn init(level: Option<&str>) {
    let _ = subscriber(level, io::stderr).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo() {
        assert_eq!(redact_credentials("GET http://user:pass@host/x"), "GET http://***@host/x");
        assert_eq!(redact_credentials("https://a:b@c@host.io?q"), "https://***@host.io?q");
        assert_eq!(redact_credentials("S3://KEY:SECRET@bucket"), "S3://***@bucket");
        assert_eq!(redact_credentials("no url here, just a@b"), "no url here, just a@b");
        assert_eq!(redact_credentials("http://host/path@x"), "http://host/path@x");
        assert_eq!(redact_credentials("two: http://a:b@x and ftp://c:d@y"), "two: http://***@x and ftp://***@y");
    }

    #[derive(Debug, thiserror::Error)]
    #[error("outer failed")]
    struct Outer(#[source] io::Error);

    #[test]
    fn describes_error_chains() {
        let error = Outer(io::Error::other("connect to http://u:p@db failed"));
        assert_eq!(describe_error(&error), "outer failed (cause: connect to http://***@db failed)");
        let plain = io::Error::other("plain");
        assert_eq!(describe_error(&plain), "plain");
    }

    #[test]
    fn maps_levels() {
        assert_eq!(level_directive(Some("silent")), "off");
        assert_eq!(level_directive(Some("DEBUG")), "debug");
        assert_eq!(level_directive(None), "info");
        assert_eq!(level_directive(Some("fatal")), "error");
    }

    #[test]
    fn redacts_username_only_userinfo() {
        assert_eq!(redact_credentials("https://admin@db.example.com/obsidian"), "https://***@db.example.com/obsidian");
    }

    #[test]
    fn redacts_passwords_with_reserved_or_raw_at_characters() {
        assert_eq!(redact_credentials("http://admin:p%40ss:w0rd@host:5984/"), "http://***@host:5984/");
        assert_eq!(redact_credentials("http://admin:p@ss@host:5984"), "http://***@host:5984");
    }

    #[test]
    fn leaves_urls_without_credentials_untouched() {
        for text in ["http://db.internal:5984/obsidian", "http://host/notes/a@b.md", "http://host/?q=a@b"] {
            assert_eq!(redact_credentials(text), text);
        }
    }

    /// An error with an arbitrary chain of sources.
    #[derive(Debug)]
    struct Chain(&'static str, Option<Box<Chain>>);

    impl std::fmt::Display for Chain {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Chain {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1.as_deref().map(|e| e as &(dyn Error + 'static))
        }
    }

    fn chain(messages: &[&'static str]) -> Chain {
        let (last, rest) = messages.split_last().unwrap();
        rest.iter().rev().fold(Chain(last, None), |source, message| Chain(message, Some(Box::new(source))))
    }

    #[test]
    fn describes_nested_causes_and_redacts_them() {
        let error = chain(&["outer", "middle", "getaddrinfo ENOTFOUND http://u:p@db"]);
        assert_eq!(describe_error(&error), "outer (cause: middle <- getaddrinfo ENOTFOUND http://***@db)");
        assert_eq!(describe_error(&chain(&["request to http://u:p@h/db failed"])), "request to http://***@h/db failed");
    }

    #[test]
    fn describes_at_most_three_causes() {
        let error = chain(&["top", "a", "b", "c", "d"]);
        assert_eq!(describe_error(&error), "top (cause: a <- b <- c)");
    }

    #[test]
    fn skips_causes_that_add_nothing() {
        // Errors that already print their source, and empty messages, would only repeat or add noise.
        let error = chain(&["read failed: disk full", "disk full", "", "EIO"]);
        assert_eq!(describe_error(&error), "read failed: disk full (cause: EIO)");
    }

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Log a fixed set of events at `level` and return the output lines.
    fn log_lines(level: &str) -> Vec<String> {
        let capture = Capture::default();
        let writer = {
            let capture = capture.clone();
            move || capture.clone()
        };
        tracing::subscriber::with_default(subscriber(Some(level), writer), || {
            let error = chain(&["save failed", "http://user:secret@host/db"]);
            tracing::debug!("debug message");
            tracing::info!("ready");
            tracing::warn!(url = "http://user:secret@host/db", "request failed");
            tracing::error!("save failed: {}", describe_error(&error));
            tracing::error!(error = %error, "raw error");
        });
        let output = String::from_utf8(capture.0.lock().clone()).unwrap();
        output.lines().map(str::to_owned).collect()
    }

    #[test]
    fn debug_messages_appear_only_at_debug_level() {
        let debug = log_lines("debug");
        assert_eq!(debug.len(), 5, "{debug:#?}");
        assert!(debug[0].contains("DEBUG") && debug[0].ends_with("debug message"), "{}", debug[0]);

        let info = log_lines("info");
        assert_eq!(info.len(), 4, "{info:#?}");
        assert!(info[0].contains(" INFO ") && info[0].ends_with("ready"), "{}", info[0]);
        assert!(log_lines("silent").is_empty());
    }

    #[test]
    fn every_logged_line_is_redacted() {
        let lines = log_lines("info");
        let output = lines.join("\n");
        assert!(!output.contains("secret"), "{output}");
        assert!(lines[1].contains("WARN") && lines[1].contains("http://***@host/db"), "{}", lines[1]);
        assert!(lines[2].ends_with("save failed: save failed (cause: http://***@host/db)"), "{}", lines[2]);
    }
}
