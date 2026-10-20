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
  $("sync-all").disabled = !st;
  $("pause").disabled = !st;
  $("pause").textContent = st && st.paused ? "Resume" : "Pause";

  // Banner: the one thing that needs attention, if any.
  const banner = $("banner");
  let html = "";
  let bad = false;
  if (!st && snap.error) {
    html = `${esc(firstLine(snap.error))} <button class="secondary" data-act="config">Open config file</button>`;
    bad = true;
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

  // Libraries.
  $("libraries").innerHTML = (st?.libraries ?? []).map((l) => {
    const [label, cls] = stateLabel(l, st.paused);
    const issues = (l.last_run?.issues ?? []).slice(0, 5).map((i) => `<li>${esc(i.path)}: ${esc(i.reason)}</li>`).join("");
    const failure = l.last_failure ? `<span class="err">${ago(l.last_failure.at)}</span>: ${esc(firstLine(l.last_failure.error))}` : "never";
    return `<div class="lib">
      <div class="lib-top">
        <div><span class="lib-name">${esc(l.name)}</span><span class="badge ${cls}">${esc(label)}</span></div>
        <div class="actions">
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
if (location.hash.startsWith("#cli-")) {
  const kind = location.hash.slice(5);
  history.replaceState(null, "", location.pathname);
  showCli(kind);
} else {
  refreshCli();
}

listen("status-changed", refresh);
setInterval(render, 1000);
refresh();
