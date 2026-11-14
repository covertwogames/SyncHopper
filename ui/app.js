// SyncHopper screens. Plain JavaScript, no build step.
// The Rust side sends one "state" object with everything to show, and the
// screens call named commands to make things happen.

(function () {
  "use strict";

  const T = window.__TAURI__;
  const invoke = (cmd, args) => T.core.invoke(cmd, args || {});
  const $ = (sel, root) => (root || document).querySelector(sel);
  const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));

  let S = null; // latest state from the app
  let view = "loading";
  let wiz = null; // setup wizard working values
  let review = null; // open review dialog: { id, kind, modal }
  const VIEWS = ["loading", "wizard", "main", "upload", "settings", "advanced"];

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

  // A dialog for walking through Koofr folders and choosing one.
  function browseKoofr(startPath, title) {
    return new Promise((resolve) => {
      let path = String(startPath || "").split("/").filter(Boolean);
      let token = 0;
      const modal = openModal({
        title: title || "Choose a Koofr folder",
        body:
          '<div class="crumbs" id="br-crumbs"></div>' +
          '<div class="browser-list"><ul class="list pick" id="br-list"></ul><p class="small muted" id="br-note" style="padding:12px 14px"></p></div>' +
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
        const parts = [{ label: "Koofr", depth: 0 }].concat(path.map((p, i) => ({ label: p, depth: i + 1 })));
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
            .map((n) => '<li tabindex="0" role="button" data-name="' + esc(n) + '"><span class="name">' + esc(n) + '</span><span class="tiny muted">Open</span></li>')
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
    closeRowMenu();
    if (name === "upload" && S) $("#up-dest").value = S.settings.upload_dest || "";
    if (name === "settings" || name === "advanced") syncControls(true);
    render();
  }

  function render() {
    if (!S) return;
    if (view === "main") renderMain();
    else if (view === "upload") renderUpload();
    else if (view === "settings" || view === "advanced") syncControls(false);
    else if (view === "wizard") wizardTick();
    updateReview();
  }

  function onState(state) {
    S = state;
    if (view === "loading") {
      if (S.setup_complete) {
        go("main");
        takeAddRequest();
      } else startWizard(false);
      return;
    }
    if (!S.setup_complete && view !== "wizard") {
      openModals.slice().forEach((m) => m.cancel());
      startWizard(false);
      return;
    }
    render();
    takeAddRequest();
  }

  // A folder sent here from outside the window, such as with "Sync folder to
  // Koofr" in File Explorer. Shown once, then handed back.
  let takingRequest = false;
  function takeAddRequest() {
    const req = S.add_request;
    if (!req || takingRequest || !S.setup_complete || view === "wizard") return;
    // An update can arrive before the request is handed back; don't open
    // the same dialog twice.
    takingRequest = true;
    run("clear_add_request").finally(() => (takingRequest = false));
    if (view !== "main") go("main");
    if (req.blocked) {
      openModal({ title: "Sync folder to Koofr", body: "<p>" + esc(req.blocked) + "</p>", buttons: [{ label: "OK", kind: "primary" }] });
      return;
    }
    addFolderDialog(req);
  }

  document.addEventListener("click", (e) => {
    const nav = e.target.closest("[data-nav]");
    if (nav) go(nav.dataset.nav);
  });

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
    const st = { chip: "plain", label: "", sub: "", bar: null, btn: "Sync now", act: "sync" };
    const hold = S.sync.paused_all ? "All syncing is paused" : S.sync.auto_pause === "battery" ? "Paused while on battery" : S.sync.auto_pause === "metered" ? "Paused on a metered connection" : "";
    const busy = p.phase === "syncing" || p.phase === "checking" || p.phase === "queued";
    if (p.paused) {
      Object.assign(st, { label: "Paused", sub: "This folder is not syncing", btn: "Resume", act: "resume" });
    } else if (p.approval && !busy) {
      const where = p.approval.side === "pc" ? "on this PC" : "in Koofr";
      Object.assign(st, {
        chip: "warn",
        label: "Needs your OK",
        sub: p.approval.everything ? "Everything here is gone " + where : num(p.approval.deleted) + " of " + num(p.approval.total) + " files were deleted " + where,
        btn: "Review",
        act: "review",
      });
    } else if (p.phase === "syncing" && p.progress) {
      const g = p.progress;
      const speed = g.speed > 0 ? fmtSpeed(g.speed) : "";
      const eta = fmtEta(g.eta);
      Object.assign(st, {
        chip: "info",
        label: g.total > 0 ? "Syncing " + num(Math.min(g.done + 1, g.total)) + " of " + plural(g.total, "file") : "Syncing",
        sub: [speed, eta].filter(Boolean).join(", ") || "Working",
        bar: g.percent,
        btn: "Pause",
        act: "pause",
      });
    } else if (busy) {
      Object.assign(st, {
        chip: "info",
        label: p.phase === "queued" ? "Waiting to sync" : "Checking for changes",
        sub: p.phase === "queued" ? "Another folder is syncing first" : "Comparing this PC with Koofr",
        btn: "Pause",
        act: "pause",
      });
    } else if (p.phase === "waiting") {
      Object.assign(st, {
        chip: "info",
        label: "Waiting for Koofr",
        sub: (p.waiting || "Koofr is having problems right now.") + " " + retryText(p.retry_at),
        btn: "Try now",
        act: "sync",
      });
    } else if (p.phase === "error") {
      Object.assign(st, { chip: "warn", label: "Needs attention", sub: p.error || "The last sync did not finish", btn: "Review", act: "review" });
    } else if (flaggedFiles(p).length) {
      const f = flaggedFiles(p);
      Object.assign(st, { chip: "warn", label: plural(f.length, "file needs", "files need") + " attention", sub: f[0].reason, btn: "Review", act: "review" });
    } else if (p.conflicts.length) {
      Object.assign(st, { chip: "warn", label: plural(p.conflicts.length, "file", "files") + " changed in both places", sub: "See which ones", btn: "Review", act: "review" });
    } else if (hold) {
      Object.assign(st, { label: "Paused", sub: hold });
    } else if (p.phase === "ok" || p.last_sync) {
      Object.assign(st, { chip: "ok", label: "Up to date", sub: [ago(p.last_sync) + ".", quietExtras(p)].filter(Boolean).join(" ") });
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

  function statusHtml(st) {
    return (
      '<span class="chip ' + st.chip + '">' + esc(st.label) + "</span>" +
      (st.bar == null ? "" : '<div class="bar" role="img" aria-label="Sync progress, ' + Math.round(st.bar) + ' percent done"><div style="width:' + Math.max(2, Math.min(100, st.bar)).toFixed(1) + '%"></div></div>') +
      '<span class="sub">' + esc(st.sub) + "</span>"
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
    let line = "Connected as " + acct.email + ".";
    if (acct.used != null && acct.total != null) line += " " + fmtBytes(acct.used) + " of " + fmtBytes(acct.total) + " used.";
    setText($("#main-account"), line);

    // Banners
    const banners = [];
    if (S.test_mode) banners.push('<div class="banner info"><span>Test mode: syncing with a folder on this PC in place of Koofr.</span></div>');
    const eng = engineText();
    if (eng) banners.push('<div class="banner"><span>' + esc(eng) + "</span>" + (engineStuck() ? '<button type="button" class="btn" data-banner="engine">Try again</button>' : "") + "</div>");
    const up = S.update || {};
    if (up.status === "available") banners.push('<div class="banner info"><span>SyncHopper ' + esc(up.version) + ' is available.</span><button type="button" class="btn" data-banner="update">Install and restart</button></div>');
    else if (up.status === "downloading") banners.push('<div class="banner info"><span>Downloading SyncHopper ' + esc(up.version) + ", " + up.percent + "% done.</span></div>");
    else if (up.status === "installing") banners.push('<div class="banner info"><span>Installing the update. SyncHopper will close and open again by itself.</span></div>');
    else if (up.status === "error" && up.version) banners.push('<div class="banner"><span>' + esc(up.message) + '</span><button type="button" class="btn" data-banner="update">Try again</button></div>');
    if (S.sync.paused_all) banners.push('<div class="banner"><span>All syncing is paused.</span><button type="button" class="btn" data-banner="resume">Resume</button></div>');
    else if (S.sync.auto_pause === "battery") banners.push('<div class="banner info"><span>Syncing is paused while this PC is on battery. Use Sync now on a folder to sync it anyway.</span></div>');
    else if (S.sync.auto_pause === "metered") banners.push('<div class="banner info"><span>Syncing is paused on this metered connection. Use Sync now on a folder to sync it anyway.</span></div>');
    (S.folder_notices || []).forEach((n) => {
      let buttons = "";
      if (n.kind === "clash" || n.kind === "not_synced" || n.kind === "failed") buttons += '<button type="button" class="btn" data-banner="fsync" data-local="' + esc(n.local) + '">Sync it</button>';
      if (n.kind === "files") buttons += '<button type="button" class="btn" data-banner="fopen">Open folder</button>';
      buttons += '<button type="button" class="btn" data-banner="fdismiss" data-id="' + esc(n.id) + '">Dismiss</button>';
      banners.push('<div class="banner info"><span>' + esc(n.message) + "</span>" + buttons + "</div>");
    });
    setHtml($("#main-banners"), banners.join(""));

    // Folder cards, updated in place so buttons stay clickable.
    const list = $("#pair-list");
    const syncOn = S.settings.feature_sync;
    const pairs = syncOn ? S.pairs : [];
    const seen = {};
    pairs.forEach((p, index) => {
      seen[p.id] = true;
      let li = list.querySelector('li[data-id="' + p.id + '"]');
      if (!li) {
        li = document.createElement("li");
        li.className = "card";
        li.dataset.id = p.id;
        li.innerHTML =
          '<div class="card-main"><span class="name"></span><span class="path selectable"></span></div>' +
          '<div class="card-status"></div>' +
          '<button type="button" class="btn main"></button>' +
          '<button type="button" class="btn icon more" aria-haspopup="menu">&#8943;</button>';
        list.appendChild(li);
      }
      if (list.children[index] !== li) list.insertBefore(li, list.children[index]);
      const st = pairStatus(p);
      setText($(".name", li), p.name);
      setText($(".path", li), p.local);
      $(".path", li).title = p.local + "   Koofr: /" + p.remote;
      setHtml($(".card-status", li), statusHtml(st));
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
      setText(empty, "Synced folders are turned off. Turn them on in Settings to keep folders on this PC.");
    } else if (!pairs.length) {
      empty.hidden = false;
      setText(empty, "No folders are kept on this PC yet. Add one and it will stay in sync with Koofr.");
    } else {
      empty.hidden = true;
    }
    $("#add-folder").hidden = !syncOn;
    setText(
      $("#main-footnote"),
      syncOn ? "Checks Koofr for changes every " + plural(S.settings.check_interval_min, "minute") + ". Local changes sync right away." : ""
    );
    renderDrive();
  }

  function renderDrive() {
    const card = $("#drive-card");
    const d = S.drive;
    card.hidden = !d.enabled;
    if (!d.enabled) return;
    const m = {
      off: ["plain", "Not connected", "The drive is turned on but not connected yet.", "Connect", "retry"],
      needs_driver: ["warn", "Driver needed", d.message || "The drive needs a small driver called WinFsp.", "Install driver", "install"],
      installing: ["info", "Installing driver", "Windows may ask for permission. This can take a minute.", "", ""],
      starting: ["info", "Connecting", "Setting up drive " + d.letter + ":", "", ""],
      connected: ["ok", "Connected", "Your whole Koofr account, online only. Files download when you open them.", "Open", "open"],
      error: ["warn", "Not connected", d.message || "The drive could not connect.", "Try again", "retry"],
    }[d.state] || ["plain", d.state, "", "", ""];
    const html =
      '<div class="card-main"><span class="name">Koofr drive (' + esc(d.letter) + ':)</span><span class="path">' + esc(m[2]) + "</span></div>" +
      '<div class="card-status"><span class="chip ' + m[0] + '">' + esc(m[1]) + "</span>" +
      (d.state === "needs_driver" ? '<button type="button" class="link small" data-drive="page" style="align-self:flex-start">Download the driver yourself</button>' : "") +
      "</div>" +
      (m[3] ? '<button type="button" class="btn main" data-drive="' + m[4] + '">' + esc(m[3]) + "</button>" : '<span style="width:104px"></span>') +
      '<span style="width:44px"></span>';
    setHtml(card, html);
  }

  $("#main-banners").addEventListener("click", (e) => {
    const b = e.target.closest("[data-banner]");
    if (!b) return;
    if (b.dataset.banner === "resume") run("set_paused_all", { paused: false });
    if (b.dataset.banner === "engine") run("retry_engine");
    if (b.dataset.banner === "update") installUpdate();
    if (b.dataset.banner === "fsync") run("request_add_folder", { folder: b.dataset.local });
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
      "This rebuilds the folder's sync record by comparing this PC with Koofr again. Files missing on either side are copied across and nothing is deleted. A file you deleted very recently can come back once.",
      "Repair"
    );
    if (ok && (await run("pair_action", { id: p.id, action: "repair" })) !== undefined) toast("Rebuilding the sync record for " + p.name + ".");
  }

  async function removePair(p) {
    const ok = await confirmDialog(
      "Stop syncing " + p.name + "?",
      "The files stay where they are, on this PC and in Koofr. They just stop being kept in step. You can add the folder again later.",
      "Stop syncing",
      true
    );
    if (ok) run("remove_pair", { id: p.id });
  }

  // ----- review dialog ----------------------------------------------------

  function reviewKind(p, wanted) {
    const busy = p.phase === "syncing" || p.phase === "checking" || p.phase === "queued";
    if (p.approval && !busy) return (p.approval.everything ? "empty-" : "deletes-") + p.approval.side;
    if (busy && p.approval) return "working";
    if (p.phase === "error") return "error";
    if (p.phase === "waiting" && wanted !== "files") return "waiting";
    if ((p.files || []).length && (wanted === "files" || flaggedFiles(p).length)) return "skipped";
    if (p.conflicts.length) return "conflicts";
    return busy ? "working" : "fine";
  }

  function fileItem(f) {
    let retry;
    if (f.kind === "permanent") retry = "SyncHopper won't try this one again by itself.";
    else if (f.next_retry) {
      const secs = f.next_retry - Math.floor(Date.now() / 1000);
      retry = secs <= 60 ? "It will be tried again at the next sync." : "It will be tried again in about " + plural(Math.max(1, Math.round(secs / 60)), "minute") + ".";
    } else retry = "It will be tried again at the next sync.";
    return (
      "<li><b>" + esc(f.path) + '</b><br><span class="small muted">' + esc(f.reason) + " " + esc(retry) + "</span>" +
      (f.detail ? '<details><summary class="small">Technical details</summary><pre>' + esc(f.detail) + "</pre></details>" : "") +
      (f.can_ignore ? '<div style="margin-top:6px"><button type="button" class="btn small-btn" data-ignore="' + esc(f.path) + '">Ignore this file</button></div>' : "") +
      "</li>"
    );
  }

  function reviewBody(p, kind) {
    const out = [];
    const a = p.approval;
    if (kind === "deletes-pc") {
      out.push("<p><b>" + num(a.deleted) + " of " + num(a.total) + " files</b> in this folder were deleted on this PC.</p>");
      out.push("<p>If that was on purpose, SyncHopper can delete them from Koofr too. If not, it can copy them back to this PC.</p>");
    } else if (kind === "deletes-koofr") {
      out.push("<p><b>" + num(a.deleted) + " of " + num(a.total) + " files</b> in this folder were deleted in Koofr.</p>");
      out.push("<p>If that was on purpose, SyncHopper can delete them from this PC too. Files deleted this way do not go to the Recycle Bin. If not, it can copy them back to Koofr.</p>");
    } else if (kind === "empty-pc") {
      out.push("<p>Everything in this folder is gone from this PC. Koofr still has <b>" + plural(a.deleted, "file") + "</b> from it.</p>");
      out.push("<p>SyncHopper never empties a whole folder by itself. You can copy the files back to this PC. If you really want the folder empty, delete the files in Koofr yourself and syncing will carry on.</p>");
    } else if (kind === "empty-koofr") {
      out.push("<p>Everything in this folder is gone from Koofr, or the folder was renamed or deleted there. This PC still has <b>" + plural(a.deleted, "file") + "</b> from it.</p>");
      out.push("<p>SyncHopper never empties a whole folder by itself. You can copy the files back to Koofr. If you really want the folder empty, delete the files on this PC yourself and syncing will carry on.</p>");
    } else if (kind === "waiting") {
      out.push("<p><b>" + esc(p.waiting || "Koofr is having problems right now.") + "</b></p>");
      out.push("<p>Nothing is wrong with your files. SyncHopper tries again by itself and catches up once Koofr is back. " + esc(retryText(p.retry_at)) + "</p>");
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
      out.push("<p>" + (p.conflicts.length === 1 ? "This file was" : "These files were") + " changed on this PC and in Koofr before they could sync:</p>");
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
      const other = kind === "deletes-pc" ? "Koofr" : "this PC";
      const here = kind === "deletes-pc" ? "this PC" : "Koofr";
      return [
        { label: "Decide later", left: true },
        { label: "Copy them back to " + here, action: act("restore") },
        { label: "Delete them from " + other + " too", kind: "danger", action: act("approve") },
      ];
    }
    if (kind === "empty-pc" || kind === "empty-koofr") {
      const here = kind === "empty-pc" ? "this PC" : "Koofr";
      return [
        { label: "Decide later", left: true },
        { label: "Stop syncing this folder", action: () => { review = null; return run("remove_pair", { id: id }); } },
        { label: "Copy them back to " + here, kind: "primary", action: act("restore") },
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
    if (kind === "error") return p.name + " could not sync";
    if (kind === "waiting") return p.name + " is waiting for Koofr";
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
    const entry = { id: id, kind: kind, wanted: wanted, modal: null };
    entry.modal = openModal({
      width: 640,
      title: reviewTitle(p, kind),
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
      const ok = await confirmDialog(
        "Ignore " + lastPart(path) + "?",
        "SyncHopper stops trying to sync this file and stops warning you about it. The file stays where it is. You can sync it again from the folder's menu under Ignored files.",
        "Ignore this file"
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
        list.map((path) => '<li class="row" style="justify-content:space-between;gap:12px"><b class="selectable">' + esc(path) + '</b><button type="button" class="btn small-btn" data-unignore="' + esc(path) + '">Sync it again</button></li>').join("") +
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
    if (kind !== review.kind) {
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
  // remote (the suggested Koofr folder) and an optional note.
  async function addFolderDialog(prefill) {
    const base = await invoke("default_base_folder").catch(() => "");
    let localTouched = !!(prefill && prefill.local);
    let token = 0;
    const modal = openModal({
      title: "Add a folder",
      width: 600,
      body:
        '<p class="muted small">Pick a folder in Koofr and where to keep it on this PC. The two are then kept the same.</p>' +
        '<div class="field"><label for="af-remote">Koofr folder</label><div class="row gap8"><input id="af-remote" type="text" class="grow" autocomplete="off" spellcheck="false" placeholder="For example Documents"><button type="button" class="btn" id="af-rbrowse">Browse</button></div></div>' +
        '<div class="field"><label for="af-local">Folder on this PC</label><div class="row gap8"><input id="af-local" type="text" class="grow" autocomplete="off" spellcheck="false"><button type="button" class="btn" id="af-lbrowse">Browse</button></div></div>' +
        (prefill && prefill.note ? '<p class="banner info"><span>' + esc(prefill.note) + "</span></p>" : "") +
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
            if (!remote && !(await confirmDialog("Sync your whole Koofr account?", "Leaving the Koofr folder empty keeps your entire account on this PC. That can be a lot of data.", "Sync everything"))) return false;
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
      const picked = await browseKoofr(remoteEl.value.trim(), "Choose a Koofr folder to sync");
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
    let text = "Koofr has " + plural(pv.remote_count, "file") + " here (" + fmtBytes(pv.remote_bytes) + "). The PC folder has " + plural(pv.local_count, "file") + (pv.local_count ? " (" + fmtBytes(pv.local_bytes) + ")" : "") + ".";
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
      $(".name", li).title = u.path + "  to  Koofr: /" + u.dest;
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
    const picked = await browseKoofr(uploadDest(), "Choose where uploads go");
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
    setText($("#st-account"), "Connected as " + S.account.email);
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
    if (!S.windows) setText($("#st-sidebar-note"), "Only available on Windows.");
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
    const fresh = await invoke("get_state").catch(() => null);
    if (fresh) S = fresh;
    syncControls(true);
  }

  document.addEventListener("change", async (e) => {
    const el = e.target.closest("[data-set]");
    if (!el) return;
    const key = el.dataset.set;
    let value = el.type === "checkbox" ? el.checked : el.value;
    if (el.dataset.num != null) value = Number(value);
    if (key === "feature_drive" && value === true) {
      const ok = await confirmDialog(
        "Turn on the Koofr drive?",
        "The drive shows your whole Koofr account as a drive letter without keeping the files on this PC. It needs a small free driver called WinFsp. If it is not installed yet, SyncHopper installs it now and Windows asks for permission.",
        "Turn on"
      );
      if (!ok) {
        el.checked = false;
        return;
      }
    }
    if (key === "feature_sync" && value === false) {
      const ok = await confirmDialog("Turn off synced folders?", "Syncing stops for every folder. The files stay where they are, and you can turn this back on any time.", "Turn off");
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
    const ok = await confirmDialog(
      "Disconnect from Koofr?",
      "SyncHopper stops syncing and forgets this account and its folder list. Your files stay where they are, on this PC and in Koofr. Setup starts over next.",
      "Disconnect",
      true
    );
    if (ok) await run("disconnect");
  });

  $("#st-rerun").addEventListener("click", () => startWizard(true));

  $("#ad-log").addEventListener("click", () => run("open_target", { kind: "log" }));

  $("#ad-check").addEventListener("click", async () => {
    const before = S.update ? S.update.status : "";
    await run("check_updates");
    const fresh = await invoke("get_state").catch(() => null);
    if (fresh) S = fresh;
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
    else openModal({ title: "Diagnostic info", width: 640, body: '<p class="small muted">Copying did not work, so here it is to select and copy by hand.</p><textarea rows="12" readonly style="width:100%">' + esc(text) + "</textarea>", buttons: [{ label: "Close", kind: "primary" }] });
  });

  $("#ad-repair").addEventListener("click", () => {
    if (!S.pairs.length) {
      toast("There are no synced folders to repair.");
      return;
    }
    const modal = openModal({
      title: "Repair a folder",
      body: '<p class="small muted">Pick the folder to repair.</p><ul class="list pick">' + S.pairs.map((p) => '<li tabindex="0" role="button" data-id="' + esc(p.id) + '"><span class="name">' + esc(p.name) + '</span><span class="tiny muted">' + esc(p.local) + "</span></li>").join("") + "</ul>",
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
        '<textarea id="ig-text" rows="9" spellcheck="false" aria-label="Ignore patterns, one per line" style="width:100%">' + esc(S.settings.ignore_patterns.join("\n")) + "</textarea>" +
        '<p class="small muted">Saving a changed list makes every folder compare itself with Koofr again, which can take a while for big folders.</p>',
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
    const ok = await confirmDialog("Restore the advanced defaults?", "Everything on this screen goes back to how it started. Your account, folders and the basic settings are not touched.", "Restore defaults");
    if (!ok) return;
    await run("restore_defaults");
    const fresh = await invoke("get_state").catch(() => null);
    if (fresh) S = fresh;
    syncControls(true);
  });

  // ----- setup wizard -------------------------------------------------------

  const STEPS = ["Connect", "Features", "Folders", "Review"];

  async function startWizard(rerun) {
    wiz = {
      rerun: rerun,
      step: 0,
      email: "",
      password: "",
      testing: false,
      result: null, // { ok, text }
      sync: S.settings.feature_sync,
      drive: S.settings.feature_drive && S.windows,
      auto: S.settings.autostart,
      base: "",
      folders: null,
      foldersError: "",
      loading: false,
      chosen: {},
      previews: {},
      busy: false,
      error: "",
    };
    S.pairs.forEach((p) => {
      wiz.chosen[p.remote] = true;
    });
    go("wizard");
    renderWizard();
    wiz.base = (await invoke("default_base_folder").catch(() => "")) || "";
    if (view === "wizard" && wiz.step === 2) renderWizard();
  }

  function connected() {
    // A saved email means a working connection was saved with it.
    return !!S.account.email;
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
  }

  function renderSteps() {
    $("#wiz-steps").innerHTML = STEPS.map((name, i) => {
      const skipped = i === 2 && !wiz.sync && wiz.step !== 2;
      const cls = i === wiz.step ? "current" : i < wiz.step ? "done" : "";
      return '<li class="' + cls + (skipped ? " skipped" : "") + '"' + (i === wiz.step ? ' aria-current="step"' : "") + '><span class="num">' + (i < wiz.step && !skipped ? "&#10003;" : i + 1) + "</span>" + name + "</li>";
    }).join("");
  }

  function renderWizard() {
    renderSteps();
    const page = $("#wiz-page");
    const back = $("#wiz-back");
    const next = $("#wiz-next");
    back.textContent = wiz.step === 0 && wiz.rerun ? "Cancel" : "Back";
    back.disabled = wiz.step === 0 && !wiz.rerun;
    next.textContent = wiz.step === 3 ? (wiz.rerun ? "Save changes" : "Start syncing") : "Next";
    next.disabled = wiz.busy;
    setText($("#wiz-footer-note"), wiz.error || "");
    $("#wiz-footer-note").className = "small " + (wiz.error ? "err" : "muted");

    if (wiz.step === 0) {
      const keep = !!S.account.email;
      page.innerHTML =
        "<header><h1>Connect your Koofr account</h1><p class=\"muted\">Koofr gives other apps their own password, so your normal password is never used here.</p></header>" +
        (S.test_mode ? '<div class="banner info"><span>Test mode: this build uses a folder on this PC in place of Koofr. Any email works and the password is ignored.</span></div>' : "") +
        (keep ? '<p><span class="chip ok big">Connected as ' + esc(S.account.email) + "</span></p><p class=\"small muted\">Leave the boxes empty to keep this account, or fill them in to switch to another one.</p>" : "") +
        '<div class="field"><label for="wz-email">Koofr email</label><input id="wz-email" type="email" autocomplete="off" spellcheck="false" value="' + esc(wiz.email) + '"></div>' +
        '<div class="field"><label for="wz-pass">App password</label><input id="wz-pass" type="password" autocomplete="off" value="' + esc(wiz.password) + '">' +
        '<p class="small muted">No app password yet? <button type="button" class="link" id="wz-koofr">Open Koofr\'s app password page</button>, create one, and paste it here.</p></div>' +
        '<div class="row" style="gap:12px"><button type="button" class="btn" id="wz-test">Test connection</button><span id="wz-result" role="status"></span></div>' +
        '<p id="wz-engine" class="tiny muted" role="status"><span id="wz-engine-text"></span><button type="button" class="link" id="wz-engine-retry" hidden>Try again</button></p>' +
        '<p class="tiny muted">The app password is saved on this PC only, scrambled the way rclone stores it. You can cancel it any time on the same Koofr page.</p>';
      $("#wz-email").addEventListener("input", (e) => {
        wiz.email = e.target.value;
        wiz.result = null;
        showResult();
      });
      $("#wz-pass").addEventListener("input", (e) => {
        wiz.password = e.target.value;
        wiz.result = null;
        showResult();
      });
      $("#wz-pass").addEventListener("keydown", (e) => {
        if (e.key === "Enter") testConnection();
      });
      $("#wz-koofr").addEventListener("click", () => run("open_target", { kind: "koofr_password_page" }));
      $("#wz-test").addEventListener("click", testConnection);
      $("#wz-engine-retry").addEventListener("click", () => run("retry_engine"));
      showResult();
      wizardTick();
    } else if (wiz.step === 1) {
      page.innerHTML =
        "<header><h1>Choose what to turn on</h1><p class=\"muted\">Every one of these can be changed later in Settings.</p></header>" +
        '<div class="check"><input id="wz-sync" type="checkbox"' + (wiz.sync ? " checked" : "") + '><label for="wz-sync"><span>Synced folders</span><span class="small muted">Keep chosen Koofr folders on this PC. Changes on either side are copied to the other.</span></label></div>' +
        '<div class="check"><input id="wz-drive" type="checkbox"' + (wiz.drive ? " checked" : "") + (S.windows ? "" : " disabled") + '><label for="wz-drive"><span>Koofr drive</span><span class="small muted">' +
        (S.windows ? "Browse your whole account as a drive letter without keeping the files on this PC. Installs a small driver called WinFsp, and Windows asks for permission." : "Browse your whole account as a drive letter. Only available on Windows.") +
        "</span></label></div>" +
        '<div class="check"><input id="wz-auto" type="checkbox"' + (wiz.auto ? " checked" : "") + '><label for="wz-auto"><span>Start with Windows</span><span class="small muted">Open quietly in the tray when you sign in, so syncing is always on.</span></label></div>' +
        '<p class="small muted">Upload to Koofr is always there: a button in the main window for sending files straight to Koofr without keeping them in sync.</p>';
      $("#wz-sync").addEventListener("change", (e) => {
        wiz.sync = e.target.checked;
        renderSteps();
      });
      $("#wz-drive").addEventListener("change", (e) => {
        wiz.drive = e.target.checked;
      });
      $("#wz-auto").addEventListener("change", (e) => {
        wiz.auto = e.target.checked;
      });
    } else if (wiz.step === 2) {
      let listHtml;
      if (wiz.loading) listHtml = '<p class="small muted" style="padding:12px 14px">Loading your Koofr folders</p>';
      else if (wiz.foldersError) listHtml = '<p class="err" style="padding:12px 14px">' + esc(wiz.foldersError) + ' <button type="button" class="link" id="wz-reload">Try again</button></p>';
      else if (wiz.folders && !wiz.folders.length) listHtml = '<p class="small muted" style="padding:12px 14px">Your Koofr account has no folders yet. You can add some later with Add a folder.</p>';
      else
        listHtml =
          '<ul class="list" style="border:0">' +
          (wiz.folders || [])
            .map((name, i) => {
              const on = !!wiz.chosen[name];
              return '<li><input id="wz-f' + i + '" type="checkbox" style="margin:0" data-folder="' + esc(name) + '"' + (on ? " checked" : "") + '><label for="wz-f' + i + '" class="grow" style="font-weight:500">' + esc(name) + '</label><span class="' + (on ? "tag-on" : "tag-off") + '">' + (on ? "On this PC" : "Online only") + "</span></li>";
            })
            .join("") +
          "</ul>";
      const names = (wiz.folders || []).filter((n) => wiz.chosen[n]);
      const others = (wiz.folders || []).length - names.length;
      page.innerHTML =
        "<header><h1>Pick folders to keep on this PC</h1><p class=\"muted\">Ticked folders are copied to this PC and stay in sync. Everything else stays online only.</p></header>" +
        '<div class="field"><label for="wz-base">Where to keep them</label><div class="row gap8"><input id="wz-base" type="text" class="grow" autocomplete="off" spellcheck="false" value="' + esc(wiz.base) + '"><button type="button" class="btn" id="wz-browse">Browse</button></div></div>' +
        '<div role="group" aria-label="Koofr folders" style="border:1px solid var(--line);border-radius:10px">' + listHtml + "</div>" +
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
      if (wiz.drive) feats.push("Koofr drive");
      if (wiz.auto) feats.push("Start with Windows");
      const plan = wizardPlan();
      let planHtml = "";
      if (wiz.sync) {
        const rows = [];
        plan.add.forEach((name) => {
          const pv = wiz.previews[name];
          let what = "Counting files";
          if (pv && pv.error) what = "Could not count the files";
          else if (pv && pv.data) what = "Koofr has " + plural(pv.data.remote_count, "file") + " (" + fmtBytes(pv.data.remote_bytes) + "), this PC has " + num(pv.data.local_count);
          rows.push("<li><span class=\"strong\">" + esc(name) + '</span><span class="what">' + esc(what) + "</span></li>");
        });
        plan.keep.forEach((name) => rows.push("<li><span class=\"strong\">" + esc(name) + '</span><span class="what">Already syncing, stays as it is</span></li>'));
        plan.drop.forEach((name) => rows.push("<li><span class=\"strong\">" + esc(name) + '</span><span class="what">Stops syncing. Files stay on this PC and in Koofr.</span></li>'));
        planHtml =
          "<h2>What the first sync will do</h2>" +
          (rows.length ? '<ul class="plan">' + rows.join("") + "</ul>" : '<p class="small muted">No folders were picked, so nothing is copied yet. Use Add a folder in the main window when you are ready.</p>') +
          (plan.add.length ? '<p class="small muted">Files missing on either side are copied across. If the same file is in both places, the newer one is kept.</p>' : "") +
          spaceWarning(plan);
      }
      page.innerHTML =
        "<header><h1>Check before the first sync</h1><p class=\"muted\">Nothing is copied or changed until you press " + (wiz.rerun ? "Save changes" : "Start syncing") + ".</p></header>" +
        '<dl class="summary"><dt>Account</dt><dd>' + esc(S.account.email) + "</dd><dt>Features</dt><dd>" + esc(feats.join(", ") || "Upload to Koofr only") + "</dd>" +
        (wiz.sync ? "<dt>Kept in</dt><dd>" + esc(wiz.base) + "</dd>" : "") +
        "</dl>" +
        planHtml +
        (wiz.drive && S.drive.state !== "connected" ? '<p class="small muted">The Koofr drive needs the WinFsp driver. If it is not installed yet, Windows asks for permission to install it right after this step.</p>' : "") +
        '<p><span class="chip ok big">No files will be deleted on this PC or in Koofr.</span></p>';
    }
  }

  function wizardPlan() {
    const existing = {};
    S.pairs.forEach((p) => {
      existing[p.remote] = true;
    });
    const listed = wiz.folders || [];
    return {
      add: listed.filter((n) => wiz.chosen[n] && !existing[n]),
      keep: listed.filter((n) => wiz.chosen[n] && existing[n]),
      drop: listed.filter((n) => !wiz.chosen[n] && existing[n]),
    };
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
      return '<div class="banner"><span>These folders hold ' + fmtBytes(need) + " in Koofr, but this drive only has " + fmtBytes(free) + " free. Go back and untick some, or pick a drive with more room.</span></div>";
    }
    return "";
  }

  function showResult() {
    const el = $("#wz-result");
    if (!el) return;
    if (wiz.testing) el.innerHTML = '<span class="chip info big">Checking with Koofr</span>';
    else if (wiz.result) el.innerHTML = '<span class="chip big ' + (wiz.result.ok ? "ok" : "warn") + '">' + esc(wiz.result.text) + "</span>";
    else el.innerHTML = "";
    const btn = $("#wz-test");
    if (btn) btn.disabled = wiz.testing;
  }

  async function testConnection() {
    if (wiz.testing) return false;
    if (S.engine.state !== "ready") {
      wiz.result = { ok: false, text: "The sync engine is not ready yet. Give it a moment and try again." };
      showResult();
      return false;
    }
    wiz.testing = true;
    wiz.result = null;
    showResult();
    try {
      const about = await invoke("connect", { email: wiz.email, password: wiz.password });
      let text = "Connected.";
      if (about.used != null && about.total != null) text += " " + fmtBytes(about.used) + " of " + fmtBytes(about.total) + " used.";
      wiz.result = { ok: true, text: text };
      wiz.folders = null; // a different account may have different folders
    } catch (e) {
      wiz.result = { ok: false, text: String(e) };
    }
    wiz.testing = false;
    const fresh = await invoke("get_state").catch(() => null);
    if (fresh) S = fresh;
    showResult();
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
      wiz.foldersError = "Could not list your Koofr folders. " + e;
    }
    wiz.loading = false;
    if (view === "wizard" && wiz.step === 2) renderWizard();
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
      if (wiz === mine && view === "wizard" && wiz.step === 3) renderWizard();
    });
  }

  $("#wiz-back").addEventListener("click", () => {
    if (!wiz || wiz.busy) return;
    wiz.error = "";
    if (wiz.step === 0) {
      if (wiz.rerun) go("settings");
      return;
    }
    wiz.step -= wiz.step === 3 && !wiz.sync ? 2 : 1;
    renderWizard();
  });

  $("#wiz-next").addEventListener("click", async () => {
    if (!wiz || wiz.busy) return;
    wiz.error = "";
    if (wiz.step === 0) {
      const typed = wiz.password.trim() !== "" || wiz.email.trim() !== "";
      if (typed && !(wiz.result && wiz.result.ok)) {
        wiz.busy = true;
        $("#wiz-next").disabled = true;
        const ok = await testConnection();
        wiz.busy = false;
        $("#wiz-next").disabled = false;
        if (!ok) return;
      }
      if (S.engine.state !== "ready") {
        wiz.error = "The sync engine is still getting ready.";
        renderWizard();
        return;
      }
      if (!connected()) {
        wiz.error = "Enter your Koofr email and app password, then test the connection.";
        renderWizard();
        return;
      }
      wiz.step = 1;
    } else if (wiz.step === 1) {
      wiz.step = wiz.sync ? 2 : 3;
      if (wiz.sync && !wiz.folders && !wiz.loading) {
        renderWizard();
        loadWizardFolders();
        return;
      }
    } else if (wiz.step === 2) {
      const any = (wiz.folders || []).some((n) => wiz.chosen[n]);
      if (any && !wiz.base.trim()) {
        wiz.error = "Pick where to keep the folders on this PC.";
        renderWizard();
        return;
      }
      wiz.step = 3;
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
            autostart: wiz.auto,
            base_folder: wiz.base.trim(),
            folders: listed.filter((n) => wiz.chosen[n]),
            listed: listed,
          },
        });
        const fresh = await invoke("get_state").catch(() => null);
        if (fresh) S = fresh;
        wiz = null;
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

  T.event.listen("state", (e) => onState(e.payload));
  T.event.listen("navigate", (e) => {
    if (S && S.setup_complete && VIEWS.includes(e.payload)) go(e.payload);
  });
  T.event.listen("files-dropped", (e) => {
    $("#drop-zone").classList.remove("over");
    if (view === "upload" && !openModals.length) addUploads(e.payload);
    else if (S && S.setup_complete && view === "main") toast("To upload files, open Upload to Koofr first, then drop them there.");
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
