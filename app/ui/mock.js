// Mock of the PLAN section 3.7 command/event contract. Active only with ?mock=1. Synthetic data only.
// Extra knob: ?signin=missing_scope|timeout makes the first add_account attempt fail that way.
(function () {
  "use strict";
  var params = new URLSearchParams(window.location.search);
  if (params.get("mock") !== "1") return;

  var NOW = Date.now();
  var H = 3600 * 1000;
  var listeners = {};
  function emit(name, payload) {
    (listeners[name] || []).slice().forEach(function (cb) { cb({ event: name, payload: payload }); });
  }
  function delay(v, ms) { return new Promise(function (r) { setTimeout(function () { r(v); }, ms || 40); }); }
  function fail(code, message, detail) {
    var e = { code: code, message: message };
    if (detail) e.detail = detail;
    return Promise.reject(e);
  }

  var accounts = [
    { id: "a1", email: "alice@example.com", state: "signedIn", inboxUnread: 3, spamUnread: 2 },
    { id: "a2", email: "bob@example.org", state: "needsReauth", inboxUnread: 1, spamUnread: null },
  ];
  var status = {
    a1: { accountId: "a1", state: "ok", asOf: NOW - 20000 },
    a2: { accountId: "a2", state: "needsReauth", asOf: NOW - 3 * 24 * H },
  };

  function folderTree() {
    function f(id, name, kind, depth, unread, clickable) {
      return { id: id, name: name, kind: kind, depth: depth, unread: unread, clickable: clickable };
    }
    return [
      f("INBOX", "Inbox", "inbox", 0, 3, true),
      f("STARRED", "Starred", "starred", 0, null, true),
      f("SENT", "Sent", "sent", 0, null, true),
      f("DRAFT", "Drafts", "drafts", 0, null, true),
      f("ALL", "All Mail", "allMail", 0, null, true),
      f("SPAM", "Spam", "spam", 0, 2, true),
      f("TRASH", "Trash", "trash", 0, null, true),
      f("group:categories", "Categories", "categoryGroup", 0, null, false),
      f("CATEGORY_PERSONAL", "Personal", "category", 1, null, true),
      f("CATEGORY_UPDATES", "Updates", "category", 1, 4, true),
      f("CATEGORY_PROMOTIONS", "Promotions", "category", 1, null, true),
      f("CATEGORY_SOCIAL", "Social", "category", 1, null, true),
      f("CATEGORY_FORUMS", "Forums", "category", 1, null, true),
      f("Label_1", "Receipts", "user", 0, null, true),
      f("group:Work", "Work", "group", 0, null, false),
      f("Label_2", "Projects", "user", 1, 1, true),
      f("group:Work/Projects", "Archive", "group", 1, null, false),
      f("Label_3", "2024", "user", 2, null, true),
    ];
  }

  var SENDERS = [
    ["Example Bank", "alerts@bank.example"], ["", "no-reply@service.example"], ["Ada Tester", "ada@example.net"],
    ["Shipping Co", "track@ship.example"], ["Dev Newsletter", "news@dev.example"], ["Grace Sample", "grace@example.com"],
  ];
  var SUBJECTS = [
    "Your verification code is 482913", "Weekly digest", "Meeting notes", "Your package has shipped",
    "Security alert: new sign-in", "Invoice 2041 attached", "Lunch on Thursday?", "Welcome aboard",
  ];

  function rows(accountId, folderId) {
    var seed = (accountId + folderId).length;
    var out = [];
    for (var i = 0; i < 30; i++) {
      var s = SENDERS[(i + seed) % SENDERS.length];
      var subj = SUBJECTS[(i + seed) % SUBJECTS.length];
      var spam = folderId === "SPAM";
      out.push({
        id: "m-" + folderId + "-" + i,
        internalDate: NOW - (i * 3 + 0.2) * H * (i < 4 ? 0.3 : 2),
        fromName: s[0], fromAddr: s[1],
        subject: spam && i < 2 ? "Your sign-in code is " + (100000 + i * 7919) : subj,
        snippet: "This is synthetic preview text for message " + i + ". It is long enough to be truncated in the list view.",
        unread: spam ? i < 2 : i < 3 || (i === 7 && folderId === "INBOX"),
        starred: i === 1 || i === 5,
        important: false,
        attachmentHint: i === 0 || i % 6 === 2,
        size: 4000 + i * 100,
      });
    }
    return out;
  }

  function messageView(accountId, id, showImages) {
    var first = /-0$/.test(id);
    var blocked = first ? 3 : 0;
    var page = "<!doctype html><html><body style=\"background:#fff;color:#111;font-family:sans-serif\">" +
      "<h3>Synthetic message</h3><p>This is a plain white mock page for " + id.replace(/[^A-Za-z0-9_-]/g, "") + ".</p>" +
      (first ? "<p>Your code: <b>482913</b></p>" : "") + "</body></html>";
    var m = {
      id: id,
      subject: first ? "Your verification code is 482913" : "Mock message " + id,
      from: { name: "Example Bank", addr: "alerts@bank.example" },
      to: [{ name: "Alice", addr: "alice@example.com" }],
      cc: first ? [{ name: null, addr: "carol@example.net" }, { name: "Dan", addr: "dan@example.net" }] : [],
      replyTo: first ? { name: null, addr: "support@bank-replies.example" } : null,
      date: NOW - 2 * H,
      labels: ["Inbox", "Updates"],
      auth: first
        ? { line: "SPF pass · DKIM pass (bank.example) · DMARC pass", level: "good", mailedBy: "bank.example", signedBy: "bank.example" }
        : /-1$/.test(id)
          ? { line: "SPF pass · DKIM fail · DMARC fail", level: "bad", mailedBy: null, signedBy: null }
          : /-2$/.test(id)
            ? { line: "SPF pass · DKIM none · DMARC none", level: "mixed", mailedBy: null, signedBy: null }
            : { line: "", level: "none", mailedBy: null, signedBy: null },
      attachments: first
        ? [{ partId: 2, name: "statement.pdf", mime: "application/pdf", size: 184320 },
           { partId: 3, name: "photo.jpg", mime: "image/jpeg", size: 2411000 }]
        : [],
      page: page,
      remoteImagesBlocked: showImages ? 0 : blocked,
      imagesShown: !!showImages,
      cached: false,
    };
    return m;
  }

  var pending = null; // {resolve, reject}
  var signInAttempts = 0;

  var handlers = {
    accounts: function () { return delay(accounts.slice()); },
    add_account: function () {
      if (pending) return fail("auth", "a sign-in is already in progress", "already_pending");
      signInAttempts++;
      return new Promise(function (resolve, reject) {
        var me = { reject: reject };
        pending = me;
        emit("sign-in-progress", { stage: "browser" });
        setTimeout(function () { if (pending === me) emit("sign-in-progress", { stage: "waiting" }); }, 400);
        setTimeout(function () {
          if (pending !== me) return;
          pending = null;
          var mode = params.get("signin");
          if (signInAttempts === 1 && mode === "missing_scope") {
            emit("sign-in-progress", { stage: "error", message: "Gmail access was not granted" });
            reject({ code: "auth", message: "Gmail access was not granted", detail: "missing_scope" });
          } else if (signInAttempts === 1 && mode === "timeout") {
            reject({ code: "auth", message: "sign-in timed out", detail: "timeout" });
          } else {
            var acc = { id: "a" + (accounts.length + 1), email: "new" + accounts.length + "@example.com", state: "signedIn", inboxUnread: 0, spamUnread: 0 };
            accounts.push(acc);
            status[acc.id] = { accountId: acc.id, state: "ok", asOf: Date.now() };
            emit("sign-in-progress", { stage: "done" });
            resolve(acc);
          }
        }, 2500);
      });
    },
    reopen_sign_in: function () { return delay(null); },
    cancel_sign_in: function () {
      if (pending) { var p = pending; pending = null; p.reject({ code: "auth", message: "sign-in cancelled", detail: "cancelled" }); }
      return delay(null);
    },
    reauth: function (a) {
      var acc = accounts.filter(function (x) { return x.id === a.accountId; })[0];
      return handlers.add_account({}).then(function (n) {
        accounts.pop(); // reuse the existing account instead of adding one
        delete status[n.id];
        acc.state = "signedIn";
        status[acc.id] = { accountId: acc.id, state: "ok", asOf: Date.now() };
        emit("account-changed", acc);
        emit("sync-status", status[acc.id]);
        return acc;
      });
    },
    sign_out: function (a) {
      accounts = accounts.filter(function (x) { return x.id !== a.accountId; });
      return delay({ revoked: a.accountId !== "a2", manualRemovalNeeded: a.accountId === "a2" });
    },
    folders: function () { return delay(folderTree()); },
    messages: function (a) {
      var all = rows(a.accountId, a.folderId);
      var offline = a.accountId === "a2";
      var n = a.more ? 24 : 12;
      return delay({
        rows: all.slice(0, n),
        more: !offline && n < 24,
        endOfCache: offline || n >= 24,
      }, 80);
    },
    message: function (a) {
      if (!/^m-/.test(a.messageId)) return fail("not_found", "message not found");
      return delay(messageView(a.accountId, a.messageId, a.showImages), 80);
    },
    refresh: function (a) {
      status[a.accountId] = { accountId: a.accountId, state: "syncing", asOf: (status[a.accountId] || {}).asOf || null };
      emit("sync-status", status[a.accountId]);
      setTimeout(function () {
        var s = { accountId: a.accountId, state: a.accountId === "a2" ? "needsReauth" : "ok", asOf: Date.now() };
        status[a.accountId] = s;
        emit("sync-status", s);
        emit("mail-changed", { accountId: a.accountId, labels: ["INBOX"] });
      }, 1200);
      return delay(null);
    },
    set_view: function () { return delay(null, 5); },
    save_attachment: function (a) {
      // The mock pretends the user cancels the save dialog for partId 3.
      if (a.partId === 3) return delay({ saved: false, fileName: null });
      return delay({ saved: true, fileName: "statement.pdf" });
    },
    status: function () { return delay(Object.keys(status).map(function (k) { return status[k]; })); },
  };

  window.__TAURI__ = {
    core: {
      invoke: function (cmd, args) {
        var h = handlers[cmd];
        if (!h) return fail("internal", "unknown command " + cmd);
        return h(args || {});
      },
    },
    event: {
      listen: function (name, cb) {
        (listeners[name] = listeners[name] || []).push(cb);
        return Promise.resolve(function () {
          listeners[name] = (listeners[name] || []).filter(function (x) { return x !== cb; });
        });
      },
    },
  };
  window.__MOCK__ = true;
})();
