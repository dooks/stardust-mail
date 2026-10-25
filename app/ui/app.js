// Placeholder (WP-G replaces this). Calls the `status` command to prove the IPC bridge is wired.
(function () {
  "use strict";
  var el = document.getElementById("status");
  var invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
  if (!invoke) {
    el.textContent = "Placeholder UI (no Tauri bridge).";
    return;
  }
  invoke("status").then(
    function () { el.textContent = "Backend reachable."; },
    function (e) { el.textContent = "Backend stub: " + (e && e.message ? e.message : "error"); }
  );
})();
