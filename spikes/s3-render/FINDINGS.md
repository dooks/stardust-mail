# S3: message view and rendering

> **Status: final (headless).** The visual comparison against Gmail was optional and not done, because the user cut user steps to the essentials. The sample is 90 real messages from dylan and ziggy (44 and 46), from 9 queries.

Crate: `spikes\s3-render\`.
- Library `s3render` and bin `s3`, with commands fetch, stats, render, render-all, time and verify-att.
- Modules: `parse`, `auth`, `text`, `sanitize`, `render`, `fetch`, `compare` and `stats`.
- `harness\` is a Tauri 2.12 test app (`com.ziggy.starmail-spikes-s3`).
- Fixtures in `fixtures\` are all synthetic, including `hostile.html`.
- 25 unit and 8 integration tests pass, and clippy is clean.
- `out\` (git-ignored) holds the real sample.
- Endpoints used: `messages.list`, and `messages.get` (raw and full) and `messages.attachments.get`, all GET.

## TL;DR
- **Pipeline:** `format=raw` → `mail-parser` 1.0 → choose the body → `ammonia` + attribute filter + CSS scrubber → a page with a CSP → `<iframe sandbox srcdoc>`. `cid:` images are served by the `gmail-asset` protocol.
- **Parsing with raw + mail-parser:**
  - 0 parse failures and 0 charset problems.
  - All 81 HTML and 71 text bodies match Gmail's `format=full`.
- **Don't use `format=full` as the main path:**
  - It re-encodes bodies to UTF-8 but keeps the original charset labels. Decoding by the label gave garbled text on 8 bodies.
  - Its attachment bytes are never inline, so every `cid:` image needs `attachments.get`.
- **Raw never omits attachment data.** 12 of 12 `attachments.get` downloads were byte-identical to the parts in raw.
- **Hostile email in WebView2:**
  - In the production setup: **0 requests leaked and no script ran.** Links can be intercepted.
  - Without sandbox and CSP (the control): 20 requests leaked and scripts ran.
- **Remote images are blocked by default.** 79% of HTML mail has them, and 37 messages have tracking pixels. "Show images" re-renders that one message with a relaxed CSP.
- **Email HTML always renders on white.** Our own plain-text view follows the theme.
- **Sender authenticity comes from Google's `Authentication-Results`**, present on 82 of 82 inbound messages and parsed 100%. Only Google's header is trusted: 33 of 90 messages carry extra, forgeable ones.

## Q1. Parsing
**Raw size (decoded):**
| p50 | p90 | p99 | max | total |
|---|---|---|---|---|
| 37 KB | 177 KB | 9.8 MB | 27.2 MB | 74 MB |

- 7 of 90 messages are 5 MB or more.
- Raw size equals `sizeEstimate` on 90 of 90.
- The same 90 messages as `full` JSON total 4.5 MB, because full leaves attachments out.

**Latency per `messages.get`:**
- Small messages: raw 162–387 ms, full 89–353 ms.
- A 13.4 MB message: raw 1.8 s, full 0.24 s.
- A 27 MB message: raw 5.8 s, full 0.13 s.

**MIME shapes:**
- 41 × `alternative(text, html)`
- 13 × html only
- 7 × text only
- 8 × `mixed` with PDFs
- 6 × `related` with images

Parts per message: p50 3, max 10.

**Charsets:** utf-8 128, us-ascii 16, iso-8859-1 8, windows-1252 1.
- 14 messages had RFC 2047 encoded words, and 0 were left undecoded.
- 8 messages mention `format=flowed`. We don't unflow them; `pre-wrap` is fine.

**raw vs full:** bodies are equal on 81/81 HTML and 71/71 text when full is read as UTF-8. Subject, From address and From name match on all. Attachment lists match on 89/90.

**Defects found and fixed:**
1. A sender labelled windows-1252 bytes as UTF-8, giving a C1 control character or U+FFFD. The fix re-decodes the part bytes with a per-byte windows-1252 fallback and maps C1 controls (`parse::part_text`, `fix_c1`).
2. `mail-parser`'s `attachments()` omits an inline image that sits between text parts. The fix keeps `text_items` (`Text | Image`) and renders them in reading order.

**Recommendation:**
- Fetch **raw** for everything: one blob and one code path, and the offline cache holds the images and attachments.
- Above about **8–10 MB** `sizeEstimate`, use a hybrid: `format=full` for the body, and `attachments.get` on demand.

## Q2. Body choice
| Case | Count |
|---|---|
| HTML + text | 62 |
| HTML only | 19 |
| Text only | 9 |
| No body | 0 |

Rule: genuine `text/html` if there is any, otherwise text.

Text → HTML (`text.rs`):
- Escape the text and drop control characters.
- Linkify only `http(s)://` and `www.` (never `javascript:`, `file:` or `data:`).
- Group `>` quote lines into `.q` blocks.
- Use `.plain { white-space: pre-wrap; overflow-wrap: anywhere }`.

## Q3. Sanitizing (ammonia 4.2.4, `Builder::empty()`)
- **Tags allowed:** text and layout tags (a, b, i, u, s, p, div, span, br, hr, h1–h6, lists, blockquote, pre, code, font, center, small, big, sub, sup, the semantic sectioning tags, details and summary, time, wbr). Also tables (table, caption, colgroup, col, thead, tbody, tfoot, tr, td, th), img, and **style**.
- **Removed with their contents:** script, noscript, title, head, template, iframe, frame(set), object, embed, applet, svg, math, video, audio, canvas, textarea, select, option, datalist, map, xmp, noembed and noframes.
- **Generic attributes:** class, dir, lang, title, style, align, valign, bgcolor, width, height, role, id and `aria-*`.
- **Per-tag attributes:** link `href`/`name`, image attributes (src, alt, border, hspace, vspace), table layout attributes (border, cellpadding, cellspacing, background, colspan, rowspan, nowrap, span and so on), font color/face/size, and list start/type/value.
- **URL schemes:** http, https, mailto, tel, cid and data.
- **Other settings:** `rel="noopener noreferrer"`, comments stripped, and `referrerpolicy="no-referrer"` on every `<img>`.
- **`attribute_filter`** is the single place URLs are decided. ammonia doesn't re-check its own output, so this is a strict allow-list:
  - `href`: http, https, mailto, tel or a `#fragment`. The scheme is computed the way browsers do it.
  - `src` and `background`:
    - `cid:` becomes the asset URL.
    - `data:image/(png|jpeg|gif|webp|bmp|avif|x-icon)` is kept. SVG is not.
    - Remote URLs follow the image policy.
    - Any `gmail-asset.localhost` URL written in the mail itself is dropped.
- **`scrub_css`** (for `style=""` and `<style>`):
  - Remove backslashes and comments, plus `@import` and `@font-face`.
  - Neutralise `expression(`, `behavior:`, `-moz-binding` and `javascript:`.
  - Every `url()` goes through the same classifier.
- **`<body>` lifting:** the first `<body …>` becomes `<div class="gm-body" …>`, so newsletter backgrounds survive.
- 59 of 81 HTML messages (73%) use `<style>`. Sanitizing takes p50 6 ms and p99 40 ms in a debug build.

**CSP** (a meta tag, first in the page):
```
default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action 'none'; base-uri 'none'
```
The page also sets `<meta name="referrer" content="no-referrer">` and `<base target="_blank">`. "Show images" adds `https: http:` to `img-src`.

**Harness** (WebView2, hostile email, every URL pointing at a loopback canary server):
| Configuration | Canary hits | Scripts ran |
|---|---|---|
| **Production: sanitized + CSP + sandbox** | **0** | no |
| Production with `sandbox="allow-popups"` (variant B) | 0 | no |
| Raw HTML + CSP + sandbox (no ammonia) | 0 (the elements stay in the page but do nothing) | no |
| Raw HTML + iframe `csp=` attribute + sandbox | 0 (the `csp=` attribute works as an extra layer) | no |
| Raw HTML + sandbox only (no CSP) | 15 | no |
| Raw HTML, no sandbox, no CSP | 20 | **yes** |

- Layout is preserved (bgcolor, widths, padding).
- `javascript:`, `data:`, `file:` and `ms-msdt:` links lose their href. https, mailto and fragment clicks were intercepted by the parent page.
- The parent webview's `on_navigation` doesn't see navigations inside the iframe, so rely on the sandbox and the sanitizer.
- The iframe's `prefers-color-scheme` follows the system theme.

## Q4. Remote images (81 HTML messages)
- 64 (79%) have remote images, with 520 references.
- 37 have tracking pixels, 51 in total.
- 15 have a remote CSS `url()`.

**Recommendation:**
- **Block by default, in the sanitizer:** remote URLs are removed, so nothing is ever requested.
- **"Show images":** re-render that one message with `ImagePolicy::Allow` and a relaxed CSP. There's no host proxy: it would hide nothing that matters (the IP is the same) and would add SSRF surface.

The privacy cost of "Show images" (measured):
- No cookies are sent.
- `<img>` requests carry no Referer. CSS backgrounds carry a constant `Referer: http://tauri.localhost/`.
- The sender learns the IP, the time, the user agent, and that the message was opened.

## Q5. cid images and attachments
- **Asset URLs:** `http://gmail-asset.localhost/<account>/<msgId>/<partId>`.
  - `partId` is **mail-parser's part index**, not Gmail's partId, so cache keys must use it.
  - Segments are checked against `[A-Za-z0-9_.@+-]` with no leading dot.
  - 19 of 20 cid parts are referenced, and 0 references are unresolved.
- **Asset responses:**
  - Only raster images are served inline.
  - Everything else is `application/octet-stream` with `Content-Disposition: attachment`.
  - All responses add `nosniff` and `CSP: default-src 'none'; sandbox`.
- **Attachments:** 38 on 22 messages (pdf 16, png 16, jpeg 3, octet-stream 2, docx 1). Sizes: p50 106 KB, p90 3.9 MB, max 19 MB.
  - Images referenced inline are hidden from the list.
  - RFC 2231 file names decode.
  - Nested `message/rfc822` parts are listed.
- **"Save attachment" is a user decision.** It writes to the local disk, not to Gmail.

## Q6. Metadata and sender authenticity
**`Authentication-Results`** is parsed for SPF, every DKIM signature and DMARC, giving mailed-by (`smtp.mailfrom`) and signed-by (the first passing `header.d`).
- **Only the first header with a `*.google.com` authserv-id is trusted.** 33 of 90 messages carry more than one.
- Coverage: 82 of 90. The 8 without one are all in Sent.
- Results:
  - SPF pass on 82.
  - DKIM pass 77, neutral 2, absent 3.
  - DMARC pass 76, fail 2, absent 4.
  - Overall: good 77, mixed 3, bad 2.
  - 0 unparsable.
- Display line: `SPF pass · DKIM pass (example.com) · DMARC pass`.

**Header block:**
- From (name and address), To and Cc.
- Reply-To, only when it differs from From.
- Subject.
- Date in local time plus relative ("2 h ago"). Use the `Date` header, falling back to `internalDate`. They differed by 0 s on all 90, so the sample can't tell them apart.
- Labels from `labelIds`. User labels need S2's `labels.list`.

## Q7. Dark mode
- **Always render email HTML on white.** Our own plain-text view follows the theme.
- An experimental invert filter (`HtmlInvert`) distorts brand colours, so offer it as opt-in only, if at all.
- 6 of 81 emails have `@media (prefers-color-scheme: dark)` rules. Those rules apply inside the iframe on a dark-theme system. Whether to let those emails own the whole page is still open.

## Recommended pipeline
1. **Fetch:** `format=raw`. Use `format=full` plus attachments on demand above about 8–10 MB. Cache the raw blob, encrypted (S4).
2. **Parse:** `mail-parser` 1.0 → headers, html/text/`text_items`, attachments, a cid map and the auth summary. Apply the C1 and windows-1252 repair.
3. **Choose the body:** HTML, otherwise `text_to_html`, otherwise "(no displayable body)".
4. **Sanitize:** ammonia, the attribute filter and `scrub_css`, with `ImagePolicy::Block` unless "Show images" is on.
5. **Wrap:** `render::page`, with the CSP first, `<base target=_blank>`, `referrer=no-referrer` and the base CSS.
6. **Show:** `<iframe sandbox srcdoc>`. Also set the iframe `csp=` attribute as a second layer.
   - Variant A: `allow-same-origin`, with a click listener in the parent.
   - Variant B: `allow-popups`, with Rust `on_new_window`. It's stricter and has no DOM access.
7. **Assets:** the `gmail-asset` protocol, reading from the cached raw.
8. **Links:** only http, https, mailto and tel go to the OS opener, after a second scheme check. Fragments scroll inside the frame.

## Still open
- Variant A or B in production. Real clicks (including keyboard and middle-click) in variant B weren't tested by hand.
- Whether CSS background images load when "Show images" is on (they leak the constant Referer).
- The 6 emails with dark-mode rules.
- `format=flowed`.
- The raw-vs-full threshold, and messages over 25 MB.
- "Save attachment" (user decision).
- Release-build timings.
- The real-mail sample in `out\` (74 MB) should be deleted once the build plan is written.
