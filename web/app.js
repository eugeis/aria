// Aria — Musik & Downloads. Vanilla SPA (no build step), served by the
// brain (hydration: vanilla); the /dl API lives in the adapter sidecar
// (same origin, split at the ingress).
"use strict";

const $ = (id) => document.getElementById(id);

let collections = [];
let library = null;          // cached /dl/library
let playlistsCache = [];     // cached /dl/playlists
let openPlaylist = null;     // current detail view {id, name, items:[...]}
let searchMode = "library";
let searchHits = [];         // current result rows (playable or youtube)
let queueTimer = null;

// ── player ────────────────────────────────────────────────────────────────
const player = {
  audio: new Audio(),
  context: [],   // [{id, title, artist, art?}] — the "now playing" list
  idx: -1,
};

function fmtDur(secs) {
  if (secs == null) return "–";
  secs = Math.floor(secs);
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

function fmtMs(ms) { return fmtDur(ms / 1000); }

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function toast(msg, isError = false) {
  const t = $("toast");
  t.textContent = msg;
  t.classList.toggle("error", isError);
  t.classList.remove("hidden");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.add("hidden"), 4000);
}

async function api(path, opts = {}) {
  // Stringified JSON bodies are still JSON — the server must see the
  // Content-Type or the axum Json extractor answers 415. Caller headers win.
  const headers = { ...(opts.headers || {}) };
  if (opts.body !== undefined && !(opts.body instanceof FormData) && !headers["Content-Type"]) {
    headers["Content-Type"] = "application/json";
  }
  const res = await fetch(path, { ...opts, headers });
  const data = await res.json().catch(() => ({}));
  if (!res.ok || data.ok === false) {
    throw new Error(data.error || data.errors?.join(" ") || `HTTP ${res.status}`);
  }
  return data;
}

function playlistSelectHtml(selected = "") {
  return `<select id="add-pl-select" class="pl-select">${
    playlistsCache.map((p) => `<option value="${p.id}" ${p.id === selected ? "selected" : ""}>${esc(p.name)}</option>`).join("")
  }</select>`;
}

// ── player ────────────────────────────────────────────────────────────────

async function playContext(list, idx) {
  if (!list.length) return;
  player.context = list;
  player.idx = idx;
  await playCurrent();
}

async function playCurrent() {
  const cur = player.context[player.idx];
  if (!cur) return;
  $("player").classList.remove("hidden");
  $("pp-title").textContent = cur.title;
  $("pp-artist").textContent = cur.artist || "";
  $("pp-play").textContent = "❚❚";
  try {
    let url;
    if (cur.path) {
      // Offline entry: cache-first blob URL (works without a connection).
      url = await MosaicOffline.url(cur.path);
    } else {
      const r = await api("/dl/play", { method: "POST", body: JSON.stringify({ id: cur.id }) });
      url = r.url;
    }
    player.audio.src = url;
    await player.audio.play();
  } catch (e) {
    toast(`Abspielen fehlgeschlagen: ${e.message}`, true);
  }
}

async function playTrackId(id) {
  const t = library?.collections?.flatMap((c) => c.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks)))?.find((x) => x.id === id);
  const list = t ? [t] : [{ id, title: "?", artist: "" }];
  await playContext(list, 0);
}

function playerNext() {
  if (player.idx < player.context.length - 1) {
    player.idx += 1;
    playCurrent();
  }
}
function playerPrev() {
  if (player.idx > 0) {
    player.idx -= 1;
    playCurrent();
  } else {
    player.audio.currentTime = 0;
  }
}

player.audio.addEventListener("play", () => ($("pp-play").textContent = "❚❚"));
player.audio.addEventListener("pause", () => ($("pp-play").textContent = "▶"));
player.audio.addEventListener("ended", playerNext);
player.audio.addEventListener("timeupdate", () => {
  const d = player.audio.duration || 0;
  $("pp-fill").style.width = d ? `${(100 * player.audio.currentTime) / d}%` : "0%";
  $("pp-time").textContent = `${fmtDur(player.audio.currentTime)} / ${fmtDur(d)}`;
});
$("pp-bar").addEventListener("click", (e) => {
  const r = $("pp-bar").getBoundingClientRect();
  const d = player.audio.duration || 0;
  if (d) player.audio.currentTime = ((e.clientX - r.left) / r.width) * d;
});
$("pp-play").addEventListener("click", () => {
  if (player.audio.paused) player.audio.play(); else player.audio.pause();
});
$("pp-next").addEventListener("click", playerNext);
$("pp-prev").addEventListener("click", playerPrev);
$("pp-close").addEventListener("click", () => {
  player.audio.pause();
  player.audio.removeAttribute("src");
  $("player").classList.add("hidden");
});

// ── offline (device cache, the mosaic offline.js helper) ────────────────

let offlineList = JSON.parse(localStorage.getItem("aria-offline") || "[]");

function fmtSize(bytes) {
  if (bytes == null) return "–";
  const mb = bytes / 1048576;
  return mb >= 1 ? mb.toFixed(1) + " MB" : Math.max(1, Math.ceil(bytes / 1024)) + " KB";
}

function offlineBadge() {
  const b = $("offline-count");
  b.textContent = String(offlineList.length);
  b.classList.toggle("hidden", offlineList.length === 0);
}

function persistOffline() {
  localStorage.setItem("aria-offline", JSON.stringify(offlineList));
  offlineBadge();
}

async function renderOffline() {
  const box = $("offline-list");
  offlineBadge();
  if (!offlineList.length) {
    $("offline-total").textContent = "";
    box.innerHTML = `<p class="muted">Nichts gespeichert — in der Bibliothek per ⤓ wählen (Titel oder „Album offline“).</p>`;
    return;
  }
  const total = offlineList.reduce((s, e) => s + (e.size || 0), 0);
  $("offline-total").textContent = `${offlineList.length} Dateien · ${fmtSize(total)} auf diesem Gerät`;
  box.innerHTML = offlineList
    .map(
      (e, i) => `
    <div class="pl-row">
      <span class="t-no"></span>
      <span class="t-title" title="${esc(e.path)}">${esc(e.title || e.path)}</span>
      <span class="t-meta muted">${e.artist ? esc(e.artist) + " · " : ""}${fmtSize(e.size)}${e.missing ? " · nicht mehr im Cache" : ""}</span>
      <span class="t-actions">
        ${e.missing ? "" : `<button class="act" data-off="play" data-i="${i}" title="Abspielen (offline)">▶</button>`}
        <button class="act" data-off="re" data-i="${i}" title="Erneut herunterladen">⭳</button>
        <button class="act danger" data-off="del" data-i="${i}" title="Entfernen">✕</button>
      </span>
    </div>`
    )
    .join("");
}

// Resolve the size of a catalog file (the download loop needs it).
async function catalogSize(path) {
  const c = await MosaicOffline.catalog();
  const f = (c.files || []).find((x) => x.path === path);
  return f ? f.size : null;
}

async function saveOffline(entry) {
  if (offlineList.some((e) => e.path === entry.path)) return toast("Bereits gespeichert", true);
  let size = null;
  try {
    size = await catalogSize(entry.path);
  } catch (e) {
    toast(`Katalog: ${e.message}`, true);
    return;
  }
  if (size == null) return toast(`Datei nicht im Offline-Katalog: ${entry.path}`, true);
  entry.size = size;
  offlineList.push(entry);
  persistOffline();
  renderOffline();
  try {
    let last = 0;
    await MosaicOffline.download(entry.path, size, (p) => {
      if (p - last >= 0.2 || p >= 1) {
        last = p;
        toast(`Speichere „${entry.title}“ … ${Math.round(p * 100)}%`);
      }
    });
    toast(`„${entry.title}“ ist offline verfügbar`);
  } catch (e) {
    offlineList = offlineList.filter((x) => x.path !== entry.path);
    persistOffline();
    renderOffline();
    toast(`Speichern fehlgeschlagen: ${e.message}`, true);
  }
}

async function saveOfflineAlbum(tracks) {
  const todo = tracks.filter(
    (t) => t.rel_path && !offlineList.some((e) => e.path === t.rel_path)
  );
  if (!todo.length) return toast("Album ist bereits komplett offline", true);
  toast(`Speichere ${todo.length} Titel von „${tracks[0].album}“ offline …`);
  let failed = 0;
  for (const t of todo) {
    try {
      const size = await catalogSize(t.rel_path);
      if (size == null) {
        failed += 1;
        continue;
      }
      offlineList.push({ path: t.rel_path, title: t.title, artist: t.artist, album: t.album, size });
      persistOffline();
      await MosaicOffline.download(t.rel_path, size, null);
      renderOffline();
    } catch {
      offlineList = offlineList.filter((x) => x.path !== t.rel_path);
      persistOffline();
      failed += 1;
    }
  }
  renderOffline();
  toast(failed ? `Album offline: ${todo.length - failed}/${todo.length} (${failed} fehlgeschlagen)` : `Album „${tracks[0].album}“ ist offline verfügbar`, failed > 0);
}

document.addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-off]");
  if (!b) return;
  const i = Number(b.dataset.i);
  const entry = offlineList[i];
  if (!entry) return;
  if (b.dataset.off === "del") {
    MosaicOffline.remove(entry.path).catch(() => {});
    offlineList.splice(i, 1);
    persistOffline();
    renderOffline();
  } else if (b.dataset.off === "re") {
    offlineList = offlineList.filter((x) => x.path !== entry.path);
    persistOffline();
    renderOffline();
    saveOffline(entry);
  } else if (b.dataset.off === "play") {
    const list = offlineList.map((x) => ({ path: x.path, title: x.title || x.path, artist: x.artist || "" }));
    const idx = list.findIndex((x) => x.path === entry.path);
    playContext(list, Math.max(0, idx));
  }
});

$("offline-verify").addEventListener("click", async () => {
  for (const e of offlineList) e.missing = !(await MosaicOffline.isCached(e.path));
  renderOffline();
  const n = offlineList.filter((e) => e.missing).length;
  toast(n ? `${n} Dateien nicht (mehr) im Cache` : "Alles im Cache");
});

// ── auth ──────────────────────────────────────────────────────────────────

async function boot() {
  let me;
  try {
    me = await api("/dl/me");
  } catch {
    me = { ok: false };
  }
  if (!me.ok) {
    $("login").classList.remove("hidden");
    $("login-user").focus();
    return;
  }
  const u = me.user || {};
  $("user-badge").textContent = u.name || u.user || "?";
  $("user-badge").title = `Angemeldet als ${u.user} (${u.role || "?"})`;
  $("user-badge").className = "badge ok";
  $("logout").classList.remove("hidden");

  loadCollections();
  loadLibrary();
  loadPlaylists();
  refreshCookiesBadge();
  refreshCookiesStatus();
  renderQueue(await api("/dl/queue").catch(() => ({ items: [] })));
  renderOffline();
  startQueuePolling();
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("login-error").textContent = "";
  try {
    const brain = await fetch("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ username: $("login-user").value, password: $("login-pass").value }),
    });
    const bd = await brain.json().catch(() => ({}));
    if (!brain.ok || !bd.token) throw new Error(bd.error || `Anmeldung fehlgeschlagen (HTTP ${brain.status})`);
    await api("/dl/login", { method: "POST", body: JSON.stringify({ token: bd.token }) });
    $("login").classList.add("hidden");
    boot();
  } catch (err) {
    $("login-error").textContent = err.message;
  }
});

$("logout").addEventListener("click", async () => {
  await api("/dl/logout", { method: "POST" }).catch(() => {});
  location.reload();
});

// ── tabs ──────────────────────────────────────────────────────────────────

document.querySelectorAll(".tabs button").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll(".tabs button").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    b.classList.add("active");
    $(`tab-${b.dataset.tab}`).classList.add("active");
    if (b.dataset.tab === "queue") refreshQueueNow();
    if (b.dataset.tab === "offline") renderOffline();
  })
);

// ── browse ────────────────────────────────────────────────────────────────

async function loadLibrary() {
  try {
    library = await api("/dl/library");
  } catch (e) {
    $("browse").innerHTML = `<p class="error">${esc(e.message)}</p>`;
    return;
  }
  renderBrowse();
}

function trackRowHtml(t, opts = {}) {
  const no = t.track_no ? `<span class="t-no">${t.track_no}</span>` : `<span class="t-no"></span>`;
  const dur = `<span class="t-dur">${fmtMs(t.duration_ms)}</span>`;
  const play = `<button class="act" data-act="play" data-id="${t.id}" title="Abspielen">▶</button>`;
  const pl = `<button class="act" data-act="pladd" data-id="${t.id}" title="Zur Playlist">＋</button>`;
  const off = t.rel_path
    ? `<button class="act" data-act="t-off" data-path="${esc(t.rel_path)}" data-title="${esc(t.title)}" data-artist="${esc(t.artist || "")}" title="Für offline speichern">⤓</button>`
    : "";
  return `<div class="t-row" data-track="${t.id}">
    ${no}<span class="t-title" title="${esc(t.title)}">${esc(t.title)}</span>
    <span class="t-meta muted">${esc(t.artist || "")}${t.album ? " · " + esc(t.album) : ""}</span>
    ${dur}<span class="t-actions">${play}${pl}${off}</span>
  </div>`;
}

function renderBrowse() {
  const box = $("browse");
  if (!library.collections?.length) {
    box.innerHTML = `<p class="muted">Die Bibliothek ist leer.</p>`;
    return;
  }
  box.innerHTML = library.collections
    .map((c) => `
      <details class="col" ${c.name === "musik" ? "open" : ""}>
        <summary><b>${esc(c.name)}</b> <span class="muted">(${c.kind} · ${c.count})</span></summary>
        ${c.artists
          .map(
            (a) => `
          <details class="artist">
            <summary><b>${esc(a.name)}</b></summary>
            ${a.albums
              .map(
                (al) => `
              <details class="album">
                <summary>${esc(al.name)}${al.year ? ` <span class="muted">(${esc(al.year)})</span>` : ""} <span class="muted">(${al.tracks.length})</span></summary>
                <div class="album-head"><span></span><span class="t-dur">Zeit</span><span class="t-actions"></span>
                  <span class="t-actions"><button class="act ghost" data-act="playall-album" data-artist="${esc(a.name)}" data-album="${esc(al.name)}" title="Album abspielen">▶ Album</button>
                  <button class="act ghost" data-act="pladd-album" data-artist="${esc(a.name)}" data-album="${esc(al.name)}" title="Album zur Playlist">＋ Album</button>
                  <button class="act ghost" data-act="off-album" data-artist="${esc(a.name)}" data-album="${esc(al.name)}" title="Album für offline speichern">⤓ Album offline</button></span>
                </div>
                ${al.tracks.map((t) => trackRowHtml(t)).join("")}
              </details>`
              )
              .join("")}
          </details>`
          )
          .join("")}
      </details>`
    )
    .join("");
}

document.addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-act]");
  if (!b) return;
  const act = b.dataset.act;
  if (act === "play") playTrackId(Number(b.dataset.id));
  else if (act === "pladd") addTrackToPlaylist(Number(b.dataset.id));
  else if (act === "playall-album") playAlbum(b.dataset.artist, b.dataset.album);
  else if (act === "pladd-album") addAlbumToPlaylist(b.dataset.artist, b.dataset.album);
  else if (act === "t-off")
    saveOffline({ path: b.dataset.path, title: b.dataset.title, artist: b.dataset.artist });
  else if (act === "off-album") {
    const tracks = albumTracks(b.dataset.artist, b.dataset.album);
    if (!tracks.length) return;
    saveOfflineAlbum(tracks);
  }
  else if (act === "yt-dl") {
    const h = searchHits[Number(b.dataset.i)];
    queueYtItems([h], h.channel || $("yt-artist").value, $("yt-album").value || h.title);
  }
  else if (act === "yt-pl") {
    const h = searchHits[Number(b.dataset.i)];
    const sel = document.getElementById(`yt-pl-${b.dataset.i}`);
    addToPlaylist([{ kind: "youtube", url: h.url, title: h.title, artist: h.channel || "", album: "" }], sel ? sel.value : null, () =>
      toast(`„${h.title}“ zur Playlist hinzugefügt`));
  }
  else if (act === "pl-play") playPlItem(Number(b.dataset.pos));
  else if (act === "pl-del") delPlItem(Number(b.dataset.pos));
  else if (act === "pl-up") movePlItem(Number(b.dataset.pos), -1);
  else if (act === "pl-down") movePlItem(Number(b.dataset.pos), 1);
});

function albumTracks(artist, album) {
  return (
    library?.collections?.flatMap((c) => c.artists).find((a) => a.name === artist)?.albums.find((al) => al.name === album)?.tracks || []
  );
}

function playAlbum(artist, album) {
  const tracks = albumTracks(artist, album).filter((t) => t.playable !== false);
  playContext(tracks, 0);
}

async function addTrackToPlaylist(id) {
  const t = albumTracksAll().find((x) => x.id === id);
  if (!t) return;
  const sel = promptPlaylist("Titel zur Playlist hinzufügen:");
  if (!sel) return;
  await addToPlaylist([{ kind: "track", track_id: id, title: t.title, artist: t.artist, album: t.album }], sel);
}

function albumTracksAll() {
  return (
    library?.collections?.flatMap((c) =>
      c.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks))
    ) || []
  );
}

function addAlbumToPlaylist(artist, album) {
  const items = albumTracks(artist, album).map((t) => ({
    kind: "track",
    track_id: t.id,
    title: t.title,
    artist: t.artist,
    album: t.album,
  }));
  const sel = promptPlaylist(`Album „${album}“ zur Playlist hinzufügen:`);
  if (!sel) return;
  addToPlaylist(items, sel, () => toast(`${items.length} Titel zur Playlist hinzugefügt`));
}

function promptPlaylist(hint) {
  if (!playlistsCache.length) {
    toast("Erst eine Playlist erstellen (Tab Playlists)", true);
    return null;
  }
  const names = playlistsCache.map((p, i) => `${i + 1}. ${p.name}`).join("\n");
  const raw = prompt(`${hint}\n\n${names}\n\nNummer wählen:`);
  const n = parseInt(raw, 10);
  if (!n || !playlistsCache[n - 1]) return null;
  return playlistsCache[n - 1].id;
}

// ── search ────────────────────────────────────────────────────────────────

$("search-mode").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-mode]");
  if (!b) return;
  searchMode = b.dataset.mode;
  document.querySelectorAll("#search-mode button").forEach((x) => x.classList.remove("active"));
  b.classList.add("active");
  $("search-n").style.visibility = searchMode === "youtube" ? "visible" : "hidden";
});

async function doSearch() {
  const q = $("search-q").value.trim();
  if (!q) return;
  const box = $("search-results");
  box.innerHTML = `<p class="muted">Suche läuft …</p>`;
  try {
    if (searchMode === "library") {
      const r = await api(`/dl/library/search?q=${encodeURIComponent(q)}`);
      searchHits = (r.tracks || []).map((t) => ({ ...t, _kind: "track" }));
      box.innerHTML =
        searchHits.length === 0
          ? `<p class="muted">Nichts gefunden.</p>`
          : searchHits
              .map((t, i) => trackRowHtml(t))
              .join("");
    } else {
      const r = await api(`/dl/search?q=${encodeURIComponent(q)}&n=${$("search-n").value}`);
      searchHits = (r.hits || []).map((h) => ({ ...h, _kind: "youtube" }));
      box.innerHTML =
        searchHits.length === 0
          ? `<p class="muted">Keine Treffer.</p>`
          : searchHits
              .map(
                (h, i) => `
          <div class="t-row">
            <span class="t-no"></span>
            <span class="t-title" title="${esc(h.title)}">${esc(h.title)}</span>
            <span class="t-meta muted">${esc(h.channel || "")}</span>
            <span class="t-dur">${fmtDur(h.duration)}</span>
            <span class="t-actions">
              <button class="act" data-act="yt-dl" data-i="${i}" title="Herunterladen">⬇</button>
              <select class="pl-select" id="yt-pl-${i}" title="Zur Playlist">${playlistSelectHtml()}</select>
              <button class="act" data-act="yt-pl" data-i="${i}">＋</button>
            </span>
          </div>`
              )
              .join("");
      if (searchHits.length && !$("yt-artist").value && searchHits[0].channel) $("yt-artist").value = searchHits[0].channel;
    }
  } catch (err) {
    box.innerHTML = `<p class="error">${esc(err.message)}</p>`;
  }
}

$("search-go").addEventListener("click", doSearch);
$("search-q").addEventListener("keydown", (e) => e.key === "Enter" && doSearch());

async function loadCollections() {
  const q = await api("/dl/queue").catch(() => ({ collections: [] }));
  collections = q.collections || [];
  const sel = $("yt-collection");
  if (sel) sel.innerHTML = collections.map((c) => `<option>${esc(c)}</option>`).join("");
}

async function queueYtItems(hits, artist, album) {
  const collection = $("yt-collection").value || collections[0];
  if (!collection) return toast("Keine Sammlung konfiguriert", true);
  const items = hits.map((h, i) => ({
    url: h.url,
    title: h.title,
    artist: artist || h.channel || "",
    album: album || "",
    collection,
    track: i + 1,
  }));
  try {
    const r = await api("/dl/queue", { method: "POST", body: JSON.stringify({ items }) });
    toast(`${items.length} in die Download-Warteschlange`);
    if (r.errors?.length) toast(r.errors.join(" "), true);
    refreshQueueNow();
  } catch (err) {
    toast(err.message, true);
  }
}

$("yt-add").addEventListener("click", () => {
  const box = $("search-results");
  const checks = [...box.querySelectorAll(".hit-check:checked")].map((c) => Number(c.dataset.i));
  if (!checks.length) return toast("Nichts markiert", true);
  queueYtItems(checks.map((i) => searchHits[i]));
});

// ── playlists ─────────────────────────────────────────────────────────────

async function loadPlaylists(openId = null) {
  try {
    const r = await api("/dl/playlists");
    playlistsCache = r.playlists || [];
    $("pl-count").textContent = String(playlistsCache.length);
    $("pl-count").classList.toggle("hidden", playlistsCache.length === 0);
  } catch {
    playlistsCache = [];
  }
  if (openId) return openPlaylistView(openId);
  renderPlList();
}

function renderPlList() {
  const box = $("pl-cards");
  if (!playlistsCache.length) {
    box.innerHTML = `<p class="muted">Noch keine Playlists — obene eine anlegen.</p>`;
    return;
  }
  box.innerHTML = playlistsCache
    .map(
      (p) => `
    <div class="pl-card" data-pl="${p.id}">
      <div class="pl-card-name">${esc(p.name)}</div>
      <div class="muted">${p.count} Titel · ${esc(p.updated_at || "")}</div>
      ${p.description ? `<div class="muted pl-card-desc">${esc(p.description)}</div>` : ""}
    </div>`
    )
    .join("");
  $("pl-list").classList.remove("hidden");
  $("pl-detail").classList.add("hidden");
}

async function openPlaylistView(id) {
  const r = await api(`/dl/playlists/${id}`);
  openPlaylist = r.playlist;
  $("pl-list").classList.add("hidden");
  $("pl-detail").classList.remove("hidden");
  $("pl-name").textContent = openPlaylist.name;
  $("pl-desc").textContent = openPlaylist.description || "";
  renderPlItems();
}

function renderPlItems() {
  const box = $("pl-items");
  const items = openPlaylist?.items || [];
  if (!items.length) {
    box.innerHTML = `<p class="muted">Leere Playlist — Titel über Suche oder Bibliothek hinzufügen.</p>`;
    return;
  }
  box.innerHTML = items
    .map(
      (it, i) => `
    <div class="pl-row ${it.missing ? "missing" : ""}">
      <span class="t-no">${i + 1}</span>
      <span class="t-title" title="${esc(it.title)}">${esc(it.title)}</span>
      <span class="t-meta muted">${esc(it.artist || "")}${it.album ? " · " + esc(it.album) : ""}${it.kind === "youtube" ? " · YouTube" : ""}${it.missing ? " · fehlt" : ""}</span>
      <span class="t-dur">${it.duration_ms ? fmtMs(it.duration_ms) : "–"}</span>
      <span class="t-actions">
        ${it.kind === "track" && it.playable !== false && !it.missing ? `<button class="act" data-act="pl-play" data-pos="${i}" title="Abspielen">▶</button>` : ""}
        <button class="act" data-act="pl-up" data-pos="${i}" title="Nach oben">↑</button>
        <button class="act" data-act="pl-down" data-pos="${i}" title="Nach unten">↓</button>
        <button class="act danger" data-act="pl-del" data-pos="${i}" title="Entfernen">✕</button>
      </span>
    </div>`
    )
    .join("");
}

function plPlayableTracks() {
  return (openPlaylist?.items || []).filter((it) => it.kind === "track" && it.track_id != null && it.playable !== false && !it.missing);
}

function playPlItem(pos) {
  const playable = plPlayableTracks();
  const it = openPlaylist.items[pos];
  if (it.kind !== "track") return toast("YouTube-Titel können nur heruntergeladen werden", true);
  const idx = playable.findIndex((p) => p.track_id === it.track_id);
  playContext(playable, Math.max(0, idx));
}

async function savePlItems(items) {
  await api(`/dl/playlists/${openPlaylist.id}/items`, {
    method: "PUT",
    body: JSON.stringify({ items: items.map((it) => ({ track_id: it.track_id, url: it.url, title: it.title, artist: it.artist, album: it.album })) }),
  });
  openPlaylist.items = items;
  renderPlItems();
}

function movePlItem(pos, dir) {
  const items = [...openPlaylist.items];
  const to = pos + dir;
  if (to < 0 || to >= items.length) return;
  [items[pos], items[to]] = [items[to], items[pos]];
  savePlItems(items);
}

async function delPlItem(pos) {
  const items = [...openPlaylist.items];
  items.splice(pos, 1);
  await savePlItems(items);
}

async function addToPlaylist(items, plId, done) {
  if (!plId) return;
  try {
    await api(`/dl/playlists/${plId}/items`, { method: "POST", body: JSON.stringify({ items }) });
    loadPlaylists();
    if (openPlaylist && openPlaylist.id === plId) openPlaylistView(plId);
    done && done();
  } catch (e) {
    toast(e.message, true);
  }
}

$("pl-cards").addEventListener("click", (e) => {
  const card = e.target.closest(".pl-card");
  if (card) openPlaylistView(card.dataset.pl);
});

$("pl-new").addEventListener("click", async () => {
  const name = $("pl-new-name").value.trim();
  if (!name) return;
  try {
    const r = await api("/dl/playlists", { method: "POST", body: JSON.stringify({ name }) });
    $("pl-new-name").value = "";
    await loadPlaylists(r.id);
  } catch (e) {
    toast(e.message, true);
  }
});

$("pl-back").addEventListener("click", () => loadPlaylists());

$("pl-play-all").addEventListener("click", () => {
  const playable = plPlayableTracks();
  if (!playable.length) return toast("Keine abspielbaren Bibliotheks-Titel in der Playlist", true);
  playContext(playable, 0);
});

$("pl-download-all").addEventListener("click", () => {
  const yt = (openPlaylist?.items || []).filter((it) => it.kind === "youtube");
  if (!yt.length) return toast("Keine YouTube-Titel in der Playlist", true);
  const collection = collections[0];
  if (!collection) return toast("Keine Sammlung konfiguriert", true);
  const items = yt.map((it, i) => ({
    url: it.url,
    title: it.title,
    artist: it.artist || openPlaylist.name,
    album: it.album || openPlaylist.name,
    collection,
    track: i + 1,
  }));
  api("/dl/queue", { method: "POST", body: JSON.stringify({ items }) })
    .then((r) => {
      toast(`${items.length} YouTube-Titel in die Download-Warteschlange`);
      if (r.errors?.length) toast(r.errors.join(" "), true);
      refreshQueueNow();
    })
    .catch((e) => toast(e.message, true));
});

$("pl-delete").addEventListener("click", async () => {
  if (!confirm(`Playlist „${openPlaylist.name}“ löschen?`)) return;
  await api(`/dl/playlists/${openPlaylist.id}`, { method: "DELETE" });
  openPlaylist = null;
  loadPlaylists();
});

// ── queue (downloads) ─────────────────────────────────────────────────────

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
  if (!items.length) {
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
      } else if (st.status === "done") statusCell = `<span class="badge ok">Fertig</span>`;
      else if (st.status === "failed") statusCell = `<span class="badge err" title="${esc(st.error)}">Fehler</span>`;
      else if (st.status === "cancelled") statusCell = `<span class="muted">Abgebrochen</span>`;
      const active = st.status === "downloading" || st.status === "queued";
      return `<tr>
        <td class="muted">${it.track ?? n}</td>
        <td class="q-title" title="${esc(it.title)}">${esc(it.title)}</td>
        <td class="muted">${esc(it.artist)}${it.album ? " / " + esc(it.album) : ""}</td>
        <td class="muted">${esc(it.collection)}</td>
        <td>${statusCell}${st.status === "failed" ? `<div class="q-err">${esc(st.error)}</div>` : ""}</td>
        <td class="q-actions">
          ${active ? `<button class="ghost danger" data-act="q-cancel" data-id="${it.id}">Abbrechen</button>` : ""}
          ${!active ? `<button class="ghost" data-act="q-del" data-id="${it.id}">Entfernen</button>` : ""}
        </td>
      </tr>`;
    })
    .join("");
}

$("queue-table").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-act]");
  if (!b) return;
  try {
    if (b.dataset.act === "q-cancel") await api(`/dl/queue/${b.dataset.id}/cancel`, { method: "POST" });
    else await api(`/dl/queue/${b.dataset.id}`, { method: "DELETE" });
    refreshQueueNow();
  } catch (err) {
    toast(err.message, true);
  }
});

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
      renderQueue(q.items || []);
    })
    .catch(() => {});
}

function startQueuePolling() {
  if (queueTimer) return;
  queueTimer = setInterval(refreshQueueNow, 2500);
}

// ── youtube cookies ───────────────────────────────────────────────────────

async function refreshCookiesBadge() {
  const s = await api("/dl/cookies").catch(() => null);
  const b = $("cookie-badge");
  if (s?.present) {
    b.textContent = "YT-Sitzung: verbunden";
    b.className = "badge ok";
  } else {
    b.textContent = "YT-Sitzung: keine";
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
  try {
    await api("/dl/cookies", { method: "POST", body: await f.text(), headers: { "Content-Type": "text/plain" } });
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

boot();
