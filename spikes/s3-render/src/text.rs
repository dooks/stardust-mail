//! text/plain -> HTML: escape everything, linkify http(s) and `www.` URLs, mark `>` quote blocks.
//! The result is meant to sit inside a `white-space: pre-wrap` container.

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + s.len() / 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            // Control characters other than tab/newline are noise in a text view.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => o.push(c),
        }
    }
    o
}

const MAX_URL: usize = 2000;

/// Length (in bytes) of the URL that starts at `s[0..]`, after trimming trailing punctuation.
fn url_len(s: &str) -> usize {
    let end = s.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\u{a0}')).unwrap_or(s.len()).min(MAX_URL);
    let mut end = s.floor_char_boundary(end);
    loop {
        let url = &s[..end];
        let Some(last) = url.chars().next_back() else { return 0 };
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
            ')' => url.matches(')').count() > url.matches('(').count(),
            ']' => url.matches(']').count() > url.matches('[').count(),
            '}' => url.matches('}').count() > url.matches('{').count(),
            _ => false,
        };
        if !trim {
            return end;
        }
        end -= last.len_utf8();
    }
}

fn is_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|c| !(c.is_alphanumeric() || c == '/' || c == '.' || c == '@' || c == '-' || c == '_'))
}

/// Escape one line and wrap URLs in anchors.
fn linkify_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 32);
    let mut i = 0;
    let lower = line.to_ascii_lowercase();
    while i < line.len() {
        let rest = &lower[i..];
        let prev = line[..i].chars().next_back();
        let scheme = if rest.starts_with("https://") || rest.starts_with("http://") {
            Some("")
        } else if rest.starts_with("www.") && is_boundary(prev) {
            Some("https://")
        } else {
            None
        };
        if let Some(prefix) = scheme {
            let n = url_len(&line[i..]);
            // "http://" alone, or "www." followed by nothing useful, is not a link.
            let min = if prefix.is_empty() { rest.find("//").map_or(0, |p| p + 3) } else { 5 };
            if n >= min {
                let text = &line[i..i + n];
                out.push_str("<a href=\"");
                out.push_str(&escape(&format!("{prefix}{text}")));
                out.push_str("\">");
                out.push_str(&escape(text));
                out.push_str("</a>");
                i += n;
                continue;
            }
        }
        let ch = line[i..].chars().next().expect("char");
        out.push_str(&escape(ch.encode_utf8(&mut [0; 4])));
        i += ch.len_utf8();
    }
    out
}

/// Plain text body -> HTML fragment (a `div.plain`).
pub fn text_to_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::from("<div class=\"plain\">");
    let mut in_quote = false;
    let mut first_in_block = true;
    for line in text.trim_end_matches('\n').split('\n') {
        let quoted = line.starts_with('>');
        if quoted != in_quote {
            if in_quote {
                out.push_str("</div>");
            }
            if quoted {
                out.push_str("<div class=\"q\">");
            }
            in_quote = quoted;
            first_in_block = true;
        }
        if !first_in_block {
            out.push('\n');
        }
        first_in_block = false;
        out.push_str(&linkify_line(line));
    }
    if in_quote {
        out.push_str("</div>");
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        let h = text_to_html("<script>alert(1)</script> & \"x\"");
        assert!(!h.contains("<script"));
        assert!(h.contains("&lt;script&gt;") && h.contains("&amp;") && h.contains("&quot;x&quot;"));
    }

    #[test]
    fn links_are_trimmed_and_escaped() {
        let h = text_to_html("see https://example.com/a?b=1&c=2). and www.example.org, ok");
        assert!(h.contains("<a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a>)."));
        assert!(h.contains("<a href=\"https://www.example.org\">www.example.org</a>,"));
    }

    #[test]
    fn parens_inside_url_are_kept() {
        let h = text_to_html("https://en.example.org/wiki/Foo_(bar)");
        assert!(h.contains(">https://en.example.org/wiki/Foo_(bar)</a>"));
    }

    #[test]
    fn non_http_schemes_are_not_linked() {
        let h = text_to_html("javascript:alert(1) file:///c:/x data:text/html,x ftp://x.org");
        assert!(!h.contains("<a "));
    }

    #[test]
    fn quotes_group_and_crlf_normalised() {
        let h = text_to_html("hi\r\n> a\r\n> b\r\nbye");
        assert_eq!(h, "<div class=\"plain\">hi<div class=\"q\">&gt; a\n&gt; b</div>bye</div>");
    }

    #[test]
    fn embedded_scheme_without_boundary() {
        // "xwww.foo" is not a link; "http://" with nothing after it is not a link.
        let h = text_to_html("xwww.foo http:// end");
        assert!(!h.contains("<a "));
    }
}
