// S3 harness UI. Vanilla JS. All data from Rust goes in via textContent; only the iframe gets srcdoc.
const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const frame = $("frame");
const messages = []; // postMessage events from the frame (only a script could send these)
window.addEventListener("message", (e) => messages.push(String(e.data).slice(0, 40)));
let linkLog = [];
let seq = 0;

function setSandbox(mode) {
  if (mode === "none") frame.removeAttribute("sandbox");
  else frame.setAttribute("sandbox", mode);
}

// Variant A: with allow-same-origin (and NO allow-scripts) the parent can read the iframe document and attach
// listeners. Nothing in the iframe can run script, so this does not give the mail any access.
function attachInterceptor(doc) {
  const onClick = (e) => {
    const a = e.target.closest && e.target.closest("a[href]");
    if (!a) return;
    e.preventDefault();
    const href = a.getAttribute("href") || "";
    if (href.startsWith("#")) {
      const t = doc.getElementById(decodeURIComponent(href.slice(1)));
      if (t) t.scrollIntoView();
      linkLog.push("fragment");
      return;
    }
    linkLog.push(href.split(":")[0].toLowerCase());
    invoke("link_clicked", { url: a.href });
  };
  doc.addEventListener("click", onClick, true);
  doc.addEventListener("auxclick", onClick, true); // middle click
  doc.addEventListener("contextmenu", (e) => { if (e.target.closest("a[href]")) e.preventDefault(); }, true);
}

async function show(key, o = {}) {
  const my = ++seq;
  const images = o.images ?? $("images").checked;
  const dark = o.dark ?? $("dark").value;
  const sbox = o.sandbox ?? $("sbox").value;
  const r = await invoke("open_message", { key, images, dark });
  if (my !== seq) return r;
  renderHeader(r);
  $("status").textContent = `body=${r.body}  remote_img=${r.report.remote_images} css_url=${r.report.remote_css_urls} tracking=${r.report.tracking_pixels} cid_ok=${r.report.cid_images} cid_missing=${r.report.cid_missing}  csp: ${r.csp}`;
  setSandbox(sbox);
  if (o.cspAttr) frame.setAttribute("csp", o.cspAttr); else frame.removeAttribute("csp");
  await new Promise((resolve) => {
    frame.addEventListener("load", resolve, { once: true });
    frame.srcdoc = r.page;
  });
  if (sbox.includes("allow-same-origin")) {
    try { attachInterceptor(frame.contentDocument); } catch (e) { console.log("no interceptor", e.name); }
  }
  return r;
}

function renderHeader(r) {
  const h = $("hdr");
  h.replaceChildren();
  const m = r.meta;
  if (!m) return;
  const row = (label, text, cls) => {
    if (!text) return;
    const d = document.createElement("div");
    const b = document.createElement("b"); b.textContent = label;
    const s = document.createElement("span"); s.textContent = text; if (cls) s.className = cls;
    d.append(b, s); h.append(d);
  };
  const t = document.createElement("h2"); t.textContent = m.subject; h.append(t);
  row("From", m.from.join(", "));
  row("To", m.to.join(", "));
  row("Cc", m.cc.join(", "));
  if (m.reply_to_differs) row("Reply-To", m.reply_to.join(", "));
  row("Date", `${m.date_local} (${m.date_relative})`);
  row("Labels", m.labels.join(", "));
  if (m.auth) row("Sender", `${m.auth.line}${m.auth.mailed_by ? " · mailed-by " + m.auth.mailed_by : ""}${m.auth.signed_by ? " · signed-by " + m.auth.signed_by : ""}`, m.auth.level);
  else row("Sender", "no Authentication-Results from Gmail", "unknown");
  if (m.attachments.length) {
    const d = document.createElement("div");
    const b = document.createElement("b"); b.textContent = "Attachments"; d.append(b);
    for (const a of m.attachments) {
      const s = document.createElement("span"); s.className = "att";
      s.textContent = `${a.name} (${a.mime}, ${a.size_human})`;
      d.append(s);
    }
    h.append(d);
  }
}

// ---- list ----
let selected = null;
async function init() {
  const items = await invoke("list_items");
  const ul = $("list");
  let group = "";
  for (const it of items) {
    if (it.group !== group) {
      group = it.group;
      const g = document.createElement("li"); g.className = "group"; g.textContent = group; ul.append(g);
    }
    const li = document.createElement("li");
    li.textContent = it.label; li.dataset.key = it.key;
    li.addEventListener("click", () => select(li));
    ul.append(li);
  }
  for (const id of ["images", "dark", "sbox"]) $(id).addEventListener("change", () => selected && show(selected));
  $("autotest").addEventListener("click", () => runAutotest(false));
  if (await invoke("autotest_enabled")) runAutotest(true);
}
function select(li) {
  for (const x of $("list").children) x.setAttribute("aria-selected", String(x === li));
  selected = li.dataset.key;
  show(selected);
}

// ---- self-test ----
const logLine = (s) => invoke("log_line", { line: s });
const CSP_BLOCK = "default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action 'none'; base-uri 'none'";

async function runCase(name, key, o) {
  await invoke("canary_clear");
  messages.length = 0; linkLog = [];
  await invoke("set_nav_guard", { on: o.guard ?? true });
  await show(key, o);
  await sleep(o.wait ?? 1500);
  const hits = await invoke("canary_hits");
  const res = {
    name,
    canary_hits: hits.map((h) => (h.tls ? "tls" : h.path)).sort(),
    referer_on_any_hit: hits.some((h) => h.referer),
    cookie_on_any_hit: hits.some((h) => h.cookie),
    frame_messages: [...messages],
  };
  await logLine(`TEST ${name}: canary_hits=${res.canary_hits.length} [${res.canary_hits.join(" ")}] referer=${res.referer_on_any_hit} cookie=${res.cookie_on_any_hit} messages=[${res.frame_messages.join(",")}]`);
  return res;
}

function probe(doc, win) {
  const q = (s) => doc.querySelectorAll(s).length;
  const cs = (sel, prop) => { const e = doc.querySelector(sel); return e ? win.getComputedStyle(e)[prop] : "(missing)"; };
  const img = (id) => { const e = doc.getElementById(id); return e ? { has_src: e.hasAttribute("src"), complete: e.complete, natural_w: e.naturalWidth } : null; };
  return {
    dangerous_elements: ["script", "form", "input", "button", "iframe", "object", "embed", "svg", "video", "base", "link", "meta[http-equiv]", "title"].map((s) => `${s}:${q(s)}`).join(" "),
    on_attributes: [...doc.querySelectorAll("*")].filter((e) => [...e.attributes].some((a) => a.name.startsWith("on"))).length,
    kept_green_text: cs("#t-keep", "color"),
    body_wrapper_bg: cs(".gm-body", "backgroundColor"),
    table_bg: cs("#t-table", "backgroundColor"),
    table_width: cs("#t-table", "width"),
    td_padding: cs("#t-table td", "paddingTop"),
    remote_img: img("t-remote"), pixel_img: img("t-pixel"), data_img: img("t-data"),
    dark_media_iframe: win.matchMedia("(prefers-color-scheme: dark)").matches,
    dark_media_parent: window.matchMedia("(prefers-color-scheme: dark)").matches,
    location: String(win.location.href).slice(0, 20),
  };
}

async function clickAll(doc) {
  const out = [];
  for (const a of doc.querySelectorAll("a")) {
    const had = a.hasAttribute("href");
    let prevented = null;
    if (had) prevented = !a.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    out.push(`${a.id}:${had ? (prevented ? "href-intercepted" : "NOT-INTERCEPTED") : "no-href"}`);
  }
  await sleep(300);
  return out;
}

async function runAutotest(exit) {
  const results = [];
  try {
    await logLine("=== self-test start ===");
    // 1. Production pipeline, variant A.
    let r = await runCase("prod (sanitized+CSP+sandbox A)", "hostile:prod", { sandbox: "allow-same-origin", guard: true });
    const p = probe(frame.contentDocument, frame.contentWindow);
    await logLine(`PROBE prod: ${JSON.stringify(p)}`);
    const clicks = await clickAll(frame.contentDocument);
    await logLine(`CLICKS prod: ${clicks.join(" ")} | parent saw schemes [${linkLog.join(",")}] | location after clicks ${String(frame.contentWindow.location.href).slice(0, 20)}`);
    results.push(r);
    // 2. Images allowed (per-message CSP relax + sanitizer keeps remote src).
    r = await runCase("prod + Show images", "hostile:prod-images", { sandbox: "allow-same-origin", images: true, guard: true });
    await logLine(`PROBE prod-images: ${JSON.stringify(probe(frame.contentDocument, frame.contentWindow))}`);
    results.push(r);
    // 3. Variant B: no allow-same-origin. Parent cannot read the frame.
    r = await runCase("prod (sandbox B allow-popups)", "hostile:prod", { sandbox: "allow-popups", guard: true });
    let readable = true;
    try { readable = frame.contentDocument !== null; } catch { readable = false; }
    await logLine(`B: parent can read iframe document: ${readable}`);
    results.push(r);
    // 8. cid images via the gmail-asset protocol.
    await invoke("canary_clear");
    for (const [round, name] of [[1, "related_cid"], [2, "text_inline_image"], [3, "related_cid"]]) {
      await show("syn:" + name, { sandbox: "allow-same-origin", guard: true });
      const t0 = performance.now();
      while (performance.now() - t0 < 6000 && ![...frame.contentDocument.images].some((i) => i.naturalWidth > 0)) await sleep(50);
      await logLine(`CID ${name} round ${round}: first image ready after ${Math.round(performance.now() - t0)} ms`);
      const im = [...frame.contentDocument.images].map((i) => `${i.hasAttribute("src") ? "src" : "nosrc"}:${i.complete}:${i.naturalWidth}`);
      await logLine(`CID synthetic related_cid round ${round} images: ${im.join(" ")}`);
    }
    await show("syn:text_inline_image", { sandbox: "allow-same-origin", guard: true });
    await sleep(800);
    await logLine(`CID synthetic text_inline_image images: ${[...frame.contentDocument.images].map((i) => `${i.complete}:${i.naturalWidth}`).join(" ")}`);
    // 4. Unsanitized hostile HTML in the production wrapper: CSP + sandbox only. Navigation guard off, to see raw behaviour.
    r = await runCase("RAW + CSP + sandbox (no ammonia)", "hostile:raw-csp", { sandbox: "allow-same-origin", guard: false });
    await logLine(`PROBE raw-csp: ${JSON.stringify(probe(frame.contentDocument, frame.contentWindow))}`);
    results.push(r);
    // 5. Unsanitized, sandbox only, no CSP: positive control that the canary sees resource loads.
    r = await runCase("RAW + sandbox only (no CSP)", "hostile:raw-nocsp", { sandbox: "allow-same-origin", guard: false });
    results.push(r);
    // 6. Unsanitized, no CSP in the document, iframe csp= attribute only.
    r = await runCase("RAW + iframe csp attribute + sandbox", "hostile:raw-nocsp", { sandbox: "allow-same-origin", cspAttr: CSP_BLOCK, guard: false });
    results.push(r);
    // 7. Unsanitized, no sandbox, no CSP: positive control that scripts run (and why sandbox matters).
    r = await runCase("RAW + NO sandbox + NO CSP (control)", "hostile:raw-open", { sandbox: "none", guard: false, wait: 2000 });
    results.push(r);
    await invoke("set_nav_guard", { on: true });
    await logLine("=== self-test done ===");
  } catch (e) {
    await logLine(`self-test ERROR ${e && e.message ? e.message : e}`);
  }
  if (exit) await invoke("exit_app");
}

init();
