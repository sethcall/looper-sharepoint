// Status window. Renders the snapshot from the `get_status` command and re-fetches on "status-changed".
// Relative times are recomputed every second from the timestamps, without asking the app.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let snap = null;

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const firstLine = (s) => String(s ?? "").split("\n")[0];

function short(secs) {
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  return `${Math.floor(s / 86400)}d`;
}
const ago = (t) => (t ? `${short((Date.now() - Date.parse(t)) / 1000)} ago` : "never");
const until = (t) => {
  if (!t) return "–";
  const d = (Date.parse(t) - Date.now()) / 1000;
  return d <= 0 ? "now" : `in ${short(d)}`;
};
const clock = (t) => new Date(t).toLocaleTimeString();

function stateLabel(l, paused) {
  switch (l.state) {
    case "idle": return paused ? ["paused", "paused"] : ["up to date", "idle"];
    case "syncing": return [l.total > 0 ? `syncing ${l.done}/${l.total}` : "syncing…", "syncing"];
    case "error": return [`retrying (${l.consecutive_failures} failures)`, "error"];
    case "sign_in_required": return ["sign-in required", "sign_in_required"];
    case "refused": return ["refused: too many deletions", "refused"];
    default: return [l.state, ""];
  }
}

const glyph = { live: "●", connecting: "◌", down: "◐", unavailable: "◐", off: "○" };

function pushLabel(l) {
  const p = l.push;
  switch (p.state) {
    case "off": return "off (polling)";
    case "connecting": return p.attempt > 0 ? `reconnecting (attempt ${p.attempt + 1})` : "connecting…";
    case "live": return `live for ${short((Date.now() - Date.parse(p.since)) / 1000)}`;
    case "down":
      if (!p.retry_at) return `paused: ${p.reason}`;
      return `down ${short((Date.now() - Date.parse(p.since)) / 1000)}: ${firstLine(p.reason)} · retry ${until(p.retry_at)}`;
    case "unavailable": return `unavailable: ${firstLine(p.reason)} · retry ${until(p.retry_at)}`;
    default: return p.state;
  }
}

// The mode strip: are changes arriving live, or is the app polling (and why)?
function renderMode(st) {
  const el = $("mode");
  const m = snap.mode;
  if (!st || !m) { el.hidden = true; return; }
  el.hidden = false;
  let text = snap.mode_text;
  let detail = "";
  let cls = m.mode;
  if (m.mode === "live") text = "Live updates on · changes arrive within about 30 seconds";
  if (m.mode === "polling") detail = `Down ${short((Date.now() - Date.parse(m.since)) / 1000)}: ${esc(firstLine(m.reason))}`;
  const canReconnect = st.libraries.some((l) => (l.push.state === "down" && l.push.retry_at) || l.push.state === "unavailable");
  const icon = { live: "●", partly_live: "◐", connecting: "◌", polling: "◐", off: "○" }[m.mode] ?? "";
  el.className = `mode ${cls}`;
  el.innerHTML = `<span class="mode-icon">${icon}</span><span class="mode-text">${esc(text)}</span>`
    + (detail ? `<span class="mode-detail">${detail}</span>` : "")
    + (canReconnect ? `<button class="secondary" data-act="reconnect">Reconnect</button>` : "");
}

function describe(r) {
  if (r.error) return firstLine(r.error);
  const s = r.summary;
  if (!s) return r.outcome;
  const parts = [];
  for (const [n, what] of [[s.downloaded, "downloaded"], [s.uploaded, "uploaded"], [s.moved, "moved"], [s.moved_remote, "moved in SharePoint"], [s.deleted, "deleted"], [s.deleted_remote, "deleted in SharePoint"], [s.conflict_copies, "conflict copies"], [s.adopted, "adopted"], [s.conflicts, "conflicts"], [s.errors, "errors"]]) {
    if (n > 0) parts.push(`${n} ${what}`);
  }
  return parts.length ? parts.join(", ") : "no changes";
}

function render() {
  if (!snap) return;
  const st = snap.status;
  $("headline").textContent = snap.headline;
  $("health").className = `dot ${st ? health(st) : "attention"}`;
  $("account").textContent = st ? (st.account ? `Signed in as ${st.account}` : "Not signed in") : "";
  $("config-path").textContent = snap.config_path;
  $("stats").textContent = st ? `${st.requests} requests (${st.throttled_responses} throttled) · checks every ${short(st.poll_interval_secs)} while polling, ${short(st.push_safety_interval_secs)} while live · running ${short((Date.now() - Date.parse(st.started_at)) / 1000)}` : "";
  renderMode(st);
  $("sync-all").disabled = !st || st.libraries.length === 0;
  $("add").hidden = !(st && snap.set_up);
  $("pause").disabled = !st;
  $("pause").textContent = st && st.paused ? "Resume" : "Pause";

  // Banner: the one thing that needs attention, if any.
  const banner = $("banner");
  let html = "";
  let bad = false;
  if (!snap.set_up) {
    html = `Not set up yet. Open the setup file or link you were sent, or <button data-act="setup">Set up…</button>`;
  } else if (!st && snap.error) {
    html = `${esc(firstLine(snap.error))} <button class="secondary" data-act="config">Open config file</button>`;
    bad = true;
  } else if (snap.config_problem) {
    html = `The config file was changed but has a problem, so the previous settings are still in use: ${esc(firstLine(snap.config_problem))} <button class="secondary" data-act="config">Open config file</button>`;
    bad = true;
  } else if (st && st.libraries.length === 0) {
    html = `Nothing to sync yet. <button data-act="add">Add a SharePoint folder…</button>`;
  } else if (st && st.libraries.some((l) => l.state === "sign_in_required")) {
    html = `Your sign-in expired or was revoked. <button data-act="signin">Sign in…</button>`;
    bad = true;
  } else if (st && st.throttled_until) {
    html = `SharePoint is throttling requests. Everything resumes ${until(st.throttled_until)} (${clock(st.throttled_until)}).`;
  } else if (st && st.libraries.some((l) => l.state === "refused")) {
    const why = st.libraries.filter((l) => l.state === "refused").map((l) => esc(firstLine(l.last_failure?.error ?? ""))).join("<br>");
    html = `A sync was stopped because it would delete many files. Nothing was changed.<br><span class="muted">${why}</span><br>`
      + `If you meant it, <button data-act="allow-deletions">Allow these deletions</button> (they go to the SharePoint recycle bin).`;
    bad = true;
  }
  banner.hidden = !html;
  banner.className = `banner${bad ? " bad" : ""}`;
  banner.innerHTML = html;
  const notice = $("notice");
  notice.hidden = !snap.notice;
  notice.innerHTML = snap.notice ? `${esc(snap.notice)} <button class="secondary" data-act="dismiss">OK</button>` : "";

  // Libraries.
  $("libraries").innerHTML = (st?.libraries ?? []).map((l) => {
    const [label, cls] = stateLabel(l, st.paused);
    const issues = (l.last_run?.issues ?? []).slice(0, 5).map((i) => `<li>${esc(i.path)}: ${esc(i.reason)}</li>`).join("");
    const failure = l.last_failure ? `<span class="err">${ago(l.last_failure.at)}</span>: ${esc(firstLine(l.last_failure.error))}` : "never";
    return `<div class="lib">
      <div class="lib-top">
        <div><span class="lib-name">${esc(l.name)}</span><span class="badge ${cls}">${esc(label)}</span></div>
        <div class="actions">
          <button class="secondary" data-act="remove" data-lib="${esc(l.name)}">Stop syncing…</button>
          <button class="secondary" data-act="open" data-lib="${esc(l.name)}">Open folder</button>
          <button data-act="sync" data-lib="${esc(l.name)}">Sync</button>
        </div>
      </div>
      <div class="lib-path">${esc(l.site)} · ${esc(l.library)} → ${esc(l.local_path)}</div>
      <dl class="facts">
        <div><dt>Live updates</dt><dd class="push-${esc(l.push.state)}">${glyph[l.push.state] ?? ""} ${esc(pushLabel(l))}</dd></div>
        <div><dt>Last change notification</dt><dd>${ago(l.last_push_at)}</dd></div>
        <div><dt>Local changes</dt><dd>${!l.uploads ? "not uploaded (read-only)" : l.watching ? "uploaded within seconds" : "uploaded at the next check"}</dd></div>
        <div><dt>Last sync</dt><dd>${ago(l.last_success_at)}</dd></div>
        <div><dt>Next check</dt><dd>${st.paused ? "paused" : until(l.next_run_at)}</dd></div>
        <div><dt>Last failure</dt><dd>${failure}</dd></div>
      </dl>
      ${issues ? `<ul class="issues">${issues}</ul>` : ""}
    </div>`;
  }).join("");

  // Recent runs.
  const rows = snap.history.map((r) => `<tr>
      <td class="when" title="${esc(r.started_at)}">${clock(r.started_at)}</td>
      <td>${esc(r.library)}</td>
      <td>${esc(r.trigger)}</td>
      <td class="outcome-${esc(r.outcome)}">${esc(r.outcome)}</td>
      <td>${esc(describe(r))}</td>
      <td class="num">${((Date.parse(r.finished_at) - Date.parse(r.started_at)) / 1000).toFixed(1)}s</td>
    </tr>`).join("");
  $("runs").querySelector("tbody").innerHTML = rows;
  $("no-runs").hidden = snap.history.length > 0;
}

function health(st) {
  const libs = st.libraries;
  if (libs.some((l) => ["sign_in_required", "error", "refused"].includes(l.state))) return "attention";
  if (st.throttled_until || st.paused) return "waiting";
  if (libs.some((l) => l.state === "syncing")) return "syncing";
  return "ok";
}

async function refresh() {
  try {
    snap = await invoke("get_status");
    render();
  } catch (e) {
    console.error(e);
  }
}

async function act(what, lib) {
  try {
    if (what === "sync") await invoke("sync_now", { library: lib ?? null });
    if (what === "open") await invoke("open_folder", { library: lib });
    if (what === "config") await invoke("open_config");
    if (what === "signin") await invoke("sign_in");
    if (what === "allow-deletions") await invoke("allow_deletions", { library: lib ?? null });
    if (what === "reconnect") await invoke("reconnect", { library: lib ?? null });
    if (what === "pause") await invoke("set_paused", { paused: !snap?.status?.paused });
    if (what === "setup") return obSetup();
    if (what === "add") return obAdd();
    if (what === "remove") return obRemove(lib);
    if (what === "dismiss") await invoke("dismiss_notice");
  } catch (e) {
    alertBanner(String(e));
  }
  refresh();
}

function alertBanner(msg) {
  const b = $("banner");
  b.hidden = false;
  b.className = "banner bad";
  b.textContent = msg;
}

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-act]");
  if (el) act(el.dataset.act, el.dataset.lib);
});
$("sync-all").addEventListener("click", () => act("sync"));
$("pause").addEventListener("click", () => act("pause"));
$("config").addEventListener("click", () => act("config"));
$("add").addEventListener("click", () => act("add"));

// ---- the command line tool dialog (specs/install-cli-design.md §4.1, §6) ----

let cli = null;

async function refreshCli() {
  try {
    cli = await invoke("cli_info");
  } catch (e) {
    cli = null;
  }
  const el = $("cli-status");
  if (!cli) { el.textContent = ""; return; }
  el.textContent = cli.installed
    ? `Command line tool: ${cli.installed.link}${cli.shadowed_by ? ` (but ${cli.shadowed_by} comes first on your PATH)` : ""}`
    : "Command line tool: not installed";
}

function cliDialog({ title, body, actions, msg, bad }) {
  $("cli-title").textContent = title;
  $("cli-body").innerHTML = body;
  const m = $("cli-msg");
  m.hidden = !msg;
  m.className = `cli-msg${bad ? " bad" : ""}`;
  m.innerHTML = msg ?? "";
  $("cli-actions").innerHTML = actions.map(([id, label, secondary]) => `<button type="button" data-cli="${id}"${secondary ? ' class="secondary"' : ""}>${esc(label)}</button>`).join("");
  const d = $("cli-dialog");
  if (!d.open) d.showModal();
}

function closeCli() {
  $("cli-dialog").close();
  refreshCli();
}

async function showCli(kind) {
  await refreshCli();
  if (!cli) return;
  if (kind === "remove") return cliRemoveDialog();
  if (kind === "repair") {
    return cliDialog({
      title: "Repair the command line tool",
      body: `<p>The <code>looper-sharepoint</code> command at <code>${esc(cli.installed?.link)}</code> points at a copy of the app that's gone. Point it at this one? This asks for an administrator password.</p>`,
      actions: [["cancel", "Cancel", true], ["repair", "Repair"]],
    });
  }
  if (cli.problem) {
    return cliDialog({ title: "Install the command line tool", body: `<p>It can't be installed from here: ${esc(cli.problem)}.</p>`, actions: [["cancel", "Close"]] });
  }
  const sys = cli.admin
    ? `<label><input type="radio" name="scope" value="all" checked> For all users: <code>/usr/local/bin</code>${cli.system_needs_admin ? " (asks for an administrator password)" : ""}</label>`
    : "";
  cliDialog({
    title: "Install the command line tool",
    body: `<p>Install the <code>looper-sharepoint</code> command so you can use it in Terminal.${cli.dev ? " (This is a development build.)" : ""}</p>
      ${sys}
      <label><input type="radio" name="scope" value="me"${cli.admin ? "" : " checked"}> Just for me: <code>~/.local/bin</code></label>`,
    actions: [["cancel", "Cancel", true], ["install", "Install"]],
  });
}

function cliRemoveDialog() {
  const edit = cli.installed?.path_edit;
  cliDialog({
    title: "Remove the command line tool",
    body: `<p>Remove the <code>looper-sharepoint</code> command at <code>${esc(cli.installed?.link)}</code>?</p>`
      + (edit ? `<label><input type="checkbox" id="cli-rm-path" checked> Also remove the PATH line this app added to <code>${esc(edit)}</code></label>` : ""),
    actions: [["cancel", "Cancel", true], ["remove", "Remove"]],
  });
}

function installedMessage(r) {
  let html = r.already
    ? `Already installed at <code>${esc(r.link)}</code>.`
    : `Installed at <code>${esc(r.link)}</code>. Open a new Terminal window (open ones don't see it) and try <code>looper-sharepoint status</code>.`;
  if (r.shadowed_by) html += `<br>Note: your shell will run <code>${esc(r.shadowed_by)}</code> instead, because it comes first on your PATH.`;
  return html;
}

async function cliAct(what) {
  try {
    if (what === "cancel") return closeCli();
    if (what === "install") {
      const scope = document.querySelector('#cli-form input[name="scope"]:checked')?.value ?? "me";
      const r = await invoke("cli_install", { allUsers: scope === "all" });
      if (r.cancelled) {
        return cliDialog({
          title: "Install the command line tool",
          body: `<p>Nothing was changed. You can install it just for yourself instead, without a password.</p><input type="hidden" name="scope" value="me" checked>`,
          actions: [["cancel", "Cancel", true], ["install-me", "Just for me"]],
        });
      }
      return afterInstall(r);
    }
    if (what === "install-me") return afterInstall(await invoke("cli_install", { allUsers: false }));
    if (what === "add-path") {
      const msg = await invoke("cli_add_to_path");
      return cliDialog({ title: "Command line tool installed", body: "", msg: `${esc(msg)}. Open a new Terminal window and try <code>looper-sharepoint status</code>.`, actions: [["cancel", "Done"]] });
    }
    if (what === "remove") {
      const r = await invoke("cli_remove", { removePath: !!$("cli-rm-path")?.checked });
      if (r.cancelled) return cliDialog({ title: "Remove the command line tool", body: "", msg: "Nothing was changed.", actions: [["cancel", "Close"]] });
      let msg = r.removed ? `Removed <code>${esc(r.removed)}</code>.` : `<code>${esc(r.left)}</code> is no longer this app's link, so it was left as it is.`;
      if (r.path_removed) msg += ` Removed the PATH line from <code>${esc(r.path_removed)}</code>.`;
      return cliDialog({ title: "Command line tool removed", body: "", msg, actions: [["cancel", "Done"]] });
    }
    if (what === "repair") {
      const ok = await invoke("cli_repair");
      return cliDialog({ title: "Repair the command line tool", body: "", msg: ok ? "Repaired." : "Nothing was changed.", actions: [["cancel", "Done"]] });
    }
  } catch (e) {
    cliDialog({ title: $("cli-title").textContent, body: "", msg: esc(String(e)), bad: true, actions: [["cancel", "Close"]] });
  }
}

function afterInstall(r) {
  if (r.needs_path) {
    if (r.path_target && !r.path_unknown) {
      const where = r.path_target === "fish" ? "with fish_add_path" : `to <code>${esc(r.path_target)}</code>`;
      return cliDialog({
        title: "Command line tool installed",
        body: `<p>${installedMessage(r)}</p><p><code>~/.local/bin</code> isn't on your PATH yet, so Terminal won't find it. Add it ${where}?</p>`,
        actions: [["cancel", "Not now", true], ["add-path", "Add to my shell startup file"]],
      });
    }
    return cliDialog({
      title: "Command line tool installed",
      body: `<p>${installedMessage(r)}</p><p><code>~/.local/bin</code> may not be on your PATH. Add this line to your shell's startup file:</p><pre>${esc(r.line)}</pre>`,
      actions: [["cancel", "Done"]],
    });
  }
  cliDialog({ title: "Command line tool installed", body: "", msg: installedMessage(r), actions: [["cancel", "Done"]] });
}

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-cli]");
  if (el) { e.preventDefault(); cliAct(el.dataset.cli); }
});
listen("show-cli", (e) => showCli(e.payload));
listen("show-onboard", (e) => showOnboard(e.payload));
listen("signed-in", () => {
  if (ob.view === "setup-done") $("ob-msg").hidden = true;
  refresh();
});
if (location.hash === "#onboard") {
  history.replaceState(null, "", location.pathname);
  showOnboard(null);
  refreshCli();
} else if (location.hash.startsWith("#cli-")) {
  const kind = location.hash.slice(5);
  history.replaceState(null, "", location.pathname);
  showCli(kind);
} else {
  refreshCli();
}

// ---- setting up and adding (specs/onboarding-design.md §4, §5) ----

let ob = {};

function obDialog({ title, body, actions, msg, bad, view }) {
  ob.view = view ?? null;
  $("ob-title").textContent = title;
  $("ob-body").innerHTML = body ?? "";
  const m = $("ob-msg");
  m.hidden = !msg;
  m.className = `cli-msg${bad ? " bad" : ""}`;
  m.innerHTML = msg ?? "";
  $("ob-actions").innerHTML = actions.map(([id, label, secondary]) => `<button type="button" data-ob="${id}"${secondary ? ' class="secondary"' : ""}>${esc(label)}</button>`).join("");
  const d = $("ob-dialog");
  if (!d.open) d.showModal();
}

function obClose() {
  $("ob-dialog").close();
  ob = {};
  refresh();
}

// From the menu ({kind: "setup" | "add"}), something opened from outside ({kind, source | link}), or a new window
// (null: whatever's waiting, else the obvious next step).
async function showOnboard(p) {
  if (!p || (!p.source && !p.link)) {
    const pending = await invoke("take_pending").catch(() => null);
    if (pending) p = pending;
  }
  if (!snap) await refresh();
  const kind = p?.kind ?? (snap?.set_up ? "add" : "setup");
  if (kind === "setup") return p?.source ? obSetupPreview(p.source) : obSetup();
  return obAdd(p?.link);
}

function obSetup() {
  obDialog({
    title: "Set up Looper SharePoint",
    body: `<p>Paste the setup link you were sent (it starts with <code>looper-sharepoint://setup</code>), or choose the setup file (<code>….looper-sharepoint</code>).</p>
      <div class="row"><input type="text" id="ob-source" placeholder="looper-sharepoint://setup?…" autofocus><button type="button" class="secondary" data-ob="choose-file">Choose file…</button></div>`,
    actions: [["cancel", "Cancel", true], ["setup-next", "Continue"]],
  });
}

async function obSetupPreview(source) {
  ob.source = source;
  try {
    const p = await invoke("setup_preview", { source });
    ob.plan = p;
    const libs = p.libraries.map((l) => `<li>${esc(l.site)} · ${esc(l.library)}${l.folder ? ` · ${esc(l.folder)}` : ""} → <code>${esc(l.path)}</code></li>`).join("");
    obDialog({
      title: "Set up Looper SharePoint",
      body: `<p>This sets up Looper SharePoint for:</p>
        <dl class="plan">
          <dt>Organization</dt><dd><code>${esc(p.account.tenant)}</code></dd>
          <dt>App</dt><dd>${esc(p.account.app_name ?? "looper-sharepoint")} (<code>${esc(p.account.client_id)}</code>)</dd>
          ${libs ? `<dt>Adds</dt><dd><ul>${libs}</ul></dd>` : ""}
        </dl>
        ${p.replaces ? `<label><input type="checkbox" id="ob-replace"> Switch this computer from the organization it's set up for now (<code>${esc(p.replaces.tenant)}</code>). Your libraries stay, and you sign in again.</label>` : ""}
        <p class="muted">Then you sign in with your work account in the browser.</p>`,
      actions: [["cancel", "Cancel", true], ["setup-apply", "Set up"]],
    });
  } catch (e) {
    obDialog({ title: "Set up Looper SharePoint", msg: esc(String(e)), bad: true, actions: [["cancel", "Close", true], ["setup-start", "Try another"]] });
  }
}

async function obSetupApply() {
  if (ob.plan?.replaces && !$("ob-replace")?.checked) {
    const m = $("ob-msg");
    m.hidden = false;
    m.className = "cli-msg bad";
    m.textContent = "Tick the box to switch organizations, or cancel.";
    return;
  }
  try {
    const r = await invoke("setup_apply", { source: ob.source, replace: !!$("ob-replace")?.checked });
    const skipped = r.skipped.map((s) => `<li>${esc(s)}</li>`).join("");
    obDialog({
      view: "setup-done",
      title: "Set up",
      body: `<p>Looper SharePoint is set up${r.added.length ? `, syncing ${r.added.map(esc).join(", ")}` : ""}.</p>${skipped ? `<p>Not added:</p><ul>${skipped}</ul>` : ""}`,
      msg: r.signed_in ? null : "Sign in with your work account in the browser window that opened.",
      actions: r.added.length ? [["cancel", "Done"]] : [["cancel", "Later", true], ["add-start", "Add a SharePoint folder…"]],
    });
  } catch (e) {
    obDialog({ title: "Set up Looper SharePoint", msg: esc(String(e)), bad: true, actions: [["cancel", "Close"]] });
  }
}

function obAdd(link) {
  obDialog({
    title: "Add a SharePoint folder",
    body: `<p>In SharePoint, open the site, library or folder you want, copy the address from the browser's address bar, and paste it here. A sharing link to a folder works too.</p>
      <input type="text" id="ob-link" placeholder="https://contoso.sharepoint.com/sites/…" value="${esc(link ?? "")}" autofocus>`,
    actions: [["cancel", "Cancel", true], ["add-next", "Continue"]],
  });
  if (link) obAddPreview(link, null);
}

async function obAddPreview(link, library) {
  ob.link = link;
  ob.library = library;
  obDialog({ title: "Add a SharePoint folder", msg: "Looking it up in SharePoint…", actions: [["cancel", "Cancel", true]] });
  try {
    const p = await invoke("add_preview", { link, library });
    ob.addPlan = p;
    if (p.already) {
      const [name, path] = p.already;
      ob.openLib = name;
      return obDialog({ title: "Already syncing", body: `<p>${esc(p.describe)} already syncs to <code>${esc(path)}</code>.</p>`, actions: [["cancel", "Close", true], ["open-lib", "Open folder"]] });
    }
    const libs = p.other_libraries.length
      ? `<dt>Library</dt><dd><select id="ob-lib">${[p.library, ...p.other_libraries].map((n) => `<option${n === p.library ? " selected" : ""}>${esc(n)}</option>`).join("")}</select></dd>`
      : "";
    obDialog({
      title: "Add a SharePoint folder",
      body: `<dl class="plan"><dt>Syncs</dt><dd>${esc(p.describe)}</dd>${libs}</dl>
        <label>Local folder</label>
        <div class="row"><input type="text" id="ob-to" value="${esc(p.suggested)}"><button type="button" class="secondary" data-ob="choose-folder">Choose…</button></div>
        <p class="muted">${p.folder ? "The SharePoint folder's contents" : "The library's contents"} go straight into this folder. Choose a new or empty one.</p>
        <label><input type="checkbox" id="ob-ro"> Only download: never upload my changes</label>
        <label id="ob-adopt-row" hidden><input type="checkbox" id="ob-adopt"> Use it anyway: keep its files (files that are only here are uploaded)</label>`,
      actions: [["cancel", "Cancel", true], ["add-apply", "Add"]],
    });
  } catch (e) {
    const signIn = String(e).includes("sign in");
    obDialog({
      title: "Add a SharePoint folder",
      msg: esc(String(e)),
      bad: true,
      actions: signIn ? [["cancel", "Close", true], ["signin", "Sign in…"]] : [["cancel", "Close", true], ["add-start", "Try another link"]],
    });
  }
}

async function obAddApply() {
  const to = $("ob-to").value.trim();
  try {
    const r = await invoke("add_apply", { link: ob.link, library: ob.library, to, readOnly: $("ob-ro").checked, adopt: $("ob-adopt").checked });
    ob.openLib = r.name;
    obDialog({
      title: "Added",
      body: `<p>Syncing ${esc(ob.addPlan.describe)} into <code>${esc(r.path)}</code>. The first sync has started; from now on your changes upload within seconds and SharePoint's arrive within about 30 seconds.</p>`,
      actions: [["cancel", "Done", true], ["open-lib", "Open folder"]],
    });
  } catch (e) {
    const m = $("ob-msg");
    m.hidden = false;
    m.className = "cli-msg bad";
    m.textContent = String(e);
    if (String(e).includes("isn't empty")) $("ob-adopt-row").hidden = false;
  }
}

function obRemove(lib) {
  const l = snap?.status?.libraries.find((x) => x.name === lib);
  ob.removeLib = lib;
  obDialog({
    title: "Stop syncing",
    body: `<p>Stop syncing <b>${esc(lib)}</b>? Nothing is deleted: the files stay in <code>${esc(l?.local_path ?? "")}</code> and in SharePoint. You can add it again later.</p>`,
    actions: [["cancel", "Cancel", true], ["remove-apply", "Stop syncing"]],
  });
}

async function obAct(what) {
  try {
    if (what === "cancel") return obClose();
    if (what === "setup-start") return obSetup();
    if (what === "choose-file") {
      const f = await invoke("choose_setup_file");
      if (f) return obSetupPreview(f);
      return;
    }
    if (what === "setup-next") {
      const v = $("ob-source").value.trim();
      if (v) return obSetupPreview(v);
      return;
    }
    if (what === "setup-apply") return obSetupApply();
    if (what === "add-start") return obAdd();
    if (what === "add-next") {
      const v = $("ob-link").value.trim();
      if (v) return obAddPreview(v, null);
      return;
    }
    if (what === "choose-folder") {
      const f = await invoke("choose_folder", { start: $("ob-to").value });
      if (f) $("ob-to").value = f;
      return;
    }
    if (what === "add-apply") return obAddApply();
    if (what === "open-lib") {
      await invoke("open_folder", { library: ob.openLib });
      return obClose();
    }
    if (what === "signin") {
      await invoke("sign_in");
      return obDialog({ title: "Sign in", msg: "Sign in with your work account in the browser window that opened, then try again.", actions: [["cancel", "Close", true], ["add-start", "Try again"]] });
    }
    if (what === "remove-apply") {
      const path = await invoke("remove_library", { name: ob.removeLib });
      return obDialog({ title: "Stopped syncing", body: `<p>Nothing was deleted. The files are still in <code>${esc(path)}</code>.</p>`, actions: [["cancel", "Done"]] });
    }
  } catch (e) {
    obDialog({ title: $("ob-title").textContent, msg: esc(String(e)), bad: true, actions: [["cancel", "Close"]] });
  }
}

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-ob]");
  if (el) { e.preventDefault(); obAct(el.dataset.ob); }
});
document.addEventListener("change", (e) => {
  if (e.target.id === "ob-lib") obAddPreview(ob.link, e.target.value);
});
document.addEventListener("keydown", (e) => {
  if (e.key !== "Enter" || !$("ob-dialog").open) return;
  if (e.target.id === "ob-source") { e.preventDefault(); obAct("setup-next"); }
  if (e.target.id === "ob-link") { e.preventDefault(); obAct("add-next"); }
});

listen("status-changed", refresh);
setInterval(render, 1000);
refresh();
