//! `Authentication-Results` (RFC 8601) -> a short "sender authenticity" summary.
//!
//! Only the header Gmail added is trusted: the first one whose authserv-id belongs to google.com. Any
//! other `Authentication-Results` header may have been written by the sender.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    Fail,
    SoftFail,
    Neutral,
    None,
    TempError,
    PermError,
    Policy,
    Other(String),
}

impl Verdict {
    fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            "softfail" => Self::SoftFail,
            "neutral" => Self::Neutral,
            "none" => Self::None,
            "temperror" => Self::TempError,
            "permerror" => Self::PermError,
            "policy" => Self::Policy,
            o => Self::Other(o.chars().take(16).collect()),
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::SoftFail => "softfail",
            Self::Neutral => "neutral",
            Self::None => "none",
            Self::TempError => "temperror",
            Self::PermError => "permerror",
            Self::Policy => "policy",
            Self::Other(o) => o,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AuthSummary {
    pub authserv_id: String,
    pub spf: Option<Verdict>,
    /// All DKIM results (a message can carry several signatures): verdict and signing domain.
    pub dkim: Vec<(Verdict, Option<String>)>,
    pub dmarc: Option<Verdict>,
    /// Domain of `smtp.mailfrom` ("mailed-by").
    pub mailed_by: Option<String>,
    /// Domain of the first passing DKIM signature ("signed-by").
    pub signed_by: Option<String>,
}

impl AuthSummary {
    pub fn dkim_overall(&self) -> Option<Verdict> {
        if self.dkim.is_empty() {
            None
        } else if self.dkim.iter().any(|(v, _)| *v == Verdict::Pass) {
            Some(Verdict::Pass)
        } else {
            Some(self.dkim[0].0.clone())
        }
    }

    /// `good`, `bad` (any fail/softfail), `unknown` (nothing useful), or `mixed`.
    pub fn level(&self) -> &'static str {
        let all: Vec<Option<Verdict>> = vec![self.spf.clone(), self.dkim_overall(), self.dmarc.clone()];
        if all.iter().flatten().any(|v| matches!(v, Verdict::Fail | Verdict::SoftFail | Verdict::PermError)) {
            "bad"
        } else if self.dmarc == Some(Verdict::Pass) || (self.spf == Some(Verdict::Pass) && self.dkim_overall() == Some(Verdict::Pass)) {
            "good"
        } else if all.iter().all(Option::is_none) {
            "unknown"
        } else {
            "mixed"
        }
    }

    /// One line, e.g. `SPF pass · DKIM pass (example.com) · DMARC pass`.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.spf {
            parts.push(format!("SPF {}", v.label()));
        }
        if let Some(v) = self.dkim_overall() {
            match &self.signed_by {
                Some(d) if v == Verdict::Pass => parts.push(format!("DKIM pass ({d})")),
                _ => parts.push(format!("DKIM {}", v.label())),
            }
        }
        if let Some(v) = &self.dmarc {
            parts.push(format!("DMARC {}", v.label()));
        }
        if parts.is_empty() { "no authentication results".to_string() } else { parts.join(" \u{b7} ") }
    }
}

/// Remove RFC 5322 comments `( ... )` (nested, with quoted-pair escapes), outside quoted strings.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut depth, mut in_quote, mut esc) = (0usize, false, false);
    for c in s.chars() {
        if esc {
            esc = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                esc = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                in_quote = !in_quote;
                out.push(c);
            }
            '(' if !in_quote => depth += 1,
            ')' if !in_quote && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn domain_of(v: &str) -> Option<String> {
    let v = v.trim().trim_matches('"').trim_matches(|c| c == '<' || c == '>');
    let d = v.rsplit('@').next()?.trim();
    (!d.is_empty() && d.len() < 254 && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .then(|| d.to_ascii_lowercase())
}

/// Parse one header value. Returns None when it does not look like `authserv-id; method=result ...`.
pub(crate) fn parse_one(value: &str) -> Option<AuthSummary> {
    let unfolded = value.replace(['\r', '\n'], " ");
    let clean = strip_comments(&unfolded);
    let mut segs = clean.split(';');
    let id = segs.next()?.split_whitespace().next()?.to_ascii_lowercase();
    if id.is_empty() || id.contains('=') {
        return None;
    }
    let mut s = AuthSummary { authserv_id: id, ..Default::default() };
    for seg in segs {
        let mut toks = seg.split_whitespace();
        let Some(head) = toks.next() else { continue };
        let Some((method, result)) = head.split_once('=') else { continue };
        let verdict = Verdict::parse(result);
        let props: Vec<(String, String)> = toks
            .filter_map(|t| t.split_once('=').map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())))
            .collect();
        let prop = |k: &str| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str());
        match method.to_ascii_lowercase().as_str() {
            "spf" if s.spf.is_none() => {
                s.spf = Some(verdict);
                s.mailed_by = prop("smtp.mailfrom").or_else(|| prop("smtp.helo")).and_then(domain_of);
            }
            "dkim" => {
                let dom = prop("header.d").or_else(|| prop("header.i")).and_then(domain_of);
                if verdict == Verdict::Pass && s.signed_by.is_none() {
                    s.signed_by = dom.clone();
                }
                s.dkim.push((verdict, dom));
            }
            "dmarc" if s.dmarc.is_none() => s.dmarc = Some(verdict),
            _ => {}
        }
    }
    Some(s)
}

fn is_google(id: &str) -> bool {
    id == "google.com" || id.ends_with(".google.com")
}

/// The trusted summary: first header whose authserv-id is Google's.
pub(crate) fn summarize(values: &[String]) -> Option<AuthSummary> {
    values.iter().filter_map(|v| parse_one(v)).find(|s| is_google(&s.authserv_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "mx.google.com;\r\n       dkim=pass header.i=@example.com header.s=sel header.b=AbCd;\r\n       spf=pass (google.com: domain of bounce@mail.example.com designates 1.2.3.4 as permitted sender) smtp.mailfrom=bounce@mail.example.com;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com";

    #[test]
    fn parses_gmail_header() {
        let s = summarize(&[GOOD.to_string()]).unwrap();
        assert_eq!(s.spf, Some(Verdict::Pass));
        assert_eq!(s.dmarc, Some(Verdict::Pass));
        assert_eq!(s.dkim_overall(), Some(Verdict::Pass));
        assert_eq!(s.mailed_by.as_deref(), Some("mail.example.com"));
        assert_eq!(s.signed_by.as_deref(), Some("example.com"));
        assert_eq!(s.level(), "good");
        assert_eq!(s.line(), "SPF pass \u{b7} DKIM pass (example.com) \u{b7} DMARC pass");
    }

    #[test]
    fn ignores_non_google_servers_and_spoofed_headers() {
        let spoof = "evil.example.net; dkim=pass header.d=bank.com; spf=pass smtp.mailfrom=bank.com; dmarc=pass".to_string();
        assert!(summarize(std::slice::from_ref(&spoof)).is_none());
        // Gmail's own header wins even when listed after a spoofed one.
        let s = summarize(&[spoof, GOOD.to_string()]).unwrap();
        assert_eq!(s.signed_by.as_deref(), Some("example.com"));
    }

    #[test]
    fn failures_and_multiple_signatures() {
        let v = "mx.google.com; dkim=fail header.i=@a.com; dkim=pass header.d=b.org; spf=softfail smtp.mailfrom=x@c.net; dmarc=fail header.from=a.com";
        let s = summarize(&[v.to_string()]).unwrap();
        assert_eq!(s.dkim.len(), 2);
        assert_eq!(s.signed_by.as_deref(), Some("b.org"));
        assert_eq!(s.level(), "bad");
    }

    #[test]
    fn comments_with_semicolons_do_not_split() {
        let v = "mx.google.com; spf=pass (google.com: domain of a@b.c designates 1.1.1.1; nested (x) ok) smtp.mailfrom=a@b.c; dkim=none";
        let s = summarize(&[v.to_string()]).unwrap();
        assert_eq!(s.spf, Some(Verdict::Pass));
        assert_eq!(s.mailed_by.as_deref(), Some("b.c"));
        assert_eq!(s.dkim_overall(), Some(Verdict::None));
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_one("").is_none());
        assert!(parse_one("dkim=pass").is_none());
        assert!(summarize(&[]).is_none());
    }
}
