"use strict";
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
let timer = null;

function log(line) {
  const t = new Date().toISOString().slice(11, 19);
  $("log").textContent += `[${t}] ${line}\n`;
  $("log").scrollTop = $("log").scrollHeight;
}
function notice(text, cls) {
  const n = $("notice");
  if (!text) { n.hidden = true; return; }
  n.hidden = false; n.className = "banner " + (cls || ""); n.textContent = "";
  const s = document.createElement("span"); s.textContent = text; n.append(s);
  return n;
}
function errText(e) { return e && e.message ? e.message : String(e); }

async function refresh() {
  const s = await invoke("snapshot");
  $("dir").textContent = s.data_dir;
  const ul = $("accounts"); ul.textContent = "";
  if (!s.accounts.length) { const li = document.createElement("li"); li.textContent = "No accounts. State: signed out."; ul.append(li); }
  for (const a of s.accounts) {
    const li = document.createElement("li");
    const who = document.createElement("div"); who.className = "who";
    const em = document.createElement("div"); em.className = "email"; em.textContent = a.email;
    const meta = document.createElement("div"); meta.className = "muted small";
    meta.textContent = `sub ${a.sub} | signed in ${a.signed_in_at} | scopes: ${a.scopes.map(x => x.replace("https://www.googleapis.com/auth/", "")).join(" ")}`;
    who.append(em, meta);
    const badge = document.createElement("span"); badge.className = "badge " + a.state;
    badge.textContent = a.state === "signed_in" ? "signed in" : "needs re-auth";
    li.append(who, badge);
    if (a.state === "needs_reauth") {
      const m = document.createElement("div"); m.className = "small"; m.textContent = "Access removed or expired. Sign in again.";
      const b = document.createElement("button"); b.textContent = "Sign in again";
      b.onclick = () => add(a.email); li.append(m, b);
    }
    const chk = document.createElement("button"); chk.textContent = "Check";
    chk.onclick = () => check(a, chk);
    const out = document.createElement("button"); out.textContent = "Sign out";
    out.onclick = () => signOut(a, out);
    li.append(chk, out);
    ul.append(li);
  }
  const p = $("pending");
  p.hidden = !s.pending;
  clearInterval(timer);
  if (s.pending) {
    const upd = () => {
      const left = Math.max(0, s.pending.started + s.pending.timeout_secs - Math.floor(Date.now() / 1000));
      $("pending-text").textContent = `Pending: waiting for the browser (times out in ${left}s). State: pending.`;
    };
    upd(); timer = setInterval(upd, 1000);
  }
  $("add").disabled = false; // clicking while pending is allowed on purpose: the host must refuse it
  if (s.unreadable_files) notice(`${s.unreadable_files} account file(s) could not be decrypted and are skipped.`, "bad");
}

async function add(hint) {
  const h = typeof hint === "string" ? hint : $("hint").value.trim();
  notice("");
  log(`add_account hint=${h || "(none)"} timeout=${$("timeout").value}s probe=${$("probe").checked}`);
  try {
    const r = await invoke("add_account", { hint: h || null, timeoutSecs: Number($("timeout").value) || 300, probe: $("probe").checked });
    log(`add OK: ${r.email} replaced=${r.replaced} hint_mismatch=${r.hint_mismatch} messagesTotal=${r.messages_total} read_only=${r.read_only} tokeninfo_scopes=[${r.tokeninfo_scopes.map(x => x.replace("https://www.googleapis.com/auth/", "")).join(" ")}]`);
    notice(`Signed in ${r.email}${r.replaced ? " (replaced the existing entry)" : ""}${r.hint_mismatch ? " - NOTE: not the account in the hint" : ""}.`, "ok");
  } catch (e) {
    log(`add FAILED kind=${e.kind}: ${errText(e)}${e.detail ? " | " + e.detail : ""}`);
    if (e.kind === "already_pending") { const n = notice(errText(e)); }
    else if (e.kind === "missing_scope") {
      const n = notice(errText(e) + (e.detail ? "  (" + e.detail + ")" : ""), "bad");
      const b = document.createElement("button"); b.textContent = "Try again"; b.onclick = () => add(h); n.append(b);
    } else if (e.kind === "cancelled") notice("Sign-in cancelled.");
    else if (e.kind === "timeout") {
      const n = notice(errText(e)); const b = document.createElement("button"); b.textContent = "Try again"; b.onclick = () => add(h); n.append(b);
    } else notice(errText(e), "bad");
  }
  await refresh();
}

async function check(a, btn) {
  btn.disabled = true; log(`check ${a.email} ...`);
  try {
    const r = await invoke("check_account", { sub: a.sub });
    log(`check OK ${a.email}: messagesTotal=${r.messages_total} read_only=${r.verdict.read_only} extra=[${r.verdict.extra}] tokeninfo(POST)=[${r.tokeninfo_scopes.map(x => x.replace("https://www.googleapis.com/auth/", "")).join(" ")}] expires_in=${r.expires_in}s ms(refresh,tokeninfo,profile)=${r.timings_ms}`);
  } catch (e) {
    log(`check FAILED ${a.email} kind=${e.kind}: ${errText(e)}`);
    notice(e.kind === "revoked" ? `${a.email}: access removed or expired. Sign in again.` : errText(e), "bad");
  }
  btn.disabled = false; await refresh();
}

async function signOut(a, btn) {
  const offline = $("offline").checked, resend = $("resend").checked;
  btn.disabled = true; log(`sign_out ${a.email} simulate_offline=${offline} resend=${resend} ...`);
  try {
    const r = await invoke("sign_out", { sub: a.sub, simulateOffline: offline, resendRevoke: resend });
    const f = (o) => o ? `${o.result} status=${o.status} error=${o.error}` : "-";
    log(`sign_out done ${a.email}: revoke[${f(r.revoke)}] again[${f(r.revoke_again)}] local_deleted=${r.local_deleted} remove_manually=${r.remove_manually}`);
    notice(r.remove_manually
      ? `${a.email} removed from this app, but Google could not be reached. Remove access at myaccount.google.com > Security > Third-party apps and services.`
      : `${a.email} signed out and access revoked at Google.`, r.remove_manually ? "bad" : "ok");
  } catch (e) { log(`sign_out FAILED kind=${e.kind}: ${errText(e)}`); notice(errText(e), "bad"); }
  await refresh();
}

$("add").onclick = () => add();
$("cancel").onclick = async () => { log("cancel_add -> " + (await invoke("cancel_add"))); };
$("reopen").onclick = async () => { try { await invoke("reopen_browser"); log("re-opened browser page"); } catch (e) { log("reopen failed: " + errText(e)); } };
$("embedded").onclick = async () => { try { log("opening embedded webview"); await invoke("try_embedded"); } catch (e) { log("embedded failed: " + errText(e)); } };
$("copy").onclick = () => navigator.clipboard.writeText($("log").textContent).then(() => log("(log copied)"));
$("clear").onclick = () => { $("log").textContent = ""; };
listen("embedded-nav", (ev) => log("embedded nav: " + ev.payload));
listen("auth-started", refresh);
listen("auth-finished", refresh);
refresh().catch((e) => log("snapshot failed: " + errText(e)));
