//! Log file setup and the redaction safeguard.
//!
//! Logs go to a daily-rolling file `gmail-ro.<date>.log` in `<app_local_data_dir>\logs` (at most [`MAX_FILES`]
//! kept; the appender deletes older ones when it starts and at each rollover). Level is INFO unless the
//! `GMAIL_RO_LOG` environment variable holds a filter (for example `debug` or `mail_service=debug`). No ANSI.
//!
//! The code base already logs only ids, counts and timings (CLAUDE.md). [`redact`] is a second line of defence:
//! every event is passed through it before it reaches the file, and masks email addresses, OAuth tokens
//! (`ya29.`, `1//`, long base64url runs) and `Authorization` header values.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Rolling files kept.
pub const MAX_FILES: usize = 7;
/// Environment variable overriding the level / filter.
pub const ENV_VAR: &str = "GMAIL_RO_LOG";

const MASK: &str = "[redacted]";
/// A base64url run at least this long is treated as a token.
const LONG_RUN: usize = 40;

/// Install the global subscriber writing to `<dir>` (created if missing). Returns an error string if the log file
/// cannot be opened; the app then runs without a log file rather than failing.
pub fn init(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create log dir: {e}"))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("gmail-ro")
        .filename_suffix("log")
        .max_log_files(MAX_FILES)
        .build(dir)
        .map_err(|e| format!("open log file: {e}"))?;
    let filter = EnvFilter::try_from_env(ENV_VAR).unwrap_or_else(|_| EnvFilter::new("info"));
    let writer = RedactingMakeWriter(Arc::new(Mutex::new(appender)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .try_init()
        .map_err(|e| format!("install subscriber: {e}"))
}

/// Hands out a [`RedactingWriter`] per event, all sharing one file appender.
#[derive(Clone)]
struct RedactingMakeWriter<W: Write + Send + 'static>(Arc<Mutex<W>>);

impl<'a, W: Write + Send + 'static> MakeWriter<'a> for RedactingMakeWriter<W> {
    type Writer = RedactingWriter<W>;
    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter { inner: self.0.clone(), buf: Vec::new() }
    }
}

/// Collects one event, redacts it, and writes it to the file when dropped (redaction needs the whole line, and
/// the formatter may write it in several pieces).
struct RedactingWriter<W: Write> {
    inner: Arc<Mutex<W>>,
    buf: Vec<u8>,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf);
        let clean = redact(&text);
        let mut f = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = f.write_all(clean.as_bytes());
        let _ = f.flush();
    }
}

fn is_b64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Mask emails, tokens and Authorization values in `s`.
pub fn redact(s: &str) -> String {
    let s = mask_authorization(s);
    let s = mask_prefixed_tokens(&s);
    let s = mask_emails(&s);
    mask_long_runs(&s)
}

/// `Authorization: Bearer xyz`, `authorization="Basic ..."`, `"authorization": "..."`: mask the value to the end
/// of the line.
fn mask_authorization(s: &str) -> String {
    const KEY: &str = "authorization";
    let lower = s.to_ascii_lowercase(); // ASCII lowercasing keeps byte offsets identical
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(i) = lower[pos..].find(KEY) {
        let key_end = pos + i + KEY.len();
        out.push_str(&s[pos..key_end]);
        // Skip the separator: quotes, colon, equals, spaces.
        let rest = &s[key_end..];
        let sep = rest.len() - rest.trim_start_matches(['"', '\'', ':', '=', ' ', '\t']).len();
        // Only a key/value separator counts (`:` or `=`), not the word "authorization" in prose.
        if !rest[..sep].contains([':', '=']) || rest[sep..].starts_with(['\r', '\n']) || rest[sep..].is_empty() {
            pos = key_end;
            continue;
        }
        out.push_str(&rest[..sep]);
        let value_end = rest[sep..].find(['\r', '\n']).map_or(rest.len(), |n| sep + n);
        out.push_str(MASK);
        pos = key_end + value_end;
    }
    out.push_str(&s[pos..]);
    out
}

/// `ya29.<...>` (access tokens) and `1//<...>` (refresh tokens).
fn mask_prefixed_tokens(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let starts = |p: &str| {
            let n = p.chars().count();
            chars.len() - i >= n && chars[i..i + n].iter().copied().eq(p.chars())
        };
        let boundary = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        let prefix_len = if starts("ya29.") {
            Some(5)
        } else if boundary && starts("1//") {
            Some(3)
        } else {
            None
        };
        if let Some(p) = prefix_len {
            let mut j = i + p;
            while j < chars.len() && (is_b64url(chars[j]) || chars[j] == '.') {
                j += 1;
            }
            // `1//` alone (a URL fragment, a date) is not a token: require some body.
            if j - (i + p) >= if p == 5 { 1 } else { 8 } {
                out.push_str(MASK);
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `local@domain.tld`.
fn mask_emails(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let mut a = out.chars().count();
            let out_chars: Vec<char> = out.chars().collect();
            while a > 0 && local(out_chars[a - 1]) {
                a -= 1;
            }
            let mut b = i + 1;
            while b < chars.len() && domain(chars[b]) {
                b += 1;
            }
            while b > i + 1 && matches!(chars[b - 1], '.' | '-') {
                b -= 1;
            }
            let has_dot = chars[i + 1..b].contains(&'.');
            let local_len = out_chars.len() - a;
            if local_len > 0 && has_dot {
                out = out_chars[..a].iter().collect();
                out.push_str(MASK);
                i = b;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Runs of [`LONG_RUN`]+ base64url characters (JWTs, opaque tokens, auth codes).
fn mask_long_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush =|run: &mut String, out: &mut String| {
        if run.chars().count() >= LONG_RUN {
            out.push_str(MASK);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if is_b64url(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_masked() {
        assert_eq!(redact("signed in as alice.b+tag@example.co.uk ok"), "signed in as [redacted] ok");
        assert_eq!(redact("to=<bob@mail.example.com>, x"), "to=<[redacted]>, x");
        assert_eq!(redact("a@b.c and d@e.fg."), "[redacted] and [redacted].");
        assert_eq!(redact("name@host"), "name@host", "no dot in the domain: not an address");
        assert_eq!(redact("@ alone, user@ nothing"), "@ alone, user@ nothing");
    }

    #[test]
    fn google_tokens_are_masked() {
        assert_eq!(redact("token ya29.a0AfH6SMBx-Yz_123 end"), "token [redacted] end");
        assert_eq!(redact("refresh=1//0gAbCdEfGhIjKlMn-op_q done"), "refresh=[redacted] done");
        assert_eq!(redact("path v1//x and 1//ab"), "path v1//x and 1//ab", "not tokens");
    }

    #[test]
    fn long_base64url_runs_are_masked_short_ones_are_not() {
        let tok = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0-_xyz";
        assert!(tok.len() >= 40);
        assert_eq!(redact(&format!("code={tok}&state=ok")), "code=[redacted]&state=ok");
        let short = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t";
        assert_eq!(short.len(), 39);
        assert_eq!(redact(short), short);
        assert_eq!(redact("message 18c2f4a9b3d5e7f1 labels 3"), "message 18c2f4a9b3d5e7f1 labels 3");
    }

    #[test]
    fn authorization_values_are_masked() {
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: [redacted]");
        assert_eq!(redact("headers {authorization=\"Basic Zm9v\"}\nnext line"), "headers {authorization=\"[redacted]\nnext line");
        assert_eq!(redact("AUTHORIZATION: Bearer x\r\nHost: y"), "AUTHORIZATION: [redacted]\r\nHost: y");
        assert_eq!(redact("authorization failed"), "authorization failed", "prose is left alone");
        assert_eq!(redact("no header here"), "no header here");
    }

    #[test]
    fn ordinary_lines_pass_through_and_unicode_is_safe() {
        let line = "2026-10-25T10:00:00Z  INFO gmail_ro_lib: accounts loaded accounts=2 skipped=0 \u{2713} \u{00e9}";
        assert_eq!(redact(line), line);
        assert_eq!(redact("\u{00e9}x@a.com"), "\u{00e9}[redacted]");
    }

    #[test]
    fn writer_redacts_whole_events_before_they_reach_the_file() {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, d: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(d);
                Ok(d.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let mw = RedactingMakeWriter(Arc::new(Mutex::new(sink.clone())));
        {
            let mut w = mw.make_writer();
            // Split mid-address and mid-token: still caught, because the event is redacted as a whole.
            w.write_all(b"user ali").unwrap();
            w.write_all(b"ce@exam").unwrap();
            w.write_all(b"ple.com ya29.abc\n").unwrap();
        }
        let got = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(got, "user [redacted] [redacted]\n");
    }
}
