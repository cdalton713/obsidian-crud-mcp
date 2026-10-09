//! Logging to stderr, with credentials redacted from every line.
//!
//! A URL in an error message may embed credentials as `user:pass@host`.
//! Everything written to the log passes through [`redact_credentials`] so a
//! password never lands in container logs.

use std::error::Error;
use std::io::{self, Write};
use std::sync::LazyLock;

use regex::Regex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

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

/// A writer that redacts each formatted log line before it reaches stderr.
#[derive(Clone, Copy, Default)]
pub struct RedactingStderr;

impl Write for RedactingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = redact_credentials(&String::from_utf8_lossy(buf));
        io::stderr().lock().write_all(line.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

impl<'a> MakeWriter<'a> for RedactingStderr {
    type Writer = RedactingStderr;

    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

/// Install the global subscriber. Logs go to stderr so stdout stays clean.
pub fn init(level: Option<&str>) {
    let directive = level_directive(level);
    let filter = EnvFilter::new(format!("{directive},aws_config=warn,aws_smithy_runtime=warn,hyper=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(RedactingStderr)
        .with_target(false)
        .with_ansi(false)
        .try_init();
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
}
