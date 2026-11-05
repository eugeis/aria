// Aria Downloads — the web app logic (vanilla, no build step).
"use strict";

const $ = (id) => document.getElementById(id);

let collections = [];
let authed = false;
let searchHits = [];
let playlistHits = [];
let queueTimer = null;

// ---------- helpers ----------

function toast(msg, isError = false) {
  const t = $("toast");
  t.textContent = msg;
  t.classList.toggle("error", isError);
  t.classList.remove("hidden");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.add("hidden"), 4000);
}

async function api(path, opts = {}) {
  const res = await fetch(path, {
    headers: opts.body instanceof FormData ? {} : { "Content-Type": "application/json" },
    ...opts,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok || data.ok === false) {
    throw new Error(data.error || data.errors?.join(" ") || `HTTP ${res.status}`);
  }
  return data;
}

function fmtDur(secs) {
  if (secs == null) return "–";
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${m}:${String(s).padStart(2, "0")}`;
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// ---------- auth ----------

async function boot() {
  try {
    const me = await api("/dl/me");
    authed = me.ok;
  } catch {
    authed = false;
  }
  if (!authed) {
    showLogin();
    return;
  }
  $("logout").classList.toggle("hidden", !authed);
  const q = await api("/dl/queue").catch(() => ({ items: [], collections: [] }));
  collections = q.collections || [];
  fillCollectionSelects();
  refreshCookiesBadge();
  renderQueue(q.items || []);
  startQueuePolling();
}

function showLogin() {
  $("login").classList.remove("hidden");
  $("login-token").focus();
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await api("/dl/login", { method: "POST", body: JSON.stringify({ token: $("login-token").value }) });
    $("login-error").textContent = "";
    $("login").classList.add("hidden");
    authed = true;
    boot();
  } catch (err) {
    $("login-error").textContent = err.message;
  }
});

$("logout").addEventListener("click", async () => {
  await api("/dl/logout", { method: "POST" }).catch(() => {});
  location.reload();
});

// ---------- tabs ----------

document.querySelectorAll(".tabs button").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll(".tabs button").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    b.classList.add("active");
    $(`tab-${b.dataset.tab}`).classList.add("active");
    if (b.dataset.tab === "queue") refreshQueueNow();
    if (b.dataset.tab === "cookies") refreshCookiesStatus();
  })
);

function fillCollectionSelects() {
  for (const id of ["search-collection", "pl-collection"]) {
    const sel = $(id);
    sel.innerHTML = collections.map((c) => `<option>${esc(c)}</option>`).join("");
  }
}

// ---------- cookies ----------

async function refreshCookiesBadge() {
  const s = await api("/dl/cookies").catch(() => null);
  const b = $("cookie-badge");
  if (s?.present) {
    b.textContent = "Sitzung: verbunden";
    b.className = "badge ok";
  } else {
    b.textContent = "Sitzung: keine";
    b.className = "badge warn";
  }
}

async function refreshCookiesStatus() {
  const s = await api("/dl/cookies").catch(() => ({ present: false }));
  $("cookie-status").innerHTML = s.present
    ? `<span class="badge ok">Verbunden</span> <span class="muted">${s.youtube_lines ?? 0} YouTube-Cookies · aktualisiert ${esc(s.updated ?? "–")}</span>`
    : `<span class="badge warn">Keine Cookies</span> <span class="muted">Downloads laufen ohne deine Sitzung.</span>`;
}

$("cookie-upload").addEventListener("click", async () => {
  const f = $("cookie-file").files[0];
  if (!f) return toast("Zuerst eine cookies.txt wählen", true);
  const text = await f.text();
  try {
    await api("/dl/cookies", { method: "POST", body: text, headers: { "Content-Type": "text/plain" } });
    toast("YouTube-Sitzung verbunden");
    refreshCookiesStatus();
    refreshCookiesBadge();
  } catch (err) {
    toast(err.message, true);
  }
});

$("cookie-delete").addEventListener("click", async () => {
  try {
    await api("/dl/cookies", { method: "DELETE" });
    toast("Cookies entfernt");
    refreshCookiesStatus();
    refreshCookiesBadge();
  } catch (err) {
    toast(err.message, true);
  }
});

// ---------- search ----------

async function doSearch() {
  const q = $("search-q").value.trim();
  if (!q) return;
  const box = $("search-results");
  box.innerHTML = `<p class="muted">Suche läuft …</p>`;
  try {
    const data = await api(`/dl/search?q=${encodeURIComponent(q)}&n=${$("search-n").value}`);
    searchHits = data.hits || [];
    box.innerHTML =
      searchHits.length === 0
        ? `<p class="muted">Keine Treffer.</p>`
        : searchHits
            .map(
              (h, i) => `
            <label class="hit">
              <input type="checkbox" class="hit-check" data-i="${i}">
              <span class="hit-dur">${fmtDur(h.duration)}</span>
              <span class="hit-title" title="${esc(h.title)}">${esc(h.title)}</span>
              <span class="hit-channel">${esc(h.channel || "")}</span>
            </label>`
            )
            .join("");
    const firstChannel = searchHits.find((h) => h.channel)?.channel || "";
    if (firstChannel && !$("search-artist").value) $("search-artist").value = firstChannel;
    showTargets("search");
  } catch (err) {
    box.innerHTML = `<p class="error">${esc(err.message)}</p>`;
  }
}

$("search-go").addEventListener("click", doSearch);
$("search-q").addEventListener("keydown", (e) => e.key === "Enter" && doSearch());

function showTargets(which) {
  $(`${which}-targets`).classList.remove("hidden");
  $(`${which}-add`).disabled = false;
}

async function addToQueue(which) {
  const hits = which === "search" ? searchHits : playlistHits;
  const box = $(`${which}-results`);
  const checks = [...box.querySelectorAll(".hit-check:checked")].map((c) => Number(c.dataset.i));
  if (checks.length === 0) return toast("Nichts markiert", true);
  const artist = $(`${which}-artist`).value.trim();
  const album = $(`${which}-album`).value.trim();
  const collection = $(`${which}-collection`).value;
  const items = checks.map((i) => ({
    url: hits[i].url,
    id: hits[i].id,
    title: hits[i].title,
    artist,
    album,
    collection,
    track: which === "playlist" ? checks.indexOf(i) + 1 : undefined,
  }));
  try {
    const r = await api("/dl/queue", { method: "POST", body: JSON.stringify({ items }) });
    toast(`${items.length} in die Warteschlange`);
    if (r.errors?.length) toast(r.errors.join(" "), true);
    box.querySelectorAll(".hit-check").forEach((c) => c.checked = false);
    refreshQueueNow();
  } catch (err) {
    toast(err.message, true);
  }
}

$("search-add").addEventListener("click", () => addToQueue("search"));
$("pl-add").addEventListener("click", () => addToQueue("playlist"));

// ---------- playlist ----------

async function doPlaylist() {
  const url = $("pl-url").value.trim();
  if (!url) return;
  const box = $("pl-results");
  box.innerHTML = `<p class="muted">Lade Playlist …</p>`;
  try {
    const data = await api(`/dl/playlist?url=${encodeURIComponent(url)}`);
    playlistHits = data.hits || [];
    $("pl-title").textContent = playlistHits.length
      ? `${data.title || url}${data.channel ? " — " + data.channel : ""} · ${playlistHits.length} Titel`
      : "Keine Titel gefunden.";
    box.innerHTML =
      playlistHits.length === 0
        ? `<p class="muted">–</p>`
        : playlistHits
            .map(
              (h, i) => `
            <label class="hit">
              <input type="checkbox" class="hit-check" data-i="${i}" checked>
              <span class="hit-dur">${fmtDur(h.duration)}</span>
              <span class="hit-title" title="${esc(h.title)}">${esc(h.title)}</span>
              <span class="hit-channel">${esc(h.channel || "")}</span>
            </label>`
            )
            .join("");
    if (data.channel && !$("pl-artist").value) $("pl-artist").value = data.channel;
    if ((data.title || "").length > 1 && !$("pl-album").value) $("pl-album").value = data.title;
    showTargets("playlist");
  } catch (err) {
    box.innerHTML = `<p class="error">${esc(err.message)}</p>`;
  }
}

$("pl-go").addEventListener("click", doPlaylist);
$("pl-url").addEventListener("keydown", (e) => e.key === "Enter" && doPlaylist());

// ---------- queue ----------

function renderQueue(items) {
  const table = $("queue-table");
  const empty = $("queue-empty");
  const running = items.filter((i) => i.status.status === "downloading").length;
  const pending = items.filter((i) => i.status.status === "queued").length;
  $("queue-state").textContent = running ? `1 läuft, ${pending} warten` : pending ? `${pending} in der Warteschlange` : items.length ? "Warteschlange leer" : "";
  const count = running + pending;
  const badge = $("queue-count");
  badge.textContent = String(count);
  badge.classList.toggle("hidden", count === 0);
  if (items.length === 0) {
    table.classList.add("hidden");
    empty.classList.remove("hidden");
    return;
  }
  empty.classList.add("hidden");
  table.classList.remove("hidden");
  let n = 0;
  table.tBodies[0].innerHTML = items
    .map((it) => {
      n += 1;
      const st = it.status;
      let statusCell = `<span class="muted">Wartet</span>`;
      if (st.status === "downloading") {
        const pct = Math.round(st.progress ?? 0);
        statusCell = `<div class="prog"><div class="prog-bar" style="width:${pct}%"></div><span>${pct}%</span></div>`;
      } else if (st.status === "done") {
        statusCell = `<span class="badge ok">Fertig</span>`;
      } else if (st.status === "failed") {
        statusCell = `<span class="badge err" title="${esc(st.error)}">Fehler</span>`;
      } else if (st.status === "cancelled") {
        statusCell = `<span class="muted">Abgebrochen</span>`;
      }
      const active = st.status === "downloading" || st.status === "queued";
      return `<tr>
        <td class="muted">${it.track ?? n}</td>
        <td class="q-title" title="${esc(it.title)}">${esc(it.title)}</td>
        <td class="muted">${esc(it.artist)}${it.album ? " / " + esc(it.album) : ""}</td>
        <td class="muted">${esc(it.collection)}</td>
        <td>${statusCell}${st.status === "failed" ? `<div class="q-err">${esc(st.error)}</div>` : ""}</td>
        <td class="q-actions">
          ${active ? `<button class="ghost danger" data-act="cancel" data-id="${it.id}">Abbrechen</button>` : ""}
          ${!active ? `<button class="ghost" data-act="remove" data-id="${it.id}">Entfernen</button>` : ""}
        </td>
      </tr>`;
    })
    .join("");
}

table_actions();
function table_actions() {
  $("queue-table").addEventListener("click", async (e) => {
    const b = e.target.closest("button[data-act]");
    if (!b) return;
    try {
      if (b.dataset.act === "cancel") await api(`/dl/queue/${b.dataset.id}/cancel`, { method: "POST" });
      else await api(`/dl/queue/${b.dataset.id}`, { method: "DELETE" });
      refreshQueueNow();
    } catch (err) {
      toast(err.message, true);
    }
  });
}

$("queue-clear").addEventListener("click", async () => {
  const r = await api("/dl/queue/clear-done", { method: "POST" }).catch((e) => {
    toast(e.message, true);
    return null;
  });
  if (r) {
    toast(`${r.removed} entfernt`);
    refreshQueueNow();
  }
});

function refreshQueueNow() {
  api("/dl/queue")
    .then((q) => {
      collections = q.collections || collections;
      fillCollectionSelects();
      renderQueue(q.items || []);
    })
    .catch(() => {});
}

function startQueuePolling() {
  if (queueTimer) return;
  queueTimer = setInterval(() => refreshQueueNow(), 2500);
}

boot();
