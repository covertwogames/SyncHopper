// SyncHopper screens. Plain JavaScript, no build step.
// The Rust side sends one "state" object with everything to show, and the
// screens call named commands to make things happen.
//
// The state holds what is true for the whole app plus a list of accounts.
// The screens show one account at a time: `S` is the state with that
// account's part laid over it, so S.pairs, S.settings, S.drive and so on
// are always the shown account's.

(function () {
  "use strict";

  const T = window.__TAURI__;
  // Commands that are about one account. The account being shown, or being
  // set up, is filled in for them.
  const PER_ACCOUNT = new Set([
    "dismiss_folder_notice", "ignore_file", "unignore_file", "reconnect", "remove_account", "rename_account", "set_account_color",
    "default_base_folder", "list_remote", "make_remote_dir", "preview_pair", "finish_setup", "update_settings",
    "restore_defaults", "add_pair", "remove_pair", "pair_action", "sync_all", "set_paused_all", "upload_add",
    "upload_action", "set_upload_dest", "drive_action", "open_target",
  ]);
  const invoke = (cmd, args) => {
    const a = Object.assign({}, args || {});
    if (PER_ACCOUNT.has(cmd) && a.account === undefined) a.account = activeAccount();
    return T.core.invoke(cmd, a);
  };
  const $ = (sel, root) => (root || document).querySelector(sel);
  const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));

  let RAW = null; // latest state from the app: general things and every account
  let S = null; // RAW with the shown account's part laid over it
  let cur = null; // id of the account being shown
  let view = "loading";
  let wiz = null; // setup wizard working values
  let review = null; // open review dialog: { id, kind, modal }
  const VIEWS = ["loading", "wizard", "main", "upload", "settings", "general", "add"];
  // The screens that have the accounts column beside them.
  const WITH_COLUMN = ["main", "upload", "settings", "general", "add"];

  // ----- accounts -----------------------------------------------------------

  // The accounts whose setup is finished. One that has signed in but is
  // still in setup is not shown anywhere but in the setup screens.
  function readyAccounts() {
    return RAW ? RAW.accounts.filter((a) => a.setup_complete) : [];
  }

  function activeAccount() {
    return wiz && wiz.account ? wiz.account : cur;
  }

  function providerOf(id) {
    return (RAW.providers || []).find((p) => p.id === id) || {};
  }

  // What the screens see while there is no account to show yet: during the
  // first setup, before signing in.
  function blankAccount() {
    const p = wiz && wiz.provider ? providerOf(wiz.provider) : {};
    return {
      id: null,
      provider: p.id || null,
      cloud: p.name || "",
      name: p.name || "",
      setup_complete: false,
      account: { email: "", used: null, total: null },
      signing_in: null,
      settings: Object.assign({ feature_sync: true, feature_drive: false, drive_letter: p.letter || "K", feature_sidebar: true, upload_dest: "", base_folder: "" }, RAW.general),
      sync: { paused_all: false, auto_pause: null },
      pairs: [],
      drive: { enabled: false, supported: RAW.windows, state: "off", letter: p.letter || "K", message: "", free_letters: [] },
      uploads: [],
      folder_notices: [],
      add_request: null,
    };
  }

  // Works out which account is shown and lays it over the general state.
  function compose() {
    const ready = readyAccounts();
    if (!ready.some((a) => a.id === cur)) cur = ready.length ? ready[0].id : null;
    let acct = null;
    if (wiz) acct = wiz.account ? RAW.accounts.find((a) => a.id === wiz.account) : null;
    else acct = ready.find((a) => a.id === cur);
    S = Object.assign({}, RAW, acct || blankAccount());
  }

  // Asks the app for fresh state right now, rather than waiting for it.
  async function refresh() {
    const fresh = await invoke("get_state").catch(() => null);
    if (fresh) {
      RAW = fresh;
      compose();
    }
  }

  // The name of the service the shown account is with: Koofr, pCloud, Filen.
  function C() {
    return S.cloud || "your account";
  }

  // ----- small helpers ----------------------------------------------------

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function num(n) {
    return Number(n || 0).toLocaleString("en-US");
  }

  function plural(n, one, many) {
    return num(n) + " " + (n === 1 ? one : many || one + "s");
  }

  function fmtBytes(bytes) {
    const units = ["bytes", "KB", "MB", "GB", "TB"];
    let v = Number(bytes || 0);
    let i = 0;
    while (v >= 1000 && i < units.length - 1) {
      v /= 1000;
      i += 1;
    }
    const text = v >= 100 || i === 0 ? Math.round(v).toString() : v.toFixed(1).replace(/\.0$/, "");
    return text + " " + units[i];
  }

  function fmtSpeed(bytesPerSecond) {
    return fmtBytes(bytesPerSecond) + " per second";
  }

  function fmtEta(seconds) {
    if (seconds == null) return "";
    if (seconds < 60) return "less than a minute left";
    if (seconds < 3600) return "about " + plural(Math.round(seconds / 60), "minute") + " left";
    return "about " + plural(Math.round(seconds / 3600), "hour") + " left";
  }

  function ago(ts) {
    if (!ts) return "Not synced yet";
    const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts);
    if (secs < 60) return "Last synced just now";
    if (secs < 3600) return "Last synced " + plural(Math.floor(secs / 60), "minute") + " ago";
    if (secs < 86400) return "Last synced " + plural(Math.floor(secs / 3600), "hour") + " ago";
    return "Last synced " + plural(Math.floor(secs / 86400), "day") + " ago";
  }

  function lastPart(path) {
    const parts = String(path || "").split(/[\\/]/).filter(Boolean);
    return parts.length ? parts[parts.length - 1] : "";
  }

  function joinLocal(base, name) {
    const sep = base.includes("\\") || /^[A-Za-z]:/.test(base) ? "\\" : "/";
    return base.replace(/[\\/]+$/, "") + sep + name.replace(/\//g, sep);
  }

  // ----- icons and small page parts ---------------------------------------

  // Line icons on a 20 by 20 grid. They ship with the app, so they look the
  // same on every system.
  const ICONS = {
    folder: '<path d="M3 6.5A1.5 1.5 0 0 1 4.5 5h3.4l1.6 1.8h6A1.5 1.5 0 0 1 17 8.3v6.2a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 3 14.5z"/>',
    drive: '<rect x="3" y="6" width="14" height="8" rx="1.5"/><path d="M6 11.3h.1M9 11.3h5"/>',
    ok: '<circle cx="10" cy="10" r="7"/><path d="M6.8 10.2l2.2 2.2 4.2-4.6"/>',
    sync: '<path d="M4.5 9a5.6 5.6 0 0 1 10-2.6M15.5 11a5.6 5.6 0 0 1-10 2.6M14.8 3.6v3h-3M5.2 16.4v-3h3"/>',
    warn: '<path d="M10 3.5l7 12.5H3zM10 8.5v3.5M10 14.2v.1"/>',
    clock: '<circle cx="10" cy="10" r="7"/><path d="M10 6.2V10l2.6 1.6"/>',
    pause: '<circle cx="10" cy="10" r="7"/><path d="M8.3 7.6v4.8M11.7 7.6v4.8"/>',
    off: '<circle cx="10" cy="10" r="7"/><path d="M6.8 10h6.4"/>',
    info: '<circle cx="10" cy="10" r="7"/><path d="M10 9.4v4.1M10 6.6v.1"/>',
    more: '<circle cx="5" cy="10" r="1" fill="currentColor"/><circle cx="10" cy="10" r="1" fill="currentColor"/><circle cx="15" cy="10" r="1" fill="currentColor"/>',
    upload: '<path d="M10 13V4.5M6.5 8L10 4.5 13.5 8M4.5 15.5h11"/>',
    plus: '<path d="M10 4.5v11M4.5 10h11"/>',
    gear: '<path d="M8.63 4.47L8.87 2.48L11.13 2.48L11.37 4.47L12.94 5.12L14.52 3.89L16.11 5.48L14.88 7.06L15.53 8.63L17.52 8.87L17.52 11.13L15.53 11.37L14.88 12.94L16.11 14.52L14.52 16.11L12.94 14.88L11.37 15.53L11.13 17.52L8.87 17.52L8.63 15.53L7.06 14.88L5.48 16.11L3.89 14.52L5.12 12.94L4.47 11.37L2.48 11.13L2.48 8.87L4.47 8.63L5.12 7.06L3.89 5.48L5.48 3.89L7.06 5.12z"/><circle cx="10" cy="10" r="2.5"/>',
    back: '<path d="M16 10H4.5M9 5.5L4.5 10 9 14.5"/>',
    chevron: '<path d="M8 5.5l4.5 4.5L8 14.5"/>',
    cloud: '<path d="M6.3 15.2a3.3 3.3 0 0 1-.6-6.55 4.4 4.4 0 0 1 8.5-.5 3.6 3.6 0 0 1 .3 7.05z"/>',
  };

  function icon(name, size) {
    const s = size || 20;
    return (
      '<svg class="ico" aria-hidden="true" focusable="false" width="' + s + '" height="' + s +
      '" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      ICONS[name] +
      "</svg>"
    );
  }

  // A notice bar. `html` goes into the page as it is, so escape text first.
  function banner(tone, html, buttons) {
    return '<div class="banner' + (tone === "warn" ? " warn" : "") + '">' + icon(tone === "warn" ? "warn" : "info") + "<span>" + html + "</span>" + (buttons || "") + "</div>";
  }

  // An icon with one line of text beside it. tone: ok, warn, busy or quiet.
  function statusLine(tone, name, text) {
    return '<span class="status ' + tone + '">' + icon(name, 18) + '<span class="status-text">' + esc(text) + "</span></span>";
  }

  let toastTimer = null;
  function toast(message, ms) {
    const el = $("#toast");
    el.textContent = message;
    el.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      el.hidden = true;
    }, ms || 5000);
  }

  // Runs a command and shows a message if it fails. Returns undefined on failure.
  async function run(cmd, args) {
    try {
      return await invoke(cmd, args);
    } catch (e) {
      toast(String(e), 8000);
      return undefined;
    }
  }

  // ----- dialogs ------------------------------------------------------------

  const openModals = [];

  // buttons: [{ label, kind: "primary" | "danger" | "", left, close: true, action }]
  function openModal(opts) {
    const overlay = document.createElement("div");
    overlay.className = "overlay";
    const titleId = "dlg-" + Math.random().toString(36).slice(2);
    overlay.innerHTML =
      '<div class="dialog" role="dialog" aria-modal="true" aria-labelledby="' + titleId + '">' +
      '<h1 id="' + titleId + '"></h1>' +
      '<div class="dialog-body"></div>' +
      '<p class="err" role="alert"></p>' +
      '<div class="dialog-foot"></div>' +
      "</div>";
    const dialog = $(".dialog", overlay);
    if (opts.width) dialog.style.width = opts.width + "px";
    $("h1", overlay).textContent = opts.title;
    const body = $(".dialog-body", overlay);
    const foot = $(".dialog-foot", overlay);
    const errEl = $(".err", overlay);
    const returnFocus = document.activeElement;
    let closed = false;

    const modal = {
      el: overlay,
      body: body,
      setBody(html) {
        if (body.dataset.html !== html) {
          body.dataset.html = html;
          body.innerHTML = html;
        }
      },
      setTitle(text) {
        $("h1", overlay).textContent = text;
      },
      setError(text) {
        errEl.textContent = text || "";
      },
      close(result) {
        if (closed) return;
        closed = true;
        overlay.remove();
        const i = openModals.indexOf(modal);
        if (i >= 0) openModals.splice(i, 1);
        if (returnFocus && document.contains(returnFocus)) returnFocus.focus();
        if (opts.onClose) opts.onClose(result);
      },
      cancel() {
        modal.close(undefined);
      },
    };

    (opts.buttons || []).forEach((b) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "btn" + (b.kind ? " " + b.kind : "") + (b.left ? " left" : "");
      btn.textContent = b.label;
      if (b.id) btn.dataset.id = b.id;
      btn.addEventListener("click", async () => {
        if (btn.disabled) return;
        if (b.action) {
          btn.disabled = true;
          let keepOpen = false;
          try {
            keepOpen = (await b.action(modal, btn)) === false;
          } catch (e) {
            modal.setError(String(e));
            keepOpen = true;
          }
          btn.disabled = false;
          if (keepOpen) return;
        }
        if (b.close !== false) modal.close(b.result);
      });
      foot.appendChild(btn);
    });

    modal.setBody(opts.body || "");
    $("#modal-root").appendChild(overlay);
    openModals.push(modal);
    const first = $("input, textarea, select", body) || $(".btn.primary", foot) || $(".btn", foot);
    if (first) first.focus();
    return modal;
  }

  function confirmDialog(title, text, okLabel, danger) {
    return new Promise((resolve) => {
      openModal({
        title: title,
        body: "<p>" + esc(text) + "</p>",
        buttons: [
          { label: "Cancel", result: false },
          { label: okLabel, kind: danger ? "danger" : "primary", result: true },
        ],
        onClose: (result) => resolve(result === true),
      });
    });
  }

  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (!$("#row-menu").hidden) {
      closeRowMenu();
    } else if (openModals.length) {
      openModals[openModals.length - 1].cancel();
    }
  });

  // A dialog for walking through the account's folders and choosing one.
  function browseCloud(startPath, title) {
    return new Promise((resolve) => {
      let path = String(startPath || "").split("/").filter(Boolean);
      let token = 0;
      const modal = openModal({
        title: title || "Choose a " + C() + " folder",
        body:
          '<div class="crumbs" id="br-crumbs"></div>' +
          '<div class="browser-list"><ul class="list pick" id="br-list"></ul><p class="small muted list-note" id="br-note"></p></div>' +
          '<div class="row gap8"><input type="text" id="br-new" class="grow" placeholder="Name for a new folder here" aria-label="Name for a new folder here" autocomplete="off" spellcheck="false">' +
          '<button type="button" class="btn" id="br-make">Create folder</button></div>',
        buttons: [
          { label: "Cancel", result: null },
          { label: "Choose this folder", kind: "primary", action: () => resolve(path.join("/")), result: "chosen" },
        ],
        onClose: (result) => {
          if (result !== "chosen") resolve(null);
        },
      });

      async function load() {
        const mine = ++token;
        const crumbs = $("#br-crumbs", modal.el);
        const parts = [{ label: C(), depth: 0 }].concat(path.map((p, i) => ({ label: p, depth: i + 1 })));
        crumbs.innerHTML = parts
          .map((p, i) =>
            i === parts.length - 1
              ? '<span class="strong">' + esc(p.label) + "</span>"
              : '<button type="button" class="link" data-depth="' + p.depth + '">' + esc(p.label) + '</button><span class="sep">/</span>'
          )
          .join("");
        const list = $("#br-list", modal.el);
        const note = $("#br-note", modal.el);
        list.innerHTML = "";
        note.textContent = "Loading folders";
        modal.setError("");
        try {
          const names = await invoke("list_remote", { path: path.join("/") });
          if (mine !== token) return;
          note.textContent = names.length ? "" : "No folders inside this one.";
          note.hidden = names.length > 0;
          list.innerHTML = names
            .map((n) => '<li tabindex="0" role="button" data-name="' + esc(n) + '">' + icon("folder", 18) + '<span class="name">' + esc(n) + "</span>" + icon("chevron", 16) + "</li>")
            .join("");
        } catch (e) {
          if (mine !== token) return;
          note.hidden = false;
          note.textContent = "";
          modal.setError(String(e));
        }
      }

      modal.el.addEventListener("click", (e) => {
        const crumb = e.target.closest("[data-depth]");
        const item = e.target.closest("[data-name]");
        if (crumb) {
          path = path.slice(0, Number(crumb.dataset.depth));
          load();
        } else if (item) {
          path.push(item.dataset.name);
          load();
        }
      });
      modal.el.addEventListener("keydown", (e) => {
        const item = e.target.closest && e.target.closest("[data-name]");
        if (item && (e.key === "Enter" || e.key === " ")) {
          e.preventDefault();
          path.push(item.dataset.name);
          load();
        }
      });
      $("#br-make", modal.el).addEventListener("click", async () => {
        const input = $("#br-new", modal.el);
        const name = input.value.trim().replace(/[\\/]/g, "");
        if (!name) {
          modal.setError("Type a name for the new folder.");
          return;
        }
        try {
          await invoke("make_remote_dir", { path: path.concat([name]).join("/") });
          input.value = "";
          path.push(name);
          load();
        } catch (e) {
          modal.setError(String(e));
        }
      });
      load();
    });
  }

  // ----- moving between screens -----------------------------------------

  function go(name) {
    view = name;
    VIEWS.forEach((v) => {
      $("#view-" + v).hidden = v !== name;
    });
    $("#accounts").hidden = !WITH_COLUMN.includes(name);
    closeRowMenu();
    if (name === "upload" && S) $("#up-dest").value = S.settings.upload_dest || "";
    if (name === "settings" || name === "general") syncControls(true);
    render();
  }

  function render() {
    if (!S) return;
    renderAccounts();
    if (view === "main") renderMain();
    else if (view === "upload") renderUpload();
    else if (view === "settings" || view === "general") syncControls(false);
    else if (view === "add") renderAdd();
    else if (view === "wizard") wizardTick();
    updateReview();
    updateReconnect();
  }

  function onState(state) {
    const shown = cur;
    RAW = state;
    compose();
    const any = readyAccounts().length > 0;
    if (view === "loading") {
      if (any) {
        go("main");
        takeRequests();
      } else startWizard(null);
      return;
    }
    if (!any && view !== "wizard") {
      // The last account was disconnected: setup starts over.
      openModals.slice().forEach((m) => m.cancel());
      startWizard(null);
      return;
    }
    if (shown && cur !== shown && view !== "wizard") {
      // The account that was showing is gone. Its screens go with it.
      openModals.slice().forEach((m) => m.cancel());
      go("main");
    } else render();
    takeRequests();
  }

  // Shows another account.
  function showAccount(id) {
    if (!readyAccounts().some((a) => a.id === id)) return;
    cur = id;
    compose();
    go("main");
  }

  function takeRequests() {
    takeAddRequest();
    takePending();
  }

  // The <option>s for choosing among the accounts.
  function accountOptions(selected, firstLabel) {
    return (
      (firstLabel ? '<option value="">' + esc(firstLabel) + "</option>" : "") +
      readyAccounts().map((a) => '<option value="' + esc(a.id) + '"' + (a.id === selected ? " selected" : "") + ">" + esc(a.name) + "</option>").join("")
    );
  }

  // A folder sent here from outside the window, such as with "Sync folder to
  // Koofr" in File Explorer. Shown once, then handed back. It arrives on one
  // account; with more than one, the dialog lets the user move it.
  let takingRequest = false;
  function takeAddRequest() {
    if (takingRequest || view === "wizard" || !RAW) return;
    const holder = readyAccounts().find((a) => a.add_request);
    if (!holder) return;
    const req = holder.add_request;
    // An update can arrive before the request is handed back; don't open
    // the same dialog twice.
    takingRequest = true;
    run("clear_add_request").finally(() => (takingRequest = false));
    if (holder.id !== cur) {
      cur = holder.id;
      compose();
    }
    if (view !== "main") go("main");
    else render();
    const moveTo = (id) => run("request_add_folder", { account: id, folder: req.local });
    if (req.blocked) {
      const others = readyAccounts().length > 1;
      const modal = openModal({
        title: "Sync folder to " + S.name,
        body:
          "<p>" + esc(req.blocked) + "</p>" +
          (others ? '<div class="field"><label for="bl-acct">Sync it with another account</label><select id="bl-acct">' + accountOptions(null, "Choose an account") + "</select></div>" : ""),
        buttons: [{ label: "OK", kind: "primary" }],
      });
      const pick = $("#bl-acct", modal.el);
      if (pick)
        pick.addEventListener("change", () => {
          if (!pick.value || pick.value === cur) return;
          modal.close();
          moveTo(pick.value);
        });
      return;
    }
    addFolderDialog(req, moveTo);
  }

  // Files sent here with the right-click menu while there is more than one
  // account: ask which account they are for.
  let pendingModal = null;
  function takePending() {
    const p = RAW && RAW.pending;
    if (!p || p.kind !== "upload" || view === "wizard") return;
    const count = p.paths.length;
    const body =
      "<p>" + esc(plural(count, "item")) + " from File Explorer. Which account " + (count === 1 ? "is it" : "are they") + " for?</p>" +
      '<ul class="box list pick">' +
      readyAccounts().map((a) => '<li tabindex="0" role="button" data-id="' + esc(a.id) + '">' + accountIcon(a, 18) + '<span class="name">' + esc(a.name) + "</span></li>").join("") +
      "</ul>";
    if (pendingModal) {
      pendingModal.setBody(body);
      return;
    }
    const modal = openModal({
      title: "Send to which account?",
      body: body,
      buttons: [{ label: "Cancel" }],
      onClose: () => {
        pendingModal = null;
        run("clear_pending");
      },
    });
    pendingModal = modal;
    const pick = (e) => {
      const li = e.target.closest("li[data-id]");
      if (!li || (e.type === "keydown" && e.key !== "Enter" && e.key !== " ")) return;
      const acct = readyAccounts().find((a) => a.id === li.dataset.id);
      const paths = ((RAW.pending || {}).paths || []).slice();
      modal.close();
      if (!acct) return;
      cur = acct.id;
      compose();
      go("upload");
      run("upload_add", { account: acct.id, paths: paths, dest: acct.settings.upload_dest || "" });
    };
    modal.el.addEventListener("click", pick);
    modal.el.addEventListener("keydown", pick);
  }

  document.addEventListener("click", (e) => {
    const nav = e.target.closest("[data-nav]");
    if (nav) go(nav.dataset.nav);
  });

  // ----- the accounts column ------------------------------------------------

  // One line on how an account is doing, and whether it needs a mark.
  function accountSummary(a) {
    if (RAW.engine.state === "error" || RAW.engine.state === "blocked") return { text: "Can't sync right now", flag: "warn" };
    const pairs = a.settings.feature_sync ? a.pairs : [];
    const working = (p) => p.phase === "syncing" || p.phase === "checking" || p.phase === "queued";
    const needs = pairs.filter((p) => !p.paused && ((p.approval && !working(p)) || p.phase === "error" || (p.files || []).some((f) => f.flagged) || p.conflicts.length)).length;
    if (needs) return { text: plural(needs, "folder needs", "folders need") + " you", flag: "warn" };
    if (a.drive.enabled && (a.drive.state === "error" || a.drive.state === "needs_driver")) return { text: "Drive needs attention", flag: "warn" };
    if (pairs.some((p) => !p.paused && p.phase === "waiting")) return { text: "Waiting", flag: null };
    const busy = pairs.filter((p) => !p.paused && working(p)).length;
    if (busy) return { text: "Syncing " + plural(busy, "folder"), flag: "busy" };
    if ((a.uploads || []).some((u) => u.state === "waiting" || u.state === "uploading")) return { text: "Uploading", flag: "busy" };
    if (a.sync.paused_all || a.sync.auto_pause) return { text: "Paused", flag: null };
    if (!a.settings.feature_sync) return { text: a.drive.enabled ? "Drive only" : "Uploads only", flag: null };
    if (!pairs.length) return { text: "No folders yet", flag: null };
    if (pairs.every((p) => p.paused)) return { text: "Paused", flag: null };
    return { text: "Up to date", flag: null };
  }

  function renderAccounts() {
    if (!RAW) return;
    const onAccount = view === "main" || view === "upload" || view === "settings";
    const html = readyAccounts()
      .map((a) => {
        const sum = accountSummary(a);
        return (
          '<button type="button" class="acct' + (onAccount && a.id === cur ? " current" : "") + '" data-acct="' + esc(a.id) + '"' + (onAccount && a.id === cur ? ' aria-current="true"' : "") + ">" +
          accountIcon(a) +
          '<span class="who"><span class="name">' + esc(a.name) + '</span><span class="state">' + esc(sum.text) + "</span></span>" +
          (sum.flag ? '<span class="flag ' + sum.flag + '">' + icon(sum.flag === "warn" ? "warn" : "sync", 16) + "</span>" : "") +
          "</button>"
        );
      })
      .join("");
    setHtml($("#acct-list"), html);
    $("#acct-add").classList.toggle("current", view === "add");
    $("#acct-general").classList.toggle("current", view === "general");
  }

  $("#acct-list").addEventListener("click", (e) => {
    const b = e.target.closest("[data-acct]");
    if (b) showAccount(b.dataset.acct);
  });

  // ----- add cloud provider -------------------------------------------------

  const SIGN_IN_HINT = {
    password: "Email and an app password",
    browser: "Sign in through your browser",
    keys: "Email and password",
  };

  // An account's icon: the SyncHopper mark on the account's color.
  function accountIcon(a, size) {
    const ok = (RAW.colors || []).some((c) => c.id === a.color);
    const px = size || 20;
    return '<img class="acct-ico" src="accounts/' + (ok ? a.color : "teal") + '.png" width="' + px + '" height="' + px + '" alt="">';
  }

  // The rows of the list of services to choose from.
  function providerRows() {
    return (RAW.providers || [])
      .map(
        (p) =>
          '<li tabindex="0" role="button" data-provider="' + esc(p.id) + '">' + icon("cloud") +
          '<div class="frow-main"><span class="name">' + esc(p.name) + '</span><span class="path">' + esc(SIGN_IN_HINT[p.sign_in] || "") + "</span></div>" +
          '<span class="go">' + icon("chevron", 16) + "</span></li>"
      )
      .join("");
  }

  function renderAdd() {
    setHtml($("#add-list"), providerRows());
  }

  // Calls `chosen` with the service's id when a row in `root` is picked.
  function onProviderPick(root, chosen) {
    const pick = (e) => {
      const li = e.target.closest("li[data-provider]");
      if (!li || (e.type === "keydown" && e.key !== "Enter" && e.key !== " ")) return;
      e.preventDefault();
      chosen(li.dataset.provider);
    };
    root.addEventListener("click", pick);
    root.addEventListener("keydown", pick);
  }
  onProviderPick($("#add-list"), (id) => startWizard(id));

  // ----- main screen ------------------------------------------------------

  function engineText() {
    const e = S.engine;
    if (e.state === "ready") return "";
    if (e.state === "downloading") return "Downloading the sync engine (rclone), " + e.percent + "% done.";
    if (e.state === "error") return e.message || "The sync engine could not be set up.";
    if (e.state === "blocked") return e.message || "This PC blocked SyncHopper's sync engine, so nothing can sync right now.";
    return "Getting the sync engine ready.";
  }

  // The engine can't be used right now, and Try again makes sense.
  function engineStuck() {
    return S.engine.state === "error" || S.engine.state === "blocked";
  }

  function pairStatus(p) {
    // tone: ok, warn, busy or quiet. icon: one of ICONS.
    const st = { tone: "quiet", icon: "clock", label: "", sub: "", bar: null, btn: "Sync now", act: "sync" };
    const hold = S.sync.paused_all ? "All syncing is paused" : S.sync.auto_pause === "battery" ? "Paused while on battery" : S.sync.auto_pause === "metered" ? "Paused on a metered connection" : "";
    const busy = p.phase === "syncing" || p.phase === "checking" || p.phase === "queued";
    if (p.paused) {
      Object.assign(st, { icon: "pause", label: "Paused", sub: "This folder is not syncing", btn: "Resume", act: "resume" });
    } else if (p.approval && !busy) {
      const where = p.approval.side === "pc" ? "on this PC" : "in " + C();
      Object.assign(st, {
        tone: "warn",
        icon: "warn",
        label: "Needs your OK",
        sub: p.approval.changed
          ? "Every file looks changed since the last sync"
          : p.approval.everything
            ? "Everything here is gone " + where
            : num(p.approval.deleted) + " of " + num(p.approval.total) + " files were deleted " + where,
        btn: "Review",
        act: "review",
      });
    } else if (p.phase === "syncing" && p.progress) {
      const g = p.progress;
      const speed = g.speed > 0 ? fmtSpeed(g.speed) : "";
      const eta = fmtEta(g.eta);
      Object.assign(st, {
        tone: "busy",
        icon: "sync",
        label: g.total > 0 ? "Syncing " + num(Math.min(g.done + 1, g.total)) + " of " + plural(g.total, "file") : "Syncing",
        sub: [speed, eta].filter(Boolean).join(", ") || "Working",
        bar: g.percent,
        btn: "Pause",
        act: "pause",
      });
    } else if (busy) {
      Object.assign(st, {
        tone: p.phase === "queued" ? "quiet" : "busy",
        icon: p.phase === "queued" ? "clock" : "sync",
        label: p.phase === "queued" ? "Waiting to sync" : "Checking for changes",
        sub: p.phase === "queued" ? "Another folder is syncing first" : "Comparing this PC with " + C(),
        btn: "Pause",
        act: "pause",
      });
    } else if (p.phase === "waiting") {
      Object.assign(st, {
        label: p.waiting_kind === "drive" ? "Waiting for the drive" : "Waiting for " + C(),
        sub: (p.waiting || C() + " is having problems right now.") + " " + retryText(p.retry_at),
        btn: "Try now",
        act: "sync",
      });
    } else if (p.phase === "error") {
      Object.assign(st, { tone: "warn", icon: "warn", label: "Needs attention", sub: p.error || "The last sync did not finish", btn: "Review", act: "review" });
    } else if (flaggedFiles(p).length) {
      const f = flaggedFiles(p);
      Object.assign(st, { tone: "warn", icon: "warn", label: plural(f.length, "file needs", "files need") + " attention", sub: f[0].reason, btn: "Review", act: "review" });
    } else if (p.conflicts.length) {
      Object.assign(st, { tone: "warn", icon: "warn", label: plural(p.conflicts.length, "file", "files") + " changed in both places", sub: "See which ones", btn: "Review", act: "review" });
    } else if (hold) {
      Object.assign(st, { icon: "pause", label: "Paused", sub: hold });
    } else if (p.phase === "ok" || p.last_sync) {
      Object.assign(st, { tone: "ok", icon: "ok", label: "Up to date", sub: [ago(p.last_sync) + ".", quietExtras(p)].filter(Boolean).join(" ") });
    } else {
      Object.assign(st, { label: "Not synced yet", sub: "Waiting for the first sync" });
    }
    return st;
  }

  // Files left out for long enough, or for good reason, to show the user.
  function flaggedFiles(p) {
    return (p.files || []).filter((f) => f.flagged);
  }

  // Files being retried quietly in the background.
  function retryingFiles(p) {
    return (p.files || []).filter((f) => !f.flagged);
  }

  function quietExtras(p) {
    const parts = [];
    const r = retryingFiles(p).length;
    if (r) parts.push(plural(r, "file", "files") + " will be tried again soon.");
    if ((p.ignored || []).length) parts.push(plural(p.ignored.length, "file") + " ignored.");
    return parts.join(" ");
  }

  function retryText(at) {
    if (!at) return "Trying again soon.";
    const secs = Math.max(0, at - Math.floor(Date.now() / 1000));
    if (secs < 60) return "Trying again in less than a minute.";
    return "Trying again in " + plural(Math.round(secs / 60), "minute") + ".";
  }

  // st.extra, when given, is ready-made markup for under the text.
  function statusHtml(st) {
    return (
      '<div class="status ' + st.tone + '">' +
      icon(st.icon, 18) +
      '<div class="status-text"><span>' + esc(st.label) + "</span>" +
      (st.bar == null ? "" : '<div class="bar" role="img" aria-label="Sync progress, ' + Math.round(st.bar) + ' percent done"><div style="width:' + Math.max(2, Math.min(100, st.bar)).toFixed(1) + '%"></div></div>') +
      (st.sub ? '<span class="sub">' + esc(st.sub) + "</span>" : "") +
      (st.extra || "") +
      "</div></div>"
    );
  }

  function setHtml(el, html) {
    if (el.dataset.html !== html) {
      el.dataset.html = html;
      el.innerHTML = html;
    }
  }

  function setText(el, text) {
    if (el.textContent !== text) el.textContent = text;
  }

  function renderMain() {
    const acct = S.account;
    setText($("#main-title"), S.name);
    $("#main-settings").title = S.name + " settings";
    $("#main-settings").setAttribute("aria-label", S.name + " settings");
    // The sign-in name, then the space used. A different name from the
    // service's own is followed by the service, so it is clear what it is.
    const parts = [];
    if (S.name !== C()) parts.push(C());
    if (acct.email) parts.push(acct.email);
    if (acct.used != null && acct.total != null) parts.push(fmtBytes(acct.used) + " of " + fmtBytes(acct.total) + " used");
    setText($("#main-account"), parts.join(" \u00b7 "));

    // Banners
    const banners = [];
    if (S.test_mode) banners.push(banner("info", "Test mode: syncing with a folder on this PC in place of " + esc(C()) + "."));
    const eng = engineText();
    if (eng) banners.push(banner("warn", esc(eng), engineStuck() ? '<button type="button" class="btn" data-banner="engine">Try again</button>' : ""));
    const up = S.update || {};
    if (up.status === "available") banners.push(banner("info", "SyncHopper " + esc(up.version) + " is available.", '<button type="button" class="btn" data-banner="update">Install and restart</button>'));
    else if (up.status === "downloading") banners.push(banner("info", "Downloading SyncHopper " + esc(up.version) + ", " + up.percent + "% done."));
    else if (up.status === "installing") banners.push(banner("info", "Installing the update. SyncHopper will close and open again by itself."));
    else if (up.status === "error" && up.version) banners.push(banner("warn", esc(up.message), '<button type="button" class="btn" data-banner="update">Try again</button>'));
    if (S.sync.paused_all) banners.push(banner("warn", "All syncing is paused.", '<button type="button" class="btn" data-banner="resume">Resume</button>'));
    else if (S.sync.auto_pause === "battery") banners.push(banner("info", "Syncing is paused while this PC is on battery. Use Sync now on a folder to sync it anyway."));
    else if (S.sync.auto_pause === "metered") banners.push(banner("info", "Syncing is paused on this metered connection. Use Sync now on a folder to sync it anyway."));
    (S.folder_notices || []).forEach((n) => {
      let buttons = "";
      if (n.kind === "clash" || n.kind === "not_synced" || n.kind === "failed") buttons += '<button type="button" class="btn" data-banner="fsync" data-local="' + esc(n.local) + '">Sync it</button>';
      if (n.kind === "files") buttons += '<button type="button" class="btn" data-banner="fopen">Open folder</button>';
      buttons += '<button type="button" class="btn" data-banner="fdismiss" data-id="' + esc(n.id) + '">Dismiss</button>';
      banners.push(banner("info", esc(n.message), buttons));
    });
    setHtml($("#main-banners"), banners.join(""));

    // Folder rows, updated in place so buttons stay clickable.
    const list = $("#pair-list");
    const syncOn = S.settings.feature_sync;
    const pairs = syncOn ? S.pairs : [];
    const seen = {};
    pairs.forEach((p, index) => {
      seen[p.id] = true;
      let li = list.querySelector('li[data-id="' + p.id + '"]');
      if (!li) {
        li = document.createElement("li");
        li.dataset.id = p.id;
        li.innerHTML =
          icon("folder") +
          '<div class="frow-main"><span class="name"></span><span class="path selectable"></span></div>' +
          '<div class="frow-status"></div>' +
          '<button type="button" class="btn main"></button>' +
          '<button type="button" class="btn subtle icon more" aria-haspopup="menu">' + icon("more", 18) + "</button>";
        list.appendChild(li);
      }
      if (list.children[index] !== li) list.insertBefore(li, list.children[index]);
      const st = pairStatus(p);
      setText($(".name", li), p.name);
      $(".name", li).title = p.name;
      setText($(".path", li), p.local);
      $(".path", li).title = p.local + "   " + C() + ": /" + p.remote;
      setHtml($(".frow-status", li), statusHtml(st));
      const btn = $(".btn.main", li);
      setText(btn, st.btn);
      btn.dataset.act = st.act;
      $(".more", li).setAttribute("aria-label", "More actions for " + p.name);
    });
    $$("li[data-id]", list).forEach((li) => {
      if (!seen[li.dataset.id]) li.remove();
    });

    const empty = $("#main-empty");
    if (!syncOn) {
      empty.hidden = false;
      setText(empty, "Synced folders are turned off for this account. Turn them on in its settings to keep folders on this PC.");
    } else if (!pairs.length) {
      empty.hidden = false;
      setText(empty, "No folders are kept on this PC yet. Add one and it will stay in sync with " + C() + ".");
    } else {
      empty.hidden = true;
    }
    $("#add-folder").hidden = !syncOn;
    $("#main-footnote").hidden = !syncOn;
    setText(
      $("#main-footnote"),
      syncOn ? "Checks " + C() + " for changes every " + plural(S.settings.check_interval_min, "minute") + ". Changes on this PC sync right away." : ""
    );
    renderDrive();
  }

  function renderDrive() {
    const card = $("#drive-card");
    const d = S.drive;
    $("#drive-section").hidden = !d.enabled;
    if (!d.enabled) return;
    // tone, icon, status, what it means, button, what the button does
    const m = {
      off: ["quiet", "off", "Not connected", "The drive is turned on but not connected yet.", "Connect", "retry"],
      needs_driver: ["warn", "warn", "Driver needed", d.message || "The drive needs a small driver called WinFsp.", "Install driver", "install"],
      installing: ["busy", "sync", "Installing driver", "Windows may ask for permission. This can take a minute.", "", ""],
      starting: ["busy", "sync", "Connecting", "Setting up drive " + d.letter + ":", "", ""],
      connected: ["ok", "ok", "Connected", "Your whole account, without keeping the files on this PC", "Open", "open"],
      error: ["warn", "warn", "Not connected", d.message || "The drive could not connect.", "Try again", "retry"],
    }[d.state] || ["quiet", "off", d.state, "", "", ""];
    const html =
      '<div class="frow">' +
      icon("drive") +
      '<div class="frow-main"><span class="name">' + esc(C()) + " drive (" + esc(d.letter) + ':)</span><span class="desc">' + esc(m[3]) + "</span></div>" +
      '<div class="frow-status">' +
      statusHtml({
        tone: m[0],
        icon: m[1],
        label: m[2],
        sub: "",
        bar: null,
        extra: d.state === "needs_driver" ? '<button type="button" class="link" data-drive="page">Download the driver yourself</button>' : "",
      }) +
      "</div>" +
      (m[4] ? '<button type="button" class="btn main" data-drive="' + m[5] + '">' + esc(m[4]) + "</button>" : '<span class="gap-main"></span>') +
      '<span class="gap-more"></span>' +
      "</div>";
    setHtml(card, html);
  }

  $("#main-banners").addEventListener("click", (e) => {
    const b = e.target.closest("[data-banner]");
    if (!b) return;
    if (b.dataset.banner === "resume") run("set_paused_all", { paused: false });
    if (b.dataset.banner === "engine") run("retry_engine");
    if (b.dataset.banner === "update") installUpdate();
    if (b.dataset.banner === "fsync") run("request_add_folder", { account: cur, folder: b.dataset.local });
    if (b.dataset.banner === "fopen") run("open_target", { kind: "base_folder" });
    if (b.dataset.banner === "fdismiss") run("dismiss_folder_notice", { id: b.dataset.id });
  });

  $("#drive-card").addEventListener("click", (e) => {
    const b = e.target.closest("[data-drive]");
    if (!b) return;
    const what = b.dataset.drive;
    if (what === "open") run("open_target", { kind: "drive" });
    else if (what === "page") run("open_target", { kind: "winfsp_page" });
    else run("drive_action", { action: what });
  });

  $("#pair-list").addEventListener("click", (e) => {
    const li = e.target.closest("li[data-id]");
    if (!li) return;
    const id = li.dataset.id;
    const more = e.target.closest(".more");
    const main = e.target.closest(".btn.main");
    if (more) {
      e.stopPropagation();
      openRowMenu(id, more);
    } else if (main) {
      const act = main.dataset.act;
      if (act === "review") openReview(id);
      else run("pair_action", { id: id, action: act });
    }
  });

  // Row menu
  function closeRowMenu() {
    const menu = $("#row-menu");
    if (menu.hidden) return;
    menu.hidden = true;
    const opener = menu._opener;
    menu._opener = null;
    if (opener && document.contains(opener)) opener.focus();
  }

  function openRowMenu(id, anchor) {
    const p = S.pairs.find((x) => x.id === id);
    if (!p) return;
    const menu = $("#row-menu");
    menu.innerHTML =
      '<button type="button" role="menuitem" data-m="open">Open folder</button>' +
      '<button type="button" role="menuitem" data-m="sync">Sync now</button>' +
      '<button type="button" role="menuitem" data-m="' + (p.paused ? "resume" : "pause") + '">' + (p.paused ? "Resume syncing" : "Pause syncing") + "</button>" +
      (retryingFiles(p).length ? '<button type="button" role="menuitem" data-m="retrying">Files waiting to sync (' + retryingFiles(p).length + ")</button>" : "") +
      ((p.ignored || []).length ? '<button type="button" role="menuitem" data-m="ignored">Ignored files (' + p.ignored.length + ")</button>" : "") +
      '<button type="button" role="menuitem" data-m="repair">Repair</button>' +
      '<div class="menu-sep" role="separator"></div>' +
      '<button type="button" role="menuitem" class="danger" data-m="remove">Stop syncing this folder</button>';
    menu.dataset.id = id;
    menu.hidden = false;
    const r = anchor.getBoundingClientRect();
    const h = menu.offsetHeight;
    const top = r.bottom + 6 + h > window.innerHeight ? Math.max(8, r.top - h - 6) : r.bottom + 6;
    menu.style.top = top + "px";
    menu.style.left = Math.max(8, r.right - menu.offsetWidth) + "px";
    menu._opener = anchor;
    $("button", menu).focus();
  }

  document.addEventListener("click", (e) => {
    if (!e.target.closest("#row-menu")) closeRowMenu();
  });

  // The list of colors for the shown account, in the same pop-up the row
  // menu uses.
  function openColorMenu(anchor) {
    const menu = $("#row-menu");
    menu.innerHTML = (RAW.colors || [])
      .map(
        (c) =>
          '<button type="button" role="menuitemradio" aria-checked="' + (c.id === S.color) + '" class="color-item' + (c.id === S.color ? " chosen" : "") + '" data-color="' + esc(c.id) + '">' +
          '<span class="swatch" style="background:' + esc(c.hex) + '"></span>' + esc(c.name) + "</button>"
      )
      .join("");
    menu.dataset.id = "";
    menu.hidden = false;
    const r = anchor.getBoundingClientRect();
    const h = menu.offsetHeight;
    menu.style.top = (r.bottom + 6 + h > window.innerHeight ? Math.max(8, r.top - h - 6) : r.bottom + 6) + "px";
    menu.style.left = Math.max(8, Math.min(r.left, window.innerWidth - menu.offsetWidth - 8)) + "px";
    menu._opener = anchor;
    ($(".chosen", menu) || $("button", menu)).focus();
  }

  $("#st-color").addEventListener("click", (e) => {
    e.stopPropagation();
    if ($("#row-menu").hidden) openColorMenu(e.currentTarget);
    else closeRowMenu();
  });

  $("#row-menu").addEventListener("click", async (e) => {
    const b = e.target.closest("[data-color]");
    if (!b) return;
    closeRowMenu();
    if (b.dataset.color === S.color) return;
    if ((await run("set_account_color", { color: b.dataset.color })) === undefined) return;
    await refresh();
    render();
  });

  $("#row-menu").addEventListener("click", async (e) => {
    const b = e.target.closest("[data-m]");
    if (!b) return;
    const id = $("#row-menu").dataset.id;
    const p = S.pairs.find((x) => x.id === id);
    closeRowMenu();
    if (!p) return;
    const m = b.dataset.m;
    if (m === "open") run("open_target", { kind: "pair_folder", value: id });
    else if (m === "sync" || m === "pause" || m === "resume") run("pair_action", { id: id, action: m });
    else if (m === "repair") repairPair(p);
    else if (m === "retrying") openReview(id, "files");
    else if (m === "ignored") openIgnored(id);
    else if (m === "remove") removePair(p);
  });

  async function repairPair(p) {
    const ok = await confirmDialog(
      "Repair " + p.name + "?",
      "This rebuilds the folder's sync record by comparing this PC with " + C() + " again. Files missing on either side are copied across and nothing is deleted. A file you deleted very recently can come back once.",
      "Repair"
    );
    if (ok && (await run("pair_action", { id: p.id, action: "repair" })) !== undefined) toast("Rebuilding the sync record for " + p.name + ".");
  }

  async function removePair(p) {
    const ok = await confirmDialog(
      "Stop syncing " + p.name + "?",
      "The files stay where they are, on this PC and in " + C() + ". They just stop being kept in step. You can add the folder again later.",
      "Stop syncing",
      true
    );
    if (ok) run("remove_pair", { id: p.id });
  }

  // ----- review dialog ----------------------------------------------------

  function reviewKind(p, wanted) {
    const busy = p.phase === "syncing" || p.phase === "checking" || p.phase === "queued";
    if (p.approval && !busy) return p.approval.changed ? "changed" : (p.approval.everything ? "empty-" : "deletes-") + p.approval.side;
    if (busy && p.approval) return "working";
    if (p.phase === "error") return "error";
    if (p.phase === "waiting" && wanted !== "files") return "waiting";
    if ((p.files || []).length && (wanted === "files" || flaggedFiles(p).length)) return "skipped";
    if (p.conflicts.length) return "conflicts";
    return busy ? "working" : "fine";
  }

  function fileItem(f) {
    let retry;
    if (!f.auto_retry) retry = "SyncHopper won't try this one again by itself.";
    else if (f.next_retry) {
      const secs = f.next_retry - Math.floor(Date.now() / 1000);
      if (secs <= 60) retry = "It will be tried again at the next sync.";
      else if (secs < 90 * 60) retry = "It will be tried again in about " + plural(Math.max(1, Math.round(secs / 60)), "minute") + ".";
      else retry = "It will be tried again in about " + plural(Math.round(secs / 3600), "hour") + ".";
    } else retry = "It will be tried again at the next sync.";
    return (
      "<li><b>" + esc(f.path) + '</b><br><span class="small muted">' + esc(f.reason) + " " + esc(retry) + "</span>" +
      (f.detail ? '<details><summary class="small">Technical details</summary><pre>' + esc(f.detail) + "</pre></details>" : "") +
      (f.can_ignore ? '<div class="item-act"><button type="button" class="btn small-btn" data-ignore="' + esc(f.path) + '">' + (f.path.endsWith("/") ? "Ignore this folder" : "Ignore this file") + "</button></div>" : "") +
      "</li>"
    );
  }

  function reviewBody(p, kind) {
    const out = [];
    const a = p.approval;
    const c = esc(C());
    if (kind === "deletes-pc") {
      out.push("<p><b>" + num(a.deleted) + " of " + num(a.total) + " files</b> in this folder were deleted on this PC.</p>");
      out.push("<p>If that was on purpose, SyncHopper can delete them from " + c + " too. If not, it can copy them back to this PC.</p>");
    } else if (kind === "deletes-koofr") {
      out.push("<p><b>" + num(a.deleted) + " of " + num(a.total) + " files</b> in this folder were deleted in " + c + ".</p>");
      out.push("<p>If that was on purpose, SyncHopper can delete them from this PC too. Files deleted this way do not go to the Recycle Bin. If not, it can copy them back to " + c + ".</p>");
    } else if (kind === "empty-pc") {
      out.push("<p>Everything in this folder is gone from this PC. " + c + " still has <b>" + plural(a.deleted, "file") + "</b> from it.</p>");
      out.push("<p>SyncHopper never empties a whole folder by itself. You can copy the files back to this PC. If you really want the folder empty, delete the files in " + c + " yourself and syncing will carry on.</p>");
    } else if (kind === "empty-koofr") {
      out.push("<p>Everything in this folder is gone from " + c + ", or the folder was renamed or deleted there. This PC still has <b>" + plural(a.deleted, "file") + "</b> from it.</p>");
      out.push("<p>SyncHopper never empties a whole folder by itself. You can copy the files back to " + c + ". If you really want the folder empty, delete the files on this PC yourself and syncing will carry on.</p>");
    } else if (kind === "changed") {
      const count = a.deleted > 0 ? " (<b>" + plural(a.deleted, "file") + "</b>)" : "";
      out.push("<p>Every file in this folder looks changed since the last sync" + count + ". That can happen after a clock change or restoring a backup. Sync them anyway?</p>");
    } else if (kind === "waiting") {
      out.push("<p><b>" + esc(p.waiting || C() + " is having problems right now.") + "</b></p>");
      let next = "SyncHopper tries again by itself and catches up once " + C() + " is back.";
      if (p.waiting_kind === "drive") next = "SyncHopper tries again by itself and catches up once it can use the drive again.";
      else if (p.waiting_kind === "cert") next = "SyncHopper tries again by itself and catches up once it can connect securely.";
      else if (p.waiting_kind === "throttled") {
        next = "SyncHopper tries again by itself.";
        if (providerOf(S.provider).request_limit && !S.settings.limit_requests) next += " If this keeps happening, turn on Limit requests to " + C() + " in this account's settings.";
      }
      out.push("<p>Nothing is wrong with your files. " + esc(next) + " " + esc(retryText(p.retry_at)) + "</p>");
      if (p.details.length) out.push("<details><summary>Technical details</summary><pre>" + esc(p.details.join("\n")) + "</pre></details>");
    } else if (kind === "working") {
      out.push("<p>This folder is syncing right now.</p>");
    } else if (kind === "fine") {
      out.push("<p>Nothing needs attention in this folder any more.</p>");
    }
    if (kind === "error") {
      out.push("<p><b>" + esc(p.error || "The last sync did not finish.") + "</b></p>");
      out.push("<p>SyncHopper tries again by itself. Nothing was deleted.</p>");
      if (p.details.length) out.push("<details><summary>Technical details</summary><pre>" + esc(p.details.join("\n")) + "</pre></details>");
    }
    if ((kind === "error" || kind === "skipped" || kind === "conflicts") && (p.files || []).length) {
      const files = p.files;
      out.push("<p>" + (files.length === 1 ? "This file is" : "These files are") + " being left out so the rest of the folder can sync:</p>");
      out.push('<ul class="plain">' + files.map(fileItem).join("") + "</ul>");
      if (!files.some((f) => f.can_ignore)) out.push('<p class="small muted">If a file keeps failing for a day, you can choose to ignore it.</p>');
    }
    if ((kind === "error" || kind === "skipped" || kind === "conflicts") && p.conflicts.length) {
      const newest = S.settings.conflict_policy === "newest";
      out.push("<p>" + (p.conflicts.length === 1 ? "This file was" : "These files were") + " changed on this PC and in " + c + " before they could sync:</p>");
      out.push('<ul class="plain">' + p.conflicts.map((c) => "<li>" + esc(c) + "</li>").join("") + "</ul>");
      out.push(
        "<p class=\"small muted\">" +
          (newest
            ? "Your setting is Newest wins, so the newer version replaced the older one."
            : "The newer version kept its name. The older one was saved next to it with \"conflict\" in its name, on both sides. Keep the one you want and delete the other.") +
          "</p>"
      );
    }
    return out.join("");
  }

  function reviewButtons(p, kind) {
    const id = p.id;
    // Once a choice is made the dialog stops following the folder, so the
    // state change that follows cannot pop it open again.
    const act = (action) => () => {
      review = null;
      return run("pair_action", { id: id, action: action });
    };
    if (kind === "deletes-pc" || kind === "deletes-koofr") {
      const other = kind === "deletes-pc" ? C() : "this PC";
      const here = kind === "deletes-pc" ? "this PC" : C();
      return [
        { label: "Decide later", left: true },
        { label: "Copy them back to " + here, action: act("restore") },
        { label: "Delete them from " + other + " too", kind: "danger", action: act("approve") },
      ];
    }
    if (kind === "empty-pc" || kind === "empty-koofr") {
      const here = kind === "empty-pc" ? "this PC" : C();
      return [
        { label: "Decide later", left: true },
        { label: "Stop syncing this folder", action: () => { review = null; return run("remove_pair", { id: id }); } },
        { label: "Copy them back to " + here, kind: "primary", action: act("restore") },
      ];
    }
    if (kind === "changed") {
      return [
        { label: "Decide later", left: true },
        { label: "Sync them anyway", kind: "primary", action: act("approve_changed") },
      ];
    }
    if (kind === "working" || kind === "fine") return [{ label: "Close", kind: "primary" }];
    if (kind === "waiting") return [{ label: "Try now", action: act("sync") }, { label: "Close", kind: "primary" }];
    const buttons = [{ label: "Open folder", left: true, close: false, action: () => run("open_target", { kind: "pair_folder", value: id }).then(() => false) }];
    if (p.conflicts.length) buttons.push({ label: "Clear this list", action: act("dismiss_conflicts") });
    const nfiles = (p.files || []).length;
    if (kind === "error" && !nfiles) buttons.push({ label: "Try again now", action: act("sync") });
    if (nfiles) buttons.push({ label: "Try " + (nfiles === 1 ? "it" : "them") + " again now", action: act("retry_skipped") });
    buttons.push({ label: "Close", kind: "primary" });
    return buttons;
  }

  function reviewTitle(p, kind) {
    if (kind.startsWith("deletes-")) return "Check before deleting from " + p.name;
    if (kind.startsWith("empty-")) return p.name + " was emptied";
    if (kind === "changed") return "Check before syncing " + p.name;
    if (kind === "error") return p.name + " could not sync";
    if (kind === "waiting") return p.name + (p.waiting_kind === "drive" ? " is waiting for its drive" : " is waiting for " + C());
    if (kind === "skipped") return "Files left out of " + p.name;
    if (kind === "conflicts") return "Changed in both places";
    return p.name;
  }

  function openReview(id, wanted) {
    const p = S.pairs.find((x) => x.id === id);
    if (!p) return;
    if (review) {
      const old = review;
      review = null;
      old.modal.close();
    }
    const kind = reviewKind(p, wanted);
    const entry = { id: id, kind: kind, wanted: wanted, title: reviewTitle(p, kind), modal: null };
    entry.modal = openModal({
      width: 640,
      title: entry.title,
      body: reviewBody(p, kind),
      buttons: reviewButtons(p, kind),
      onClose: () => {
        if (review === entry) review = null;
      },
    });
    review = entry;
    entry.modal.el.addEventListener("click", async (e) => {
      const b = e.target.closest("[data-ignore]");
      if (!b) return;
      const path = b.dataset.ignore;
      const what = path.endsWith("/") ? "folder" : "file";
      const ok = await confirmDialog(
        "Ignore " + lastPart(path) + "?",
        "SyncHopper stops trying to sync this " + what + " and stops warning you about it. The " + what + " stays where it is. You can sync it again from the folder's menu under Ignored files.",
        "Ignore this " + what
      );
      if (ok) run("ignore_file", { id: id, path: path });
    });
  }

  function openIgnored(id) {
    const render = () => {
      const p = S.pairs.find((x) => x.id === id);
      const list = p ? p.ignored || [] : [];
      if (!list.length) return '<p class="muted">No files are being ignored in this folder.</p>';
      return (
        '<p class="small muted">These files are not synced. Choose Sync it again to have SyncHopper pick a file back up.</p>' +
        '<ul class="plain">' +
        list.map((path) => '<li class="row between gap12"><b class="selectable">' + esc(path) + '</b><button type="button" class="btn small-btn" data-unignore="' + esc(path) + '">Sync it again</button></li>').join("") +
        "</ul>"
      );
    };
    const p = S.pairs.find((x) => x.id === id);
    const modal = openModal({ title: "Ignored files in " + (p ? p.name : "this folder"), body: render(), buttons: [{ label: "Close", kind: "primary" }], onClose: () => { ignoredModal = null; } });
    ignoredModal = { modal: modal, render: render };
    modal.el.addEventListener("click", (e) => {
      const b = e.target.closest("[data-unignore]");
      if (b) run("unignore_file", { id: id, path: b.dataset.unignore });
    });
  }
  let ignoredModal = null;

  // Keeps an open review dialog in step with what is going on.
  function updateReview() {
    if (!S) return;
    if (ignoredModal) ignoredModal.modal.setBody(ignoredModal.render());
    if (!review) return;
    const p = S.pairs.find((x) => x.id === review.id);
    if (!p) {
      review.modal.close();
      return;
    }
    const kind = reviewKind(p, review.wanted);
    if (kind !== review.kind || reviewTitle(p, kind) !== review.title) {
      openReview(p.id, review.wanted);
    } else {
      review.modal.setBody(reviewBody(p, kind));
    }
  }

  async function installUpdate() {
    const up = S.update || {};
    const ok = await confirmDialog(
      "Install SyncHopper " + up.version + "?",
      "SyncHopper stops what it's doing, installs the update, and opens again by itself. Your settings and folders stay as they are. Any sync that gets interrupted picks up where it left off.",
      "Install and restart"
    );
    if (ok) run("install_app_update");
  }

  // ----- add a folder ------------------------------------------------------

  $("#add-folder").addEventListener("click", () => addFolderDialog());

  // `prefill`, when given, holds a folder on this PC to start from: local,
  // remote (the suggested folder in the account) and an optional note.
  // `moveTo`, given with it, hands the folder to another account.
  async function addFolderDialog(prefill, moveTo) {
    const base = await invoke("default_base_folder").catch(() => "");
    const choose = !!(prefill && moveTo && readyAccounts().length > 1);
    let localTouched = !!(prefill && prefill.local);
    let token = 0;
    const modal = openModal({
      title: "Add a folder",
      width: 600,
      body:
        '<p class="muted small">Pick a folder in ' + esc(C()) + " and where to keep it on this PC. The two are then kept the same.</p>" +
        (choose ? '<div class="field"><label for="af-acct">Account</label><select id="af-acct">' + accountOptions(cur) + "</select></div>" : "") +
        '<div class="field"><label for="af-remote">' + esc(C()) + ' folder</label><div class="row gap8"><input id="af-remote" type="text" class="grow" autocomplete="off" spellcheck="false" placeholder="For example Documents"><button type="button" class="btn" id="af-rbrowse">Browse</button></div></div>' +
        '<div class="field"><label for="af-local">Folder on this PC</label><div class="row gap8"><input id="af-local" type="text" class="grow" autocomplete="off" spellcheck="false"><button type="button" class="btn" id="af-lbrowse">Browse</button></div></div>' +
        (prefill && prefill.note ? banner("info", esc(prefill.note)) : "") +
        '<p id="af-preview" class="small muted" role="status"></p>' +
        '<p class="small muted">Files missing on either side are copied across. Nothing is deleted. If the same file is in both places, the newer one is kept.</p>',
      buttons: [
        { label: "Cancel" },
        {
          label: "Add folder",
          kind: "primary",
          action: async (m) => {
            const remote = $("#af-remote", m.el).value.trim();
            const local = $("#af-local", m.el).value.trim();
            if (!local) {
              m.setError("Pick a folder on this PC.");
              return false;
            }
            if (!remote && !(await confirmDialog("Sync your whole " + C() + " account?", "Leaving the " + C() + " folder empty keeps your entire account on this PC. That can be a lot of data.", "Sync everything"))) return false;
            try {
              await invoke("add_pair", { local: local, remote: remote });
            } catch (e) {
              m.setError(String(e));
              return false;
            }
            return true;
          },
        },
      ],
    });
    const remoteEl = $("#af-remote", modal.el);
    const localEl = $("#af-local", modal.el);
    const previewEl = $("#af-preview", modal.el);
    if (prefill && prefill.local) {
      remoteEl.value = prefill.remote || "";
      localEl.value = prefill.local;
    }
    const acctEl = $("#af-acct", modal.el);
    if (acctEl)
      acctEl.addEventListener("change", () => {
        if (acctEl.value === cur) return;
        // The other account checks the folder for itself and opens this
        // dialog again with its own suggestion.
        modal.close();
        moveTo(acctEl.value);
      });

    function suggestLocal() {
      if (localTouched || !base) return;
      const name = lastPart(remoteEl.value.trim());
      localEl.value = name ? joinLocal(base, name) : "";
    }
    let timer = null;
    function schedulePreview() {
      clearTimeout(timer);
      timer = setTimeout(preview, 500);
    }
    async function preview() {
      const mine = ++token;
      const remote = remoteEl.value.trim();
      const local = localEl.value.trim();
      if (!local) {
        previewEl.textContent = "";
        return;
      }
      previewEl.textContent = "Counting files";
      try {
        const pv = await invoke("preview_pair", { local: local, remote: remote });
        if (mine !== token) return;
        previewEl.textContent = previewText(pv);
      } catch (e) {
        if (mine !== token) return;
        previewEl.textContent = "Could not count the files: " + e;
      }
    }
    remoteEl.addEventListener("input", () => {
      suggestLocal();
      schedulePreview();
    });
    localEl.addEventListener("input", () => {
      localTouched = true;
      schedulePreview();
    });
    $("#af-rbrowse", modal.el).addEventListener("click", async () => {
      const picked = await browseCloud(remoteEl.value.trim(), "Choose a " + C() + " folder to sync");
      if (picked == null) return;
      remoteEl.value = picked;
      suggestLocal();
      preview();
    });
    $("#af-lbrowse", modal.el).addEventListener("click", async () => {
      const picked = await run("pick_folder", { start: base || null });
      if (!picked) return;
      localEl.value = picked;
      localTouched = true;
      preview();
    });
    if (prefill && prefill.local) preview();
  }

  function previewText(pv) {
    let text = C() + " has " + plural(pv.remote_count, "file") + " here (" + fmtBytes(pv.remote_bytes) + "). The PC folder has " + plural(pv.local_count, "file") + (pv.local_count ? " (" + fmtBytes(pv.local_bytes) + ")" : "") + ".";
    if (pv.free_bytes != null && pv.remote_bytes > pv.free_bytes) {
      text += " Careful: that is more than the " + fmtBytes(pv.free_bytes) + " free on this drive.";
    }
    return text;
  }

  // ----- upload screen ------------------------------------------------------

  function uploadRow(u) {
    if (u.state === "uploading") {
      return (
        '<div class="bar" role="img" aria-label="Upload progress, ' + Math.round(u.percent) + ' percent done"><div style="width:' + Math.max(2, u.percent).toFixed(1) + '%"></div></div>' +
        '<span class="speed">' + (u.speed > 0 ? esc(fmtSpeed(u.speed)) : "Starting") + "</span>"
      );
    }
    if (u.state === "done") return '<span class="state ok">Done</span>';
    if (u.state === "failed") return '<span class="state bad">' + esc(u.error || "Failed") + "</span>";
    if (u.state === "cancelled") return '<span class="state">Cancelled</span>';
    return '<span class="state">Waiting</span>';
  }

  function renderUpload() {
    setText($("#up-title"), "Upload to " + S.name);
    setText($("#up-sub"), "Files go straight to " + C() + ". They are not kept in sync with this PC.");
    setText($("#up-dest-label"), C() + " folder");
    $("#up-dest").placeholder = "Top of your " + C();
    const list = $("#upload-list");
    const seen = {};
    S.uploads.forEach((u, index) => {
      seen[u.id] = true;
      let li = list.querySelector('li[data-id="' + u.id + '"]');
      if (!li) {
        li = document.createElement("li");
        li.dataset.id = u.id;
        li.innerHTML = '<span class="name"></span><span class="row gap8 st"></span><button type="button" class="btn retry" hidden>Try again</button>';
        list.appendChild(li);
      }
      if (list.children[index] !== li) list.insertBefore(li, list.children[index]);
      setText($(".name", li), u.name);
      $(".name", li).title = u.path + "  to  " + C() + ": /" + u.dest;
      setHtml($(".st", li), uploadRow(u));
      $(".retry", li).hidden = !(u.state === "failed" || u.state === "cancelled");
    });
    $$("li[data-id]", list).forEach((li) => {
      if (!seen[li.dataset.id]) li.remove();
    });
    const total = S.uploads.length;
    const done = S.uploads.filter((u) => u.state === "done").length;
    const active = S.uploads.filter((u) => u.state === "waiting" || u.state === "uploading").length;
    const failed = S.uploads.filter((u) => u.state === "failed").length;
    let summary = "Nothing is uploading.";
    if (active) summary = done + " of " + total + " done. Uploads keep going if you hide this window.";
    else if (total) summary = done + " of " + total + " done." + (failed ? " " + plural(failed, "upload") + " failed." : "");
    setText($("#upload-summary"), summary);
    $("#up-cancel").disabled = !active;
    $("#up-clear").disabled = total === active;
    if (document.activeElement !== $("#up-dest") && $("#up-dest").value !== (S.settings.upload_dest || "") && !$("#up-dest").dataset.dirty) {
      $("#up-dest").value = S.settings.upload_dest || "";
    }
  }

  function uploadDest() {
    return $("#up-dest").value.trim();
  }

  async function addUploads(paths) {
    if (!paths || !paths.length) return;
    const added = await run("upload_add", { paths: paths, dest: uploadDest() });
    delete $("#up-dest").dataset.dirty;
    if (added === 0) toast("Those are already in the list.");
  }

  $("#up-dest").addEventListener("input", () => {
    $("#up-dest").dataset.dirty = "1";
  });
  $("#up-dest").addEventListener("change", () => {
    delete $("#up-dest").dataset.dirty;
    run("set_upload_dest", { dest: uploadDest() });
  });
  $("#up-browse").addEventListener("click", async () => {
    const picked = await browseCloud(uploadDest(), "Choose where uploads go");
    if (picked == null) return;
    $("#up-dest").value = picked;
    delete $("#up-dest").dataset.dirty;
    run("set_upload_dest", { dest: picked });
  });
  $("#up-files").addEventListener("click", async () => addUploads(await run("pick_upload", { folders: false })));
  $("#up-folder").addEventListener("click", async () => addUploads(await run("pick_upload", { folders: true })));
  $("#up-cancel").addEventListener("click", () => run("upload_action", { action: "cancel_all" }));
  $("#up-clear").addEventListener("click", () => run("upload_action", { action: "clear" }));
  $("#up-hide").addEventListener("click", () => run("hide_window"));
  $("#upload-list").addEventListener("click", (e) => {
    const b = e.target.closest(".retry");
    const li = e.target.closest("li[data-id]");
    if (b && li) run("upload_action", { action: "retry", id: Number(li.dataset.id) });
  });

  // ----- settings -----------------------------------------------------------

  const INTERVALS = [5, 10, 30];
  const TRANSFERS = [2, 4, 8];
  const DELETE_CHOICES = [
    [50, "Half of a folder"],
    [25, "A quarter of a folder"],
    [10, "A tenth of a folder"],
  ];

  function fillSelect(el, options, current) {
    const opts = options.slice();
    if (!opts.some((o) => String(o[0]) === String(current))) opts.push([current, String(current)]);
    const html = opts.map((o) => '<option value="' + esc(o[0]) + '">' + esc(o[1]) + "</option>").join("");
    if (el.dataset.html !== html) {
      el.dataset.html = html;
      el.innerHTML = html;
    }
  }

  // Copies the saved settings into the controls. A control the user is in
  // the middle of changing is left alone unless `force` is set.
  function syncControls(force) {
    if (!S) return;
    const s = S.settings;
    fillSelect($("#st-every"), INTERVALS.map((n) => [n, plural(n, "minute")]), s.check_interval_min);
    fillSelect($("#st-par"), TRANSFERS.map((n) => [n, String(n)]), s.transfers);
    fillSelect($("#st-del"), DELETE_CHOICES, s.max_delete_percent);
    fillSelect($("#st-letter"), S.drive.free_letters.map((l) => [l, l + ":"]), s.drive_letter);
    $$("[data-set]").forEach((el) => {
      if (!force && document.activeElement === el) return;
      const value = s[el.dataset.set];
      if (el.type === "checkbox") el.checked = !!value;
      else el.value = String(value);
    });
    $("#st-letter-row").hidden = !s.feature_drive;
    $("#st-drive").disabled = !S.windows && !s.feature_drive;
    setText($("#st-drive-note"), S.windows ? "Browse your whole account as a drive letter. Turning this on installs a small driver." : "Only available on Windows.");
    // Wording that names the account or its service.
    const c = C();
    const p = providerOf(S.provider);
    setText($("#st-title"), S.name + " settings");
    setText($("#st-name"), S.name);
    const mine = (RAW.colors || []).find((c) => c.id === S.color) || (RAW.colors || [])[0];
    if (mine) {
      $("#st-color-swatch").style.background = mine.hex;
      $("#st-color").title = "Color: " + mine.name;
      $("#st-color").setAttribute("aria-label", "Color of this account's icon: " + mine.name);
    }
    setText($("#st-account"), S.account.email || c + " account");
    setText($("#st-reconnect-note"), "Sign in again, for example after changing your password.");
    setText($("#st-disconnect-note"), "Stops using this account here. Files stay on this PC and in " + c + ".");
    setText($("#st-drive-label"), c + " drive");
    setText($("#st-sidebar-label"), S.name + " in File Explorer's sidebar");
    setText($("#st-sidebar-note"), S.windows ? "Adds " + S.name + " next to OneDrive and This PC. It opens the folder this account's synced folders are in." : "Only available on Windows.");
    setText($("#st-base"), s.base_folder || "Not chosen yet");
    $("#st-base").title = s.base_folder || "";
    $("#st-base-open").disabled = !s.base_folder;
    setText($("#st-every-label"), "Check " + c + " for changes every");
    setText($("#st-auto-label"), "Automatically sync new folders added to your " + c + " folder");
    setText($("#st-auto-note"), "New folders you put there get a matching folder in " + c + ". If " + c + " already has one with that name, SyncHopper asks first. When this is off, SyncHopper tells you about new folders instead.");
    $("#st-only").hidden = !(p.http2_choice || p.request_limit);
    setText($("#st-h-only"), "Only for " + c);
    $("#st-http2-row").hidden = !p.http2_choice;
    setText($("#st-http2-note"), "Off is much faster for uploads to " + c + ". Turn on only to test.");
    $("#st-limit-row").hidden = !p.request_limit;
    setText($("#st-limit-label"), "Limit requests to " + c);
    setText($("#st-limit-note"), "Limits syncs, uploads and the drive to " + p.request_limit + " requests per second each, the rate " + c + " recommends. Turn this on if " + c + " says SyncHopper is sending too many requests. Big syncs may be a little slower, and the drive reconnects when you change this.");
    const ready = readyAccounts();
    setText(
      $("#st-menu-note"),
      ready.length === 1
        ? "Adds Send to " + ready[0].cloud + " and Sync folder to " + ready[0].cloud + " to File Explorer's right-click menu."
        : "Adds Send to and Sync folder to to File Explorer's right-click menu. SyncHopper then asks which account."
    );
    $("#ge-limit-note").hidden = ready.length < 2;
    [["up", s.upload_limit_mbps], ["down", s.download_limit_mbps]].forEach((pair) => {
      const mode = $("#ad-" + pair[0] + "-mode");
      const input = $("#ad-" + pair[0]);
      const wrap = $("#ad-" + pair[0] + "-wrap");
      if (force || (document.activeElement !== mode && document.activeElement !== input)) {
        mode.value = pair[1] > 0 ? "custom" : "none";
        wrap.hidden = !(pair[1] > 0);
        if (pair[1] > 0) input.value = String(pair[1]);
      }
    });
    $("#st-sidebar").disabled = !S.windows && !s.feature_sidebar;
    $("#st-auto").disabled = !s.feature_sync;
    const up = S.update || {};
    let appNote = "You have SyncHopper " + S.version + ".";
    if (!up.enabled) appNote += " This copy was built on this PC, so it can't update itself. Install the release version to get updates.";
    else if (up.status === "checking") appNote += " Checking for updates.";
    else if (up.status === "current") appNote += " It's the newest version.";
    else if (up.status === "available") appNote += " Version " + up.version + " is available. Install it from the main window.";
    else if (up.status === "error") appNote += " " + (up.message || "");
    setText($("#ad-app-note"), appNote);
    $("#ad-app-update").disabled = !up.enabled;
    $("#ad-check").disabled = !up.enabled || up.status === "checking" || up.status === "downloading" || up.status === "installing";
    const e = S.engine;
    let engineNote = "The engine is rclone " + e.version + ".";
    if (e.signed) engineNote = "The engine is rclone " + e.version + ", signed by Cover Two Strategies LLC.";
    else if (S.engine_channel) engineNote += " SyncHopper switches it to a signed copy as soon as it can download one.";
    setText($("#ad-engine-note"), e.state !== "ready" ? engineText() : engineNote);
  }

  async function saveSetting(key, value) {
    try {
      await invoke("update_settings", { patch: { [key]: value } });
    } catch (e) {
      toast(String(e), 8000);
    }
    await refresh();
    syncControls(true);
    renderAccounts();
  }

  document.addEventListener("change", async (e) => {
    const el = e.target.closest("[data-set]");
    if (!el) return;
    const key = el.dataset.set;
    let value = el.type === "checkbox" ? el.checked : el.value;
    if (el.dataset.num != null) value = Number(value);
    if (key === "feature_drive" && value === true) {
      const ok = await confirmDialog(
        "Turn on the " + C() + " drive?",
        "The drive shows your whole " + C() + " account as a drive letter without keeping the files on this PC. It needs a small free driver called WinFsp. If it is not installed yet, SyncHopper installs it now and Windows asks for permission.",
        "Turn on"
      );
      if (!ok) {
        el.checked = false;
        return;
      }
    }
    if (key === "feature_sync" && value === false) {
      const ok = await confirmDialog("Turn off synced folders?", "Syncing stops for every folder in this account. The files stay where they are, and you can turn this back on any time.", "Turn off");
      if (!ok) {
        el.checked = true;
        return;
      }
    }
    saveSetting(key, value);
  });

  ["up", "down"].forEach((dir) => {
    const key = dir === "up" ? "upload_limit_mbps" : "download_limit_mbps";
    const mode = $("#ad-" + dir + "-mode");
    const input = $("#ad-" + dir);
    mode.addEventListener("change", () => {
      if (mode.value === "none") {
        saveSetting(key, 0);
      } else {
        if (!(Number(input.value) > 0)) input.value = "5";
        $("#ad-" + dir + "-wrap").hidden = false;
        saveSetting(key, Number(input.value));
      }
    });
    input.addEventListener("change", () => {
      const v = Number(input.value);
      if (v > 0) saveSetting(key, v);
      else syncControls(true);
    });
  });

  $("#st-disconnect").addEventListener("click", async () => {
    const last = readyAccounts().length === 1;
    const ok = await confirmDialog(
      "Disconnect " + S.name + "?",
      "SyncHopper stops syncing this account and forgets its sign-in and its folder list. Your files stay where they are, on this PC and in " + C() + "." + (last ? " It is your only account, so setup starts over next." : ""),
      "Disconnect",
      true
    );
    if (ok) await run("remove_account");
  });

  $("#st-rename").addEventListener("click", () => {
    openModal({
      title: "Rename " + S.name,
      body:
        '<div class="field"><label for="rn-name">Name</label><input id="rn-name" type="text" class="fill" maxlength="40" autocomplete="off" spellcheck="false" value="' + esc(S.name) + '"></div>' +
        '<p class="small muted">This is the name SyncHopper shows for the account, here and in File Explorer. Nothing changes in ' + esc(C()) + ".</p>",
      buttons: [
        { label: "Cancel" },
        {
          label: "Rename",
          kind: "primary",
          action: async (m) => {
            await invoke("rename_account", { name: $("#rn-name", m.el).value });
            await refresh();
            render();
            return true;
          },
        },
      ],
    });
  });

  $("#st-base-open").addEventListener("click", () => run("open_target", { kind: "base_folder" }));

  // ----- signing in again -----------------------------------------------

  // Why Filen needs the account password, and what is kept instead.
  function keysWhy(c) {
    return (
      c + " encrypts your files with a key made from your password, so SyncHopper needs your password once, to sign in and unlock your files. " +
      "It is never saved and never leaves this PC. Like " + c + "'s own apps, SyncHopper sends " + c + " a code made from it instead."
    );
  }
  function keysKept(c, reconnecting) {
    return (
      "SyncHopper keeps only the keys that unlock your files, saved on this PC" +
      (RAW.lock_signins ? " and locked so that only your Windows account can read them." : ".") +
      (reconnecting ? "" : " If you change your " + c + " password, choose Reconnect in this account's settings.")
    );
  }

  // The words in an error that mean Filen wants a two-factor code.
  const CODE_WORDS = "two-factor code";

  // The boxes each kind of sign-in needs. `v` holds what is typed so far.
  function signInFields(p, v) {
    const c = esc(p.name);
    if (p.sign_in === "keys") {
      // The code box shows once Filen asks for a code.
      return (
        '<div class="field"><label for="wz-email">' + c + ' email</label><input id="wz-email" type="email" class="narrow" autocomplete="off" spellcheck="false" value="' + esc(v.email) + '"></div>' +
        '<div class="field"><label for="wz-pass">' + c + ' password</label><input id="wz-pass" type="password" class="narrow" autocomplete="off" value="' + esc(v.password) + '"></div>' +
        '<div class="field" id="wz-code-field"' + (v.needsCode ? "" : " hidden") + '><label for="wz-code">Two-factor code</label>' +
        '<input id="wz-code" type="text" class="narrow code" inputmode="numeric" autocomplete="one-time-code" spellcheck="false" value="' + esc(v.code || "") + '">' +
        '<p class="small muted">The code from your authenticator app.</p></div>'
      );
    }
    if (p.sign_in === "browser") {
      // kDrive signs in with an Infomaniak account, which can have more
      // than one kDrive, so it is asked which one.
      const which = p.asks_drive
        ? '<div class="field"><label for="wz-kdrive">Address of your ' + c + '</label>' +
          '<input id="wz-kdrive" type="text" class="narrow" autocomplete="off" spellcheck="false" placeholder="https://ksuite.infomaniak.com/.../kdrive/app/drive/123456/files" value="' + esc(v.kdrive || "") + '">' +
          '<p class="small muted">Open your ' + c + " in your browser and copy the address from the address bar. SyncHopper only uses the number after /drive/, which says which " + c + " is yours.</p></div>"
        : "";
      return (
        which +
        "<p>" + c + " signs you in on " + (p.asks_drive ? "Infomaniak's" : "its own") + " page. SyncHopper opens it in your browser and never sees your password.</p>" +
        '<p class="small muted">The page asks you to let rclone use your account. rclone is the sync engine SyncHopper is built on.</p>'
      );
    }
    return (
      '<div class="field"><label for="wz-email">' + c + ' email</label><input id="wz-email" type="email" class="narrow" autocomplete="off" spellcheck="false" value="' + esc(v.email) + '"></div>' +
      '<div class="field"><label for="wz-pass">App password</label><input id="wz-pass" type="password" class="narrow" autocomplete="off" value="' + esc(v.password) + '">' +
      '<p class="small muted">No app password yet? <button type="button" class="link" data-open="koofr_password_page">Open ' + c + "'s app password page</button>, create one, and paste it here.</p></div>"
    );
  }

  // What is in the sign-in boxes inside `root`.
  function signInValues(root) {
    const val = (id) => {
      const el = $(id, root);
      return el ? el.value : "";
    };
    return { email: val("#wz-email"), password: val("#wz-pass"), code: val("#wz-code"), drive: val("#wz-kdrive") };
  }

  // While a sign-in waits in the browser: what to show, with a way to open
  // the page again and to give up.
  function browserWaitHtml(link) {
    return (
      '<span class="wait">' +
      statusLine("busy", "sync", "Waiting for you to finish signing in in your browser") +
      '<span class="wait-links small">' +
      (link ? '<button type="button" class="link" data-signin-link="' + esc(link) + '">Open the sign-in page again</button>' : "") +
      '<button type="button" class="link" data-signin-cancel>Cancel</button></span></span>'
    );
  }

  document.addEventListener("click", (e) => {
    const open = e.target.closest("[data-open]");
    const link = e.target.closest("[data-signin-link]");
    const cancel = e.target.closest("[data-signin-cancel]");
    if (open) run("open_target", { kind: open.dataset.open });
    if (link) run("open_target", { kind: "signin_link", value: link.dataset.signinLink });
    if (cancel) run("cancel_sign_in");
  });

  let reconnectModal = null;
  $("#st-reconnect").addEventListener("click", () => {
    const p = providerOf(S.provider);
    const browser = p.sign_in === "browser";
    const modal = openModal({
      title: "Reconnect " + S.name,
      width: 520,
      body:
        '<p class="muted small">Your folders and settings stay as they are. Only the saved sign-in is replaced.</p>' +
        (p.sign_in === "keys" ? '<p class="small">' + keysWhy(esc(p.name)) + "</p>" : "") +
        signInFields(p, { email: S.account.email || "", password: "", code: "", kdrive: S.kdrive || "" }) +
        (p.sign_in === "keys" ? '<p class="tiny muted">' + keysKept(esc(p.name), true) + "</p>" : "") +
        '<p id="rc-wait" role="status"></p>',
      buttons: [
        { label: "Cancel", action: () => { run("cancel_sign_in"); return true; } },
        {
          label: browser ? "Sign in to " + p.name : "Reconnect",
          kind: "primary",
          action: async (m) => {
            m.setError("");
            m.waiting = browser;
            updateReconnect();
            try {
              await invoke("reconnect", { signin: signInValues(m.el) });
            } catch (e) {
              m.waiting = false;
              updateReconnect();
              const field = $("#wz-code-field", m.el);
              if (field && String(e).includes(CODE_WORDS)) {
                field.hidden = false;
                setTimeout(() => $("#wz-code", m.el).focus(), 0);
              }
              throw e;
            }
            m.waiting = false;
            toast("Reconnected to " + p.name + ".");
            return true;
          },
        },
      ],
      onClose: () => {
        reconnectModal = null;
      },
    });
    reconnectModal = modal;
  });

  // Keeps the reconnect dialog's waiting line in step.
  function updateReconnect() {
    if (!reconnectModal) return;
    const el = $("#rc-wait", reconnectModal.el);
    if (!el) return;
    const link = S.signing_in ? S.signing_in.link : null;
    setHtml(el, reconnectModal.waiting ? browserWaitHtml(link) : "");
  }

  $("#ad-log").addEventListener("click", () => run("open_target", { kind: "log" }));
  $("#ad-rclone").addEventListener("click", () => run("open_target", { kind: "rclone_site" }));

  $("#ad-check").addEventListener("click", async () => {
    const before = S.update ? S.update.status : "";
    await run("check_updates");
    await refresh();
    syncControls(true);
    const up = S.update || {};
    if (up.status === "current" && before !== "current") toast("You have the newest version of SyncHopper.");
    else if (up.status === "current") toast("Still the newest version.");
    else if (up.status === "available") toast("SyncHopper " + up.version + " is available. Install it from the main window.");
  });

  $("#ad-diag").addEventListener("click", async () => {
    const text = await run("diagnostics");
    if (text === undefined) return;
    let copied = false;
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
    } catch (_) {
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      try {
        copied = document.execCommand("copy");
      } catch (_e) {
        copied = false;
      }
      ta.remove();
    }
    if (copied) toast("Diagnostic info copied. It has no passwords, and your email is masked.");
    else openModal({ title: "Diagnostic info", width: 640, body: '<p class="small muted">Copying did not work, so here it is to select and copy by hand.</p><textarea rows="12" readonly class="fill">' + esc(text) + "</textarea>", buttons: [{ label: "Close", kind: "primary" }] });
  });

  $("#ad-repair").addEventListener("click", () => {
    if (!S.pairs.length) {
      toast("There are no synced folders to repair.");
      return;
    }
    const modal = openModal({
      title: "Repair a folder",
      body: '<p class="small muted">Pick the folder to repair.</p><ul class="box list pick">' + S.pairs.map((p) => '<li tabindex="0" role="button" data-id="' + esc(p.id) + '">' + icon("folder", 18) + '<span class="name">' + esc(p.name) + '</span><span class="tiny muted">' + esc(p.local) + "</span></li>").join("") + "</ul>",
      buttons: [{ label: "Cancel" }],
    });
    const pick = (e) => {
      const li = e.target.closest("li[data-id]");
      if (!li || (e.type === "keydown" && e.key !== "Enter" && e.key !== " ")) return;
      const p = S.pairs.find((x) => x.id === li.dataset.id);
      modal.close();
      if (p) repairPair(p);
    };
    modal.el.addEventListener("click", pick);
    modal.el.addEventListener("keydown", pick);
  });

  $("#ad-ignore").addEventListener("click", () => {
    openModal({
      title: "Ignored files",
      body:
        '<p class="small muted">One pattern per line. A star stands for any text, so *.tmp means every file ending in .tmp. Files that match are never uploaded or downloaded.</p>' +
        '<textarea id="ig-text" rows="9" spellcheck="false" aria-label="Ignore patterns, one per line" class="fill">' + esc(S.settings.ignore_patterns.join("\n")) + "</textarea>" +
        '<p class="small muted">The list is the same for every account. Saving a changed list makes every folder compare itself with its account again, which can take a while for big folders.</p>',
      buttons: [
        { label: "Use the standard list", left: true, close: false, action: (m) => { $("#ig-text", m.el).value = S.default_ignores.join("\n"); return false; } },
        { label: "Cancel" },
        {
          label: "Save",
          kind: "primary",
          action: async (m) => {
            const lines = $("#ig-text", m.el).value.split("\n").map((l) => l.trim()).filter(Boolean);
            await invoke("update_settings", { patch: { ignore_patterns: lines } });
            return true;
          },
        },
      ],
    });
  });

  $("#ad-defaults").addEventListener("click", async () => {
    const ok = await confirmDialog("Restore the general defaults?", "Everything on this screen goes back to how it started, except Start with Windows and the right-click menu. Your accounts, their folders and their own settings are not touched.", "Restore defaults");
    if (!ok) return;
    await run("restore_defaults");
    await refresh();
    syncControls(true);
  });

  // ----- setup wizard -------------------------------------------------------
  //
  // Sets up one account: pick the service, sign in, choose features and
  // folders, check, go. It runs when SyncHopper has no account yet, and
  // again for every account added later (then the service was already
  // picked on the Add cloud provider screen, and Back from signing in
  // leaves setup).

  const STEPS = ["Service", "Sign in", "Features", "Folders", "Review"];

  // `providerId` is the service picked on the Add cloud provider screen, or
  // null to start by picking one.
  function startWizard(providerId) {
    wiz = {
      first: readyAccounts().length === 0, // the very first account
      picked: !!providerId, // the service was picked before setup opened
      provider: providerId || null,
      account: null, // the account's id once it has signed in
      step: providerId ? 1 : 0,
      email: "",
      password: "",
      code: "", // Filen's two-factor code
      needsCode: false, // Filen asked for one
      kdrive: "", // kDrive: the address of the kDrive
      testing: false,
      result: null, // { ok, text }
      sync: true,
      drive: false,
      auto: RAW.general.autostart,
      base: "",
      folders: null,
      foldersError: "",
      loading: false,
      chosen: {},
      previews: {},
      busy: false,
      error: "",
    };
    compose();
    go("wizard");
    renderWizard();
  }

  // Leaves setup without finishing: the account that was being set up, if
  // it got as far as signing in, is taken out again.
  async function leaveWizard() {
    const acct = wiz ? wiz.account : null;
    run("cancel_sign_in");
    wiz = null;
    if (acct) await run("remove_account", { account: acct });
    await refresh();
    go(readyAccounts().length ? "add" : "loading");
    if (!readyAccounts().length) startWizard(null);
  }

  function connected() {
    return !!(wiz.account && wiz.result && wiz.result.ok);
  }

  function wizEngineLine() {
    const e = S.engine;
    if (e.state === "ready") return "The sync engine is ready (rclone " + e.version + ").";
    if (e.state === "downloading") return "Downloading the sync engine (rclone), " + e.percent + "% done. This happens once.";
    if (e.state === "error") return (e.message || "The sync engine could not be set up.") + " ";
    if (e.state === "blocked") return (e.message || "This PC blocked SyncHopper's sync engine.") + " ";
    return "Getting the sync engine ready.";
  }

  // Updates the bits of the wizard that depend on live state, without
  // rebuilding the page (which would wipe what is being typed).
  function wizardTick() {
    if (!wiz) return;
    const line = $("#wz-engine");
    if (line) {
      setText($("#wz-engine-text"), wizEngineLine());
      $("#wz-engine-retry").hidden = !engineStuck();
    }
    showResult();
  }

  function renderSteps() {
    $("#wiz-steps").innerHTML = STEPS.map((name, i) => {
      const skipped = i === 3 && !wiz.sync && wiz.step !== 3;
      const cls = i === wiz.step ? "current" : i < wiz.step ? "done" : "";
      return '<li class="' + cls + (skipped ? " skipped" : "") + '"' + (i === wiz.step ? ' aria-current="step"' : "") + '><span class="num">' + (i < wiz.step && !skipped ? icon("ok", 20) : i + 1) + "</span>" + name + "</li>";
    }).join("");
  }

  function renderWizard() {
    renderSteps();
    const page = $("#wiz-page");
    const back = $("#wiz-back");
    const next = $("#wiz-next");
    const p = providerOf(wiz.provider);
    const c = esc(C());
    // Back from the first screen leaves setup, when there is somewhere to go.
    const firstScreen = wiz.step === (wiz.picked ? 1 : 0);
    back.textContent = firstScreen && !wiz.first ? "Cancel" : "Back";
    back.disabled = firstScreen && wiz.first;
    next.textContent = wiz.step === 4 ? "Start syncing" : "Next";
    next.disabled = wiz.busy;
    next.hidden = wiz.step === 0;
    setText($("#wiz-footer-note"), wiz.error || "");
    $("#wiz-footer-note").className = "small " + (wiz.error ? "err" : "muted");

    if (wiz.step === 0) {
      page.innerHTML =
        "<header><h1>Choose your cloud provider</h1><p class=\"muted\">" +
        (wiz.first ? "Pick the service your files are with. You can add more accounts later, with the same service or another." : "Pick the service for the new account.") +
        "</p></header>" +
        '<ul class="panel rows pick" id="wz-providers">' + providerRows() + "</ul>" +
        '<p id="wz-engine" class="tiny muted" role="status"><span id="wz-engine-text"></span><button type="button" class="link" id="wz-engine-retry" hidden>Try again</button></p>';
      onProviderPick($("#wz-providers"), (id) => {
        wiz.provider = id;
        wiz.step = 1;
        wiz.error = "";
        compose();
        renderWizard();
      });
      $("#wz-engine-retry").addEventListener("click", () => run("retry_engine"));
      wizardTick();
    } else if (wiz.step === 1) {
      const browser = p.sign_in === "browser";
      const lead =
        p.sign_in === "password"
          ? c + " gives other apps their own password, so your normal password is never used here."
          : p.sign_in === "keys"
            ? keysWhy(c)
            : "";
      page.innerHTML =
        "<header><h1>Connect your " + c + " account</h1>" + (lead ? '<p class="muted">' + lead + "</p>" : "") + "</header>" +
        (S.test_mode ? banner("info", "Test mode: this build uses a folder on this PC in place of " + c + ". Any sign-in works.") : "") +
        signInFields(p, wiz) +
        '<div class="row gap12"><button type="button" class="btn" id="wz-test">' + (browser ? "Sign in to " + c : "Test connection") + '</button><span id="wz-result" class="grow" role="status"></span></div>' +
        '<p id="wz-engine" class="tiny muted" role="status"><span id="wz-engine-text"></span><button type="button" class="link" id="wz-engine-retry" hidden>Try again</button></p>' +
        '<p class="tiny muted">' +
        (p.sign_in === "keys"
          ? keysKept(c, false)
          : (browser ? "What " + c + " gives SyncHopper to sign in with" : "The sign-in") +
            " is saved on this PC only, " + (RAW.lock_signins ? "locked so that only your Windows account can read it." : "scrambled the way rclone stores it.") +
            (p.sign_in === "password" ? " You can cancel the app password any time on the same " + c + " page." : "")) +
        "</p>" +
        (p.asks_drive
          ? '<p class="tiny muted">SyncHopper works in your ' + c + "'s Private folder. " + c + " keeps the old version each time a file is replaced, and old versions count against your storage. You can clear them out on " + c + "'s website.</p>"
          : "");
      const typed = (key) => (e) => {
        wiz[key] = e.target.value;
        wiz.result = null;
        showResult();
      };
      if ($("#wz-email")) $("#wz-email").addEventListener("input", typed("email"));
      if ($("#wz-pass")) $("#wz-pass").addEventListener("input", typed("password"));
      if ($("#wz-code")) $("#wz-code").addEventListener("input", typed("code"));
      if ($("#wz-kdrive")) $("#wz-kdrive").addEventListener("input", typed("kdrive"));
      for (const id of ["#wz-pass", "#wz-code", "#wz-kdrive"]) {
        if ($(id))
          $(id).addEventListener("keydown", (e) => {
            if (e.key === "Enter") testConnection();
          });
      }
      $("#wz-test").addEventListener("click", testConnection);
      $("#wz-engine-retry").addEventListener("click", () => run("retry_engine"));
      showResult();
      wizardTick();
    } else if (wiz.step === 2) {
      page.innerHTML =
        "<header><h1>Choose what to turn on</h1><p class=\"muted\">Every one of these can be changed later in settings.</p></header>" +
        '<div class="panel">' +
        '<div class="check"><input id="wz-sync" type="checkbox"' + (wiz.sync ? " checked" : "") + '><label for="wz-sync"><span>Synced folders</span><span class="small muted">Keep chosen ' + c + " folders on this PC. Changes on either side are copied to the other.</span></label></div>" +
        '<div class="check"><input id="wz-drive" type="checkbox"' + (wiz.drive ? " checked" : "") + (S.windows ? "" : " disabled") + '><label for="wz-drive"><span>' + c + ' drive</span><span class="small muted">' +
        (S.windows ? "Browse your whole account as a drive letter without keeping the files on this PC. Installs a small driver called WinFsp, and Windows asks for permission." : "Browse your whole account as a drive letter. Only available on Windows.") +
        "</span></label></div>" +
        (wiz.first ? '<div class="check"><input id="wz-auto" type="checkbox"' + (wiz.auto ? " checked" : "") + '><label for="wz-auto"><span>Start with Windows</span><span class="small muted">Open quietly in the tray when you sign in, so syncing is always on.</span></label></div>' : "") +
        "</div>" +
        '<p class="small muted">Upload is always there: a button in the main window for sending files straight to ' + c + " without keeping them in sync.</p>";
      $("#wz-sync").addEventListener("change", (e) => {
        wiz.sync = e.target.checked;
        renderSteps();
      });
      $("#wz-drive").addEventListener("change", (e) => {
        wiz.drive = e.target.checked;
      });
      if ($("#wz-auto"))
        $("#wz-auto").addEventListener("change", (e) => {
          wiz.auto = e.target.checked;
        });
    } else if (wiz.step === 3) {
      let listHtml;
      if (wiz.loading) listHtml = '<p class="small muted list-note">Loading your ' + c + " folders</p>";
      else if (wiz.foldersError) listHtml = '<p class="err list-note">' + esc(wiz.foldersError) + ' <button type="button" class="link" id="wz-reload">Try again</button></p>';
      else if (wiz.folders && !wiz.folders.length) listHtml = '<p class="small muted list-note">Your ' + c + " account has no folders yet. You can add some later with Add folder.</p>";
      else
        listHtml =
          '<ul class="list">' +
          (wiz.folders || [])
            .map((name, i) => {
              const on = !!wiz.chosen[name];
              return '<li><input id="wz-f' + i + '" type="checkbox" data-folder="' + esc(name) + '"' + (on ? " checked" : "") + '><label for="wz-f' + i + '" class="grow">' + esc(name) + '</label><span class="' + (on ? "tag-on" : "tag-off") + '">' + (on ? "On this PC" : "Online only") + "</span></li>";
            })
            .join("") +
          "</ul>";
      const names = (wiz.folders || []).filter((n) => wiz.chosen[n]);
      const others = (wiz.folders || []).length - names.length;
      page.innerHTML =
        "<header><h1>Pick folders to keep on this PC</h1><p class=\"muted\">Ticked folders are copied to this PC and stay in sync. Everything else stays online only.</p></header>" +
        '<div class="field"><label for="wz-base">Where to keep them</label><div class="row gap8"><input id="wz-base" type="text" class="grow" autocomplete="off" spellcheck="false" value="' + esc(wiz.base) + '"><button type="button" class="btn" id="wz-browse">Browse</button></div></div>' +
        '<div role="group" aria-label="' + c + ' folders" class="box">' + listHtml + "</div>" +
        '<p class="small muted" role="status">' +
        (wiz.folders ? plural(names.length, "folder") + " will be kept on this PC. " + plural(others, "folder stays", "folders stay") + " online only. You can add more later, including a single folder from deeper inside your account." : "") +
        "</p>";
      $("#wz-base").addEventListener("input", (e) => {
        wiz.base = e.target.value;
      });
      $("#wz-browse").addEventListener("click", async () => {
        const picked = await run("pick_folder", { start: null });
        if (picked) {
          wiz.base = picked;
          renderWizard();
        }
      });
      const reload = $("#wz-reload");
      if (reload) reload.addEventListener("click", loadWizardFolders);
      $$("[data-folder]", page).forEach((box) =>
        box.addEventListener("change", () => {
          wiz.chosen[box.dataset.folder] = box.checked;
          const y = page.scrollTop;
          const focusId = box.id;
          renderWizard();
          $("#wiz-page").scrollTop = y;
          const again = document.getElementById(focusId);
          if (again) again.focus();
        })
      );
    } else {
      const feats = [];
      if (wiz.sync) feats.push("Synced folders");
      if (wiz.drive) feats.push(C() + " drive");
      if (wiz.first && wiz.auto) feats.push("Start with Windows");
      const plan = wizardPlan();
      let planHtml = "";
      if (wiz.sync) {
        const rows = [];
        plan.add.forEach((name) => {
          const pv = wiz.previews[name];
          let what = "Counting files";
          if (pv && pv.error) what = "Could not count the files";
          else if (pv && pv.data) what = C() + " has " + plural(pv.data.remote_count, "file") + " (" + fmtBytes(pv.data.remote_bytes) + "), this PC has " + num(pv.data.local_count);
          rows.push("<li><span class=\"strong\">" + esc(name) + '</span><span class="what">' + esc(what) + "</span></li>");
        });
        planHtml =
          "<h2>What the first sync will do</h2>" +
          (rows.length ? '<ul class="panel plan">' + rows.join("") + "</ul>" : '<p class="small muted">No folders were picked, so nothing is copied yet. Use Add folder in the main window when you are ready.</p>') +
          (plan.add.length ? '<p class="small muted">Files missing on either side are copied across. If the same file is in both places, the newer one is kept.</p>' : "") +
          spaceWarning(plan);
      }
      page.innerHTML =
        "<header><h1>Check before the first sync</h1><p class=\"muted\">Nothing is copied or changed until you press Start syncing.</p></header>" +
        '<dl class="summary"><dt>Account</dt><dd>' + esc([C(), S.account.email].filter(Boolean).join(", ")) + "</dd><dt>Features</dt><dd>" + esc(feats.join(", ") || "Upload to " + C() + " only") + "</dd>" +
        (wiz.sync ? "<dt>Kept in</dt><dd>" + esc(wiz.base) + "</dd>" : "") +
        "</dl>" +
        planHtml +
        (wiz.drive && S.drive.state !== "connected" ? '<p class="small muted">The ' + c + " drive needs the WinFsp driver. If it is not installed yet, Windows asks for permission to install it right after this step.</p>" : "") +
        "<p>" + statusLine("ok", "ok", "No files will be deleted on this PC or in " + C() + ".") + "</p>";
    }
  }

  // A new account has no synced folders yet, so everything ticked is added.
  function wizardPlan() {
    const listed = wiz.folders || [];
    return { add: listed.filter((n) => wiz.chosen[n]) };
  }

  function spaceWarning(plan) {
    let need = 0;
    let free = null;
    plan.add.forEach((name) => {
      const pv = wiz.previews[name];
      if (pv && pv.data) {
        need += pv.data.remote_bytes;
        if (pv.data.free_bytes != null) free = pv.data.free_bytes;
      }
    });
    if (free != null && need > free) {
      return banner("warn", "These folders hold " + fmtBytes(need) + " in " + esc(C()) + ", but this drive only has " + fmtBytes(free) + " free. Go back and untick some, or pick a drive with more room.");
    }
    return "";
  }

  function showResult() {
    const el = $("#wz-result");
    if (!el || !wiz) return;
    const browser = providerOf(wiz.provider).sign_in === "browser";
    let html = "";
    if (wiz.testing && browser) {
      // The account that is waiting on its sign-in page carries the link.
      const waiting = (RAW.accounts || []).find((a) => a.signing_in);
      html = browserWaitHtml(waiting && waiting.signing_in ? waiting.signing_in.link : null);
    } else if (wiz.testing) html = statusLine("busy", "sync", "Checking with " + C());
    else if (wiz.result) html = statusLine(wiz.result.ok ? "ok" : "warn", wiz.result.ok ? "ok" : "warn", wiz.result.text);
    setHtml(el, html);
    const btn = $("#wz-test");
    if (btn) btn.disabled = wiz.testing;
  }

  // Signs in with what is on the screen. On success the account exists and
  // the rest of setup works on it.
  async function testConnection() {
    if (wiz.testing) return false;
    if (S.engine.state !== "ready") {
      wiz.result = { ok: false, text: "The sync engine is not ready yet. Give it a moment and try again." };
      showResult();
      return false;
    }
    const mine = wiz;
    wiz.testing = true;
    wiz.result = null;
    showResult();
    try {
      const done = await invoke("add_account", { provider: wiz.provider, signin: { email: wiz.email, password: wiz.password, code: wiz.code, drive: wiz.kdrive } });
      if (wiz !== mine) {
        // Setup was left while this was under way.
        run("remove_account", { account: done.account });
        return false;
      }
      wiz.account = done.account;
      // Not needed any more. Filen's especially is never kept.
      wiz.password = "";
      wiz.code = "";
      for (const id of ["#wz-pass", "#wz-code"]) if ($(id)) $(id).value = "";
      let text = "Connected.";
      if (done.about.used != null && done.about.total != null) text += " " + fmtBytes(done.about.used) + " of " + fmtBytes(done.about.total) + " used.";
      wiz.result = { ok: true, text: text };
      wiz.folders = null; // a different account may have different folders
      wiz.base = "";
    } catch (e) {
      if (wiz !== mine) return false;
      wiz.account = null;
      wiz.result = { ok: false, text: String(e) };
      if (String(e).includes(CODE_WORDS)) {
        wiz.needsCode = true;
        const field = $("#wz-code-field");
        if (field) field.hidden = false;
      }
    }
    wiz.testing = false;
    await refresh();
    if (wiz !== mine) return false;
    showResult();
    if (!wiz.result.ok && wiz.needsCode && $("#wz-code")) $("#wz-code").focus();
    return wiz.result.ok;
  }

  async function loadWizardFolders() {
    wiz.loading = true;
    wiz.foldersError = "";
    renderWizard();
    try {
      wiz.folders = await invoke("list_remote", { path: "" });
    } catch (e) {
      wiz.folders = null;
      wiz.foldersError = "Could not list your " + C() + " folders. " + e;
    }
    wiz.loading = false;
    if (view === "wizard" && wiz.step === 3) renderWizard();
  }

  function loadWizardPreviews() {
    wizardPlan().add.forEach(async (name) => {
      if (wiz.previews[name]) return;
      wiz.previews[name] = { loading: true };
      const mine = wiz;
      try {
        const data = await invoke("preview_pair", { local: joinLocal(wiz.base, name), remote: name });
        mine.previews[name] = { data: data };
      } catch (e) {
        mine.previews[name] = { error: String(e) };
      }
      if (wiz === mine && view === "wizard" && wiz.step === 4) renderWizard();
    });
  }

  $("#wiz-back").addEventListener("click", async () => {
    if (!wiz || wiz.busy) return;
    wiz.error = "";
    if (wiz.step === (wiz.picked ? 1 : 0)) {
      if (!wiz.first) leaveWizard();
      return;
    }
    if (wiz.step === 1) {
      // Back to picking the service: a sign-in made so far is dropped.
      run("cancel_sign_in");
      const acct = wiz.account;
      wiz.account = null;
      wiz.result = null;
      wiz.testing = false;
      wiz.provider = null;
      wiz.step = 0;
      if (acct) await run("remove_account", { account: acct });
      compose();
      renderWizard();
      return;
    }
    wiz.step -= wiz.step === 4 && !wiz.sync ? 2 : 1;
    renderWizard();
  });

  $("#wiz-next").addEventListener("click", async () => {
    if (!wiz || wiz.busy) return;
    wiz.error = "";
    if (wiz.step === 0) {
      return;
    } else if (wiz.step === 1) {
      if (!connected()) {
        wiz.busy = true;
        $("#wiz-next").disabled = true;
        const ok = await testConnection();
        if (!wiz) return;
        wiz.busy = false;
        $("#wiz-next").disabled = false;
        if (!ok) return;
      }
      if (S.engine.state !== "ready") {
        wiz.error = "The sync engine is still getting ready.";
        renderWizard();
        return;
      }
      wiz.step = 2;
    } else if (wiz.step === 2) {
      wiz.step = wiz.sync ? 3 : 4;
      if (wiz.sync) {
        if (!wiz.base) wiz.base = (await invoke("default_base_folder").catch(() => "")) || "";
        if (!wiz.folders && !wiz.loading) {
          renderWizard();
          loadWizardFolders();
          return;
        }
      }
    } else if (wiz.step === 3) {
      const any = (wiz.folders || []).some((n) => wiz.chosen[n]);
      if (any && !wiz.base.trim()) {
        wiz.error = "Pick where to keep the folders on this PC.";
        renderWizard();
        return;
      }
      wiz.step = 4;
      wiz.previews = {};
      loadWizardPreviews();
    } else {
      wiz.busy = true;
      renderWizard();
      const listed = wiz.sync ? wiz.folders || [] : [];
      try {
        const notes = await invoke("finish_setup", {
          choices: {
            feature_sync: wiz.sync,
            feature_drive: wiz.drive,
            autostart: wiz.first ? wiz.auto : RAW.general.autostart,
            base_folder: wiz.base.trim(),
            folders: listed.filter((n) => wiz.chosen[n]),
            listed: listed,
          },
        });
        cur = wiz.account;
        wiz = null;
        await refresh();
        go("main");
        if (notes && notes.length) toast(notes.join(" "), 12000);
        return;
      } catch (e) {
        wiz.busy = false;
        wiz.error = String(e);
      }
    }
    renderWizard();
  });


  // ----- wiring up ----------------------------------------------------------

  // Buttons in index.html name their icon with data-icon. Draw it in.
  $$("[data-icon]").forEach((el) => {
    el.insertAdjacentHTML("afterbegin", icon(el.dataset.icon, Number(el.dataset.iconSize) || 16));
  });

  T.event.listen("state", (e) => onState(e.payload));
  T.event.listen("navigate", (e) => {
    if (S && S.setup_complete && view !== "wizard" && VIEWS.includes(e.payload)) go(e.payload);
  });
  T.event.listen("files-dropped", (e) => {
    $("#drop-zone").classList.remove("over");
    if (view === "upload" && !openModals.length) addUploads(e.payload);
    else if (S && S.setup_complete && view === "main") toast("To upload files, click Upload first, then drop them there.");
  });
  T.event.listen("drag-hover", (e) => {
    $("#drop-zone").classList.toggle("over", !!e.payload && view === "upload");
  });

  // Keeps "Last synced 2 minutes ago" moving along.
  setInterval(() => {
    if (S && view === "main") renderMain();
  }, 30000);

  // No right-click menu or text-drop surprises from the web view.
  window.addEventListener("contextmenu", (e) => {
    if (!e.target.closest("input, textarea, .selectable, pre")) e.preventDefault();
  });

  invoke("get_state").then(onState).catch((e) => {
    $("#view-loading").innerHTML = '<p class="err">SyncHopper could not start: ' + esc(e) + "</p>";
  });
})();
