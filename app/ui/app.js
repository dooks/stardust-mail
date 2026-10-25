// gmail-ro UI. Vanilla JS; all text via textContent (never innerHTML) except the iframe srcdoc.
(function () {
  "use strict";

  var tauri = window.__TAURI__ || {};
  var invoke = tauri.core && tauri.core.invoke;
  var listen = tauri.event && tauri.event.listen;
  var $ = function (id) { return document.getElementById(id); };

  var S = {
    accounts: [],
    accountId: null,
    folders: [],
    folderId: "INBOX",
    folderByAccount: {},
    list: { rows: [], more: false, endOfCache: false },
    cursor: -1,        // keyboard/selected row index
    openId: null,      // message shown in the reader
    msg: null,
    status: {},        // accountId -> SyncStatus
    catCollapsed: false,
    loadingMore: false,
  };
  var seq = { list: 0, msg: 0, folders: 0 };

  // ---------- helpers ----------
  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }
  function clear(n) { while (n.firstChild) n.removeChild(n.firstChild); }
  function call(cmd, args) {
    if (!invoke) return Promise.reject({ code: "internal", message: "No Tauri bridge" });
    return invoke(cmd, args);
  }
  function errText(e) { return (e && e.message) ? e.message : String(e || "Error"); }
  function pad(n) { return n < 10 ? "0" + n : "" + n; }
  function hm(d) { return pad(d.getHours()) + ":" + pad(d.getMinutes()); }
  function sameDay(a, b) { return a.toDateString() === b.toDateString(); }
  var MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

  function listTime(ms) {
    var d = new Date(ms), now = new Date();
    if (sameDay(d, now)) return hm(d);
    if (d.getFullYear() === now.getFullYear()) return MONTHS[d.getMonth()] + " " + d.getDate();
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate());
  }
  function relative(ms) {
    var s = Math.round((Date.now() - ms) / 1000);
    if (s < 0) return "in the future";
    if (s < 60) return "just now";
    var m = Math.round(s / 60);
    if (m < 60) return m + " min ago";
    var h = Math.round(m / 60);
    if (h < 48) return h + " h ago";
    return Math.round(h / 24) + " days ago";
  }
  function fullDate(ms) {
    var d = new Date(ms);
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + " " + hm(d);
  }
  function fmtSize(n) {
    if (n < 1024) return n + " B";
    if (n < 1048576) return Math.round(n / 1024) + " KB";
    return (n / 1048576).toFixed(1) + " MB";
  }
  function addrText(a) { return a.name ? a.name + " <" + a.addr + ">" : a.addr; }
  function curAccount() {
    for (var i = 0; i < S.accounts.length; i++) if (S.accounts[i].id === S.accountId) return S.accounts[i];
    return null;
  }

  var toastTimer = null;
  function toast(text, isError) {
    var t = $("toast");
    t.textContent = text;
    t.className = "toast" + (isError ? " error" : "");
    t.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { t.hidden = true; }, isError ? 6000 : 3000);
  }

  // ---------- modal ----------
  var modalEl = null;
  // spec: {title, paragraphs: [{text, dim}], buttons: [{label, cls, onClick}]}; returns {close, update}
  function showModal(spec) {
    closeModal();
    var overlay = el("div", "overlay");
    var box = el("div", "modal");
    box.setAttribute("role", "dialog");
    box.setAttribute("aria-modal", "true");
    overlay.appendChild(box);
    $("modal-root").appendChild(overlay);
    modalEl = overlay;
    function render(s) {
      clear(box);
      box.appendChild(el("h3", null, s.title));
      (s.paragraphs || []).forEach(function (p) { box.appendChild(el("p", p.dim ? "dim" : null, p.text)); });
      var row = el("div", "buttons");
      (s.buttons || []).forEach(function (b) {
        var btn = el("button", "btn" + (b.cls ? " " + b.cls : ""), b.label);
        btn.type = "button";
        btn.addEventListener("click", b.onClick);
        row.appendChild(btn);
      });
      box.appendChild(row);
      var first = row.querySelector(".primary, .danger") || row.lastChild;
      if (first) first.focus();
    }
    render(spec);
    return { close: closeModal, update: render };
  }
  function closeModal() {
    if (modalEl && modalEl.parentNode) modalEl.parentNode.removeChild(modalEl);
    modalEl = null;
  }

  // ---------- status / refresh ----------
  function renderStatus() {
    var acc = curAccount();
    var st = acc ? S.status[acc.id] : null;
    var text = "";
    var busy = false;
    if (!acc) text = "No account";
    else if (!st) text = "";
    else {
      var asOf = st.asOf ? fullDate(st.asOf) : null;
      if (st.state === "offline") text = "Offline: showing cached mail" + (asOf ? " as of " + asOf : "");
      else if (st.state === "throttled") text = "Throttled by Google, retrying later" + (asOf ? " (mail as of " + asOf + ")" : "");
      else if (st.state === "needsReauth") text = "Sign-in expired: showing cached mail" + (asOf ? " as of " + asOf : "");
      else if (st.state === "syncing") { text = "Syncing…"; busy = true; }
      else text = asOf ? "Up to date as of " + asOf : "Up to date";
    }
    $("status-text").textContent = text;
    $("refresh").classList.toggle("busy", busy);
    $("refresh").disabled = !acc;
  }
  function doRefresh() {
    var acc = curAccount();
    if (!acc) return;
    call("refresh", { accountId: acc.id }).catch(function (e) { toast(errText(e), true); });
  }

  function renderBanner() {
    var acc = curAccount();
    var show = acc && acc.state === "needsReauth";
    $("banner").hidden = !show;
    if (show) $("banner-text").textContent = acc.email + ": sign-in expired. Cached mail is still readable.";
  }

  // ---------- accounts ----------
  function renderAccounts() {
    var ul = $("accounts");
    clear(ul);
    S.accounts.forEach(function (a, idx) {
      var li = el("li", "account");
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", a.id === S.accountId ? "true" : "false");
      li.title = (idx < 9 ? "Alt/number key " + (idx + 1) + ": " : "") + a.email;
      li.appendChild(el("span", "email", a.email));
      if (a.state === "needsReauth") li.appendChild(el("span", "pill warn", "sign in"));
      if (a.inboxUnread) {
        var p = el("span", "pill", String(a.inboxUnread));
        p.title = "Unread in Inbox";
        li.appendChild(p);
      }
      var more = el("button", "more", "⋯");
      more.type = "button";
      more.title = "Account menu";
      more.setAttribute("aria-label", "Account menu for " + a.email);
      more.addEventListener("click", function (ev) { ev.stopPropagation(); openAccountMenu(a, ev.clientX, ev.clientY); });
      li.appendChild(more);
      li.addEventListener("click", function () { selectAccount(a.id); });
      li.addEventListener("contextmenu", function (ev) { ev.preventDefault(); openAccountMenu(a, ev.clientX, ev.clientY); });
      ul.appendChild(li);
    });
    // highlight the selected account
    Array.prototype.forEach.call(ul.children, function (li) {
      li.style.background = li.getAttribute("aria-selected") === "true" ? "var(--sel)" : "";
    });
  }

  function openAccountMenu(acc, x, y) {
    var m = $("menu");
    clear(m);
    function item(label, fn) {
      var b = el("button", null, label);
      b.type = "button";
      b.setAttribute("role", "menuitem");
      b.addEventListener("click", function () { closeMenu(); fn(); });
      m.appendChild(b);
    }
    if (acc.state === "needsReauth") item("Sign in again", function () { startSignIn("reauth", acc); });
    item("Sign out…", function () { confirmSignOut(acc); });
    m.style.left = Math.min(x, window.innerWidth - 170) + "px";
    m.style.top = Math.min(y, window.innerHeight - 90) + "px";
    m.hidden = false;
  }
  function closeMenu() { $("menu").hidden = true; }

  function confirmSignOut(acc) {
    showModal({
      title: "Sign out " + acc.email + "?",
      paragraphs: [
        { text: "This removes the account from this app and deletes its locally cached mail." },
        { text: "It also revokes this app's access to the account at Google. You would have to sign in and grant access again to use it later.", dim: true },
      ],
      buttons: [
        { label: "Cancel", onClick: closeModal },
        { label: "Sign out", cls: "danger", onClick: function () { doSignOut(acc); } },
      ],
    });
  }
  function doSignOut(acc) {
    call("sign_out", { accountId: acc.id }).then(function (res) {
      closeModal();
      return reloadAccounts(true).then(function () {
        if (res && res.manualRemovalNeeded) {
          showModal({
            title: "Signed out of " + acc.email,
            paragraphs: [
              { text: "The app could not tell Google to revoke its access. Local data was deleted, but you should remove access manually." },
              { text: "Open myaccount.google.com → Security → Third-party apps and remove gmail-ro.", dim: true },
            ],
            buttons: [{ label: "OK", cls: "primary", onClick: closeModal }],
          });
        } else {
          toast("Signed out " + acc.email);
        }
      });
    }).catch(function (e) { closeModal(); toast(errText(e), true); });
  }

  function reloadAccounts(afterRemoval) {
    return call("accounts").then(function (list) {
      S.accounts = list || [];
      var still = S.accounts.some(function (a) { return a.id === S.accountId; });
      if (!still) {
        S.accountId = null;
        if (S.accounts.length) return selectAccount(S.accounts[0].id);
        clearAccountView();
      }
      renderAccounts();
      renderBanner();
      renderStatus();
    });
  }

  function clearAccountView() {
    S.folders = []; S.list = { rows: [], more: false, endOfCache: false }; S.cursor = -1;
    renderFolders(); renderList(); showMessage(null);
    renderAccounts(); renderBanner(); renderStatus();
  }

  function selectAccount(id) {
    if (S.accountId === id) return Promise.resolve();
    S.accountId = id;
    S.folderId = S.folderByAccount[id] || "INBOX";
    S.openId = null; S.msg = null; S.cursor = -1;
    S.list = { rows: [], more: false, endOfCache: false };
    showMessage(null);
    renderAccounts(); renderBanner(); renderStatus(); renderList();
    var mine = ++seq.folders;
    return call("folders", { accountId: id }).then(function (f) {
      if (mine !== seq.folders) return;
      S.folders = f || [];
      renderFolders();
    }, function (e) { toast(errText(e), true); }).then(function () {
      return selectFolder(S.folderId, true);
    });
  }

  // ---------- folders ----------
  function renderFolders() {
    var ul = $("folders");
    clear(ul);
    S.folders.forEach(function (f) {
      var inCat = f.kind === "category";
      if (inCat && S.catCollapsed) return;
      var isCatHead = f.kind === "categoryGroup";
      var li = el("li", "folder" + (f.clickable ? "" : " group") + (isCatHead ? " collapsible" : "") + (f.unread ? " unread" : ""));
      li.style.paddingLeft = (12 + f.depth * 14) + "px";
      li.dataset.id = f.id;
      if (isCatHead) {
        li.appendChild(el("span", "caret", S.catCollapsed ? "▸" : "▾"));
        li.setAttribute("aria-expanded", S.catCollapsed ? "false" : "true");
      }
      li.appendChild(el("span", "name", f.name));
      if (f.unread) li.appendChild(el("span", "count", String(f.unread)));
      if (f.clickable) {
        li.setAttribute("role", "option");
        li.setAttribute("aria-selected", f.id === S.folderId ? "true" : "false");
        if (f.id === S.folderId) li.style.background = "var(--sel)";
        li.addEventListener("click", function () { selectFolder(f.id); });
      } else if (isCatHead) {
        li.addEventListener("click", function () { S.catCollapsed = !S.catCollapsed; renderFolders(); });
      } else {
        li.setAttribute("aria-disabled", "true");
      }
      ul.appendChild(li);
    });
  }
  function refreshFolderCounts() {
    var id = S.accountId;
    if (!id) return;
    var mine = ++seq.folders;
    call("folders", { accountId: id }).then(function (f) {
      if (mine !== seq.folders || id !== S.accountId) return;
      S.folders = f || [];
      renderFolders();
    }, function () {});
    call("accounts").then(function (list) {
      S.accounts = list || S.accounts;
      renderAccounts(); renderBanner();
    }, function () {});
  }

  function selectFolder(id, force) {
    if (!force && id === S.folderId) return Promise.resolve();
    S.folderId = id;
    S.folderByAccount[S.accountId] = id;
    S.cursor = -1;
    S.list = { rows: [], more: false, endOfCache: false };
    renderFolders();
    renderList("Loading…");
    call("set_view", { accountId: S.accountId, folderId: id }).catch(function () {});
    return loadList(false);
  }

  // ---------- message list ----------
  function folderName() {
    for (var i = 0; i < S.folders.length; i++) if (S.folders[i].id === S.folderId) return S.folders[i].name;
    return "";
  }

  // mode: false = (re)load first page, true = load older
  function loadList(more) {
    var accountId = S.accountId, folderId = S.folderId;
    if (!accountId) return Promise.resolve();
    var mine = ++seq.list;
    if (more) S.loadingMore = true;
    return call("messages", { accountId: accountId, folderId: folderId, more: more ? true : undefined }).then(function (res) {
      if (mine !== seq.list || accountId !== S.accountId || folderId !== S.folderId) return;
      S.loadingMore = false;
      mergeList(res, more);
      renderList();
    }, function (e) {
      if (mine !== seq.list) return;
      S.loadingMore = false;
      renderList(errText(e));
      toast(errText(e), true);
    });
  }
  function mergeList(res, more) {
    var rows = res.rows || [];
    var old = S.list.rows;
    var keepMore = res.more, keepEnd = res.endOfCache;
    if (old.length) {
      var byId = {};
      rows.forEach(function (r) { byId[r.id] = true; });
      if (more) {
        // Accept either an extended or an incremental result: merge by id.
        old.forEach(function (r) { if (!byId[r.id]) rows = rows.concat([r]); });
      } else {
        // Reload of the first page: keep already-loaded older rows below it.
        var oldest = rows.length ? rows[rows.length - 1].internalDate : 0;
        var older = old.filter(function (r) { return !byId[r.id] && r.internalDate < oldest; });
        if (older.length) { rows = rows.concat(older); keepMore = S.list.more; keepEnd = S.list.endOfCache; }
      }
      rows = rows.slice().sort(function (a, b) { return b.internalDate - a.internalDate; });
    }
    var openIdx = -1;
    rows.forEach(function (r, i) { if (r.id === S.openId) openIdx = i; });
    S.list = { rows: rows, more: keepMore, endOfCache: keepEnd };
    S.cursor = openIdx >= 0 ? openIdx : Math.min(S.cursor, rows.length - 1);
  }

  function renderList(placeholder) {
    var ul = $("messages");
    var scroll = ul.scrollTop;
    clear(ul);
    $("list-heading").textContent = folderName();
    var rows = S.list.rows;
    var empty = $("list-empty");
    if (!rows.length) {
      empty.hidden = false;
      empty.textContent = placeholder || (S.accountId ? "No messages" : "Add an account to get started");
      return;
    }
    empty.hidden = true;
    rows.forEach(function (r, i) {
      var li = el("li", "msg" + (r.unread ? " unread" : "") + (i === S.cursor ? " cursor" : ""));
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", r.id === S.openId ? "true" : "false");
      li.dataset.idx = String(i);
      li.appendChild(el("span", "dot"));
      li.appendChild(el("span", "from", r.fromName || r.fromAddr));
      li.appendChild(el("span", "when", listTime(r.internalDate)));
      li.appendChild(el("span", "subject", r.subject || "(no subject)"));
      var icons = el("span", "icons");
      if (r.starred) { var st = el("span", "star", "★"); st.title = "Starred"; icons.appendChild(st); }
      if (r.attachmentHint) { var pc = el("span", "clip", "📎"); pc.title = "Has attachments"; icons.appendChild(pc); }
      li.appendChild(icons);
      li.appendChild(el("span", "snippet", r.snippet));
      li.addEventListener("click", function () { S.cursor = i; openRow(i); });
      ul.appendChild(li);
    });
    if (S.list.more) {
      var foot = el("li", "list-foot");
      var b = el("button", "btn", S.loadingMore ? "Loading…" : "Load older");
      b.type = "button";
      b.disabled = S.loadingMore;
      b.addEventListener("click", function () { loadList(true); renderList(); });
      foot.appendChild(b);
      ul.appendChild(foot);
    } else if (S.list.endOfCache) {
      ul.appendChild(el("li", "list-foot", "End of cached mail"));
    }
    ul.scrollTop = scroll;
  }

  function moveCursor(delta) {
    var n = S.list.rows.length;
    if (!n) return;
    S.cursor = Math.max(0, Math.min(n - 1, (S.cursor < 0 ? (delta > 0 ? -1 : n) : S.cursor) + delta));
    renderList();
    var cur = $("messages").querySelector(".cursor");
    if (cur && cur.scrollIntoView) cur.scrollIntoView({ block: "nearest" });
  }

  // ---------- reader ----------
  function openRow(i) {
    var r = S.list.rows[i];
    if (!r) return;
    S.openId = r.id;
    renderList();
    loadMessage(r.id, false);
  }
  function loadMessage(id, showImages) {
    var accountId = S.accountId;
    var mine = ++seq.msg;
    return call("message", { accountId: accountId, messageId: id, showImages: !!showImages }).then(function (m) {
      if (mine !== seq.msg || accountId !== S.accountId) return;
      showMessage(m);
    }, function (e) {
      if (mine !== seq.msg) return;
      showMessage(null, errText(e));
    });
  }

  function showMessage(m, errMsg) {
    S.msg = m;
    var reader = $("reader"), emptyEl = $("read-empty");
    if (!m) {
      reader.hidden = true;
      emptyEl.hidden = false;
      emptyEl.textContent = errMsg || "Select a message";
      $("mail-frame").srcdoc = "";
      return;
    }
    emptyEl.hidden = true;
    reader.hidden = false;
    $("h-subject").textContent = m.subject || "(no subject)";
    var f = $("h-fields");
    clear(f);
    function field(k, v) { if (!v) return; f.appendChild(el("dt", null, k)); f.appendChild(el("dd", null, v)); }
    field("From", addrText(m.from));
    field("To", (m.to || []).map(addrText).join(", "));
    field("Cc", (m.cc || []).map(addrText).join(", "));
    if (m.replyTo) field("Reply-To", addrText(m.replyTo));
    field("Date", fullDate(m.date) + " (" + relative(m.date) + ")");
    var chips = $("h-labels");
    clear(chips);
    (m.labels || []).forEach(function (l) { chips.appendChild(el("span", "chip", l)); });
    var level = m.auth && m.auth.level ? m.auth.level : "none";
    var badge = $("h-auth-badge");
    badge.className = "badge " + level;
    badge.textContent = level === "good" ? "Sender verified" : level === "mixed" ? "Partly verified" : level === "bad" ? "Check failed" : "Unchecked";
    $("h-auth-line").textContent = m.auth && m.auth.line ? m.auth.line : "No sender check available";

    var atts = $("atts");
    clear(atts);
    atts.hidden = !(m.attachments && m.attachments.length);
    (m.attachments || []).forEach(function (a) {
      var box = el("div", "att");
      box.appendChild(el("span", null, a.name));
      box.appendChild(el("span", "size", fmtSize(a.size)));
      var b = el("button", "btn", "Save…");
      b.type = "button";
      b.addEventListener("click", function () {
        b.disabled = true;
        call("save_attachment", { accountId: S.accountId, messageId: m.id, partId: a.partId }).then(function (res) {
          if (res && res.saved) toast("Saved " + (res.fileName || a.name));
        }, function (e) { toast(errText(e), true); }).then(function () { b.disabled = false; });
      });
      box.appendChild(b);
      atts.appendChild(box);
    });

    var blocked = m.remoteImagesBlocked || 0;
    var bar = $("img-bar");
    bar.hidden = !(blocked > 0 && !m.imagesShown);
    $("img-bar-text").textContent = "Remote images are blocked for your privacy.";
    $("img-show").textContent = "Show images (" + blocked + " blocked)";

    $("mail-frame").srcdoc = m.page || "";
  }

  // ---------- sign-in ----------
  var signIn = null; // {modal, kind, acc}
  function startSignIn(kind, acc) {
    var title = kind === "reauth" ? "Sign in again: " + acc.email : "Add a Gmail account";
    showModal({
      title: title,
      paragraphs: [
        { text: "Your browser will open Google’s sign-in page." },
        { text: "On Google’s page, tick “Read all your email”. If you leave it unticked the app cannot show your mail." },
        { text: "The app is read-only: it can never send, change or delete mail.", dim: true },
      ],
      buttons: [
        { label: "Cancel", onClick: closeModal },
        { label: "Continue", cls: "primary", onClick: function () { runSignIn(kind, acc); } },
      ],
    });
  }
  function waitingSpec(kind, acc) {
    return {
      title: "Waiting for Google…",
      paragraphs: [
        { text: "Finish signing in in your browser, then come back here. Remember to tick “Read all your email”." },
        { text: "Browser did not open? Use Re-open.", dim: true },
      ],
      buttons: [
        { label: "Re-open browser", onClick: function () { call("reopen_sign_in").catch(function (e) { toast(errText(e), true); }); } },
        { label: "Cancel", onClick: function () { call("cancel_sign_in").catch(function () {}); } },
      ],
    };
  }
  function failSpec(kind, acc, title, text, canRetry) {
    var buttons = [{ label: "Close", onClick: closeModal }];
    if (canRetry) buttons.push({ label: "Try again", cls: "primary", onClick: function () { startSignIn(kind, acc); } });
    return { title: title, paragraphs: [{ text: text }], buttons: buttons };
  }
  function runSignIn(kind, acc) {
    var modal = showModal(waitingSpec(kind, acc));
    signIn = { modal: modal };
    var p = kind === "reauth" ? call("reauth", { accountId: acc.id }) : call("add_account", {});
    p.then(function (view) {
      signIn = null;
      closeModal();
      toast(kind === "reauth" ? "Signed in again" : "Added " + (view && view.email ? view.email : "account"));
      return reloadAccounts().then(function () {
        if (view && view.id) { var id = view.id; if (S.accountId === id) { S.accountId = null; } return selectAccount(id); }
      });
    }, function (e) {
      signIn = null;
      var d = e && e.detail;
      if (d === "missing_scope") {
        modal.update(failSpec(kind, acc, "Gmail access was not granted",
          "Google did not give this app permission to read your email, so nothing was saved. On Google’s page you need to tick “Read all your email”.", true));
      } else if (d === "cancelled") {
        closeModal();
        toast("Sign-in cancelled");
      } else if (d === "timeout") {
        modal.update(failSpec(kind, acc, "Sign-in timed out", "No sign-in was completed within 5 minutes.", true));
      } else if (d === "denied") {
        modal.update(failSpec(kind, acc, "Access denied", "Access was denied on Google’s page.", true));
      } else if (d === "already_pending") {
        modal.update(failSpec(kind, acc, "Sign-in already in progress", "Finish or cancel the other sign-in first.", false));
      } else {
        modal.update(failSpec(kind, acc, "Sign-in failed", errText(e), true));
      }
    });
  }

  // ---------- events & keyboard ----------
  function wireEvents() {
    if (!listen) return;
    listen("mail-changed", function (ev) {
      var p = ev.payload || {};
      if (p.accountId !== S.accountId) {
        return;
      }
      var labels = p.labels || [];
      if (!labels.length || S.folderId === "ALL" || labels.indexOf(S.folderId) >= 0) loadList(false);
      refreshFolderCounts();
    });
    listen("sync-status", function (ev) {
      var s = ev.payload;
      if (!s) return;
      S.status[s.accountId] = s;
      if (s.accountId === S.accountId) renderStatus();
    });
    listen("account-changed", function (ev) {
      var a = ev.payload;
      if (!a) return;
      var found = false;
      S.accounts = S.accounts.map(function (x) { if (x.id === a.id) { found = true; return a; } return x; });
      if (!found) S.accounts.push(a);
      renderAccounts(); renderBanner(); renderStatus();
    });
    listen("sign-in-progress", function (ev) {
      var p = ev.payload || {};
      if (p.stage === "error" && p.message && !signIn) toast(p.message, true);
    });
  }

  function typing(t) {
    return t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
  }
  document.addEventListener("click", function () { closeMenu(); });
  document.addEventListener("keydown", function (ev) {
    if (ev.key === "Escape") { closeMenu(); return; }
    if (modalEl || typing(ev.target) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    var onButton = ev.target && ev.target.tagName === "BUTTON";
    if (ev.key === "ArrowDown") { ev.preventDefault(); moveCursor(1); }
    else if (ev.key === "ArrowUp") { ev.preventDefault(); moveCursor(-1); }
    else if (ev.key === "Enter" && !onButton) { if (S.cursor >= 0) { ev.preventDefault(); openRow(S.cursor); } }
    else if (ev.key === "r" || ev.key === "R") { doRefresh(); }
    else if (ev.key >= "1" && ev.key <= "9") {
      var a = S.accounts[Number(ev.key) - 1];
      if (a) selectAccount(a.id);
    }
  });

  // ---------- init ----------
  function init() {
    if (window.__MOCK__) $("mock-badge").hidden = false;
    $("refresh").addEventListener("click", doRefresh);
    $("add-account").addEventListener("click", function () { startSignIn("add"); });
    $("banner-btn").addEventListener("click", function () { var a = curAccount(); if (a) startSignIn("reauth", a); });
    $("img-show").addEventListener("click", function () { if (S.msg) loadMessage(S.msg.id, true); });
    wireEvents();
    renderList();
    renderStatus();
    call("status").then(function (list) {
      (list || []).forEach(function (s) { S.status[s.accountId] = s; });
    }, function () {}).then(function () {
      return call("accounts");
    }).then(function (list) {
      S.accounts = list || [];
      renderAccounts();
      if (S.accounts.length) return selectAccount(S.accounts[0].id);
      renderList();
    }).catch(function (e) {
      $("list-empty").hidden = false;
      $("list-empty").textContent = errText(e);
    });
  }
  init();
})();
