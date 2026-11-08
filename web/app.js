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
  // Only playlists the caller may EDIT (own, or admin) — adding to someone
  // else's shared playlist would be a 404.
  const editable = playlistsCache.filter((p) => p.is_mine || !isReadOnly());
  return `<select id="add-pl-select" class="pl-select">${
    editable.map((p) => `<option value="${p.id}" ${p.id === selected ? "selected" : ""}>${esc(p.name)}${p.shared ? " (geteilt)" : ""}</option>`).join("")
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
  try {
    $("player-title").textContent = cur.title;
    $("player-artist").textContent = cur.artist || "";
    $("pp-play").textContent = "❚❚";
    // Album art for the current track (embedded or enrichment-embedded)
    const artBox = $("player-art");
    if (cur.has_art) {
      const img = new Image();
      img.onload = () => {
        artBox.innerHTML = "";
        artBox.classList.add("has-art");
        artBox.appendChild(img);
      };
      img.onerror = () => { artBox.classList.remove("has-art"); artBox.textContent = "♫"; };
      img.src = `/art/${cur.id}`;
    } else {
      artBox.classList.remove("has-art");
      artBox.textContent = "♫";
    }
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
// Phones: tap the title / expand button for the full-screen player.
$("pp-expand").addEventListener("click", (e) => {
  e.stopPropagation();
  $("player").classList.toggle("full");
});
$("player-info").addEventListener("click", () => $("player").classList.toggle("full"));
$("pp-close").addEventListener("click", () => {
  player.audio.pause();
  player.audio.removeAttribute("src");
  $("player").classList.remove("full");
  $("player").classList.add("hidden");
});

// ── reorganizer (layout moves, dedupe, LLM catalog suggestions) ─────────

let reorgState = { plan: null, suggestions: [], accepted: new Set() };

// The layout the user checks in the Organizer (fixed order: language →
// genre → artist → album). No box = keep the current layout (report only).
// "Lose Titel bündeln" appends the loose flag (one-track albums collapse
// into a per-artist folder; junk artist names share "Unsortiert").
function reorgLayoutSpec() {
  const dims = [
    $("ro-lang").checked ? "language" : null,
    $("ro-genre").checked ? "genre" : null,
    $("ro-artist").checked ? "artist" : null,
    $("ro-album").checked ? "album" : null,
  ].filter(Boolean);
  if (!dims.length) return "keep";
  if ($("ro-loose").checked) dims.push("loose");
  return dims.join("-");
}

async function loadReorgCollections() {
  const sel = $("reorg-collection");
  if (sel.options.length) return;
  sel.innerHTML = collections.map((c) => `<option>${esc(c)}</option>`).join("");
}

async function doReorgPlan() {
  await loadReorgCollections();
  const col = $("reorg-collection").value;
  if (!col) return toast("Keine Sammlung konfiguriert", true);
  const box = $("reorg-summary");
  box.textContent = "Plane …";
  let r;
  try {
    r = await api(`/dl/reorg/plan?layout=${encodeURIComponent(reorgLayoutSpec())}&collection=${encodeURIComponent(col)}`);
  } catch (e) {
    box.textContent = "";
    return toast(e.message, true);
  }
  const p = (r.plans || [])[0];
  if (!p) return (box.textContent = "Keine Sammlung gefunden.");
  reorgState.plan = p;
  reorgState.suggestions = [];
  reorgState.accepted.clear();
  const gb = p.move_bytes / 1048576;
  box.textContent =
    `${p.kind} · Layout ${p.layout} · ${p.moves.length} Verschiebungen (${gb.toFixed(1)} MB) · ` +
    `${p.duplicates.length} Duplikat-Gruppen` +
    (p.tag_stats ? ` · Katalog: ${p.tag_stats.total} Titel, ${p.tag_stats.missing_language} ohne Sprache, ${p.tag_stats.missing_genre} ohne Genre, ${p.tag_stats.mojibake} verdächtige Zeichensätze` : "");
  const moves = $("reorg-moves");
  moves.innerHTML = p.moves.length
    ? p.moves
        .map(
          (m) => `
      <div class="pl-row">
        <span class="t-no"></span>
        <span class="t-title" title="${esc(m.from)} → ${esc(m.to)}">${esc(m.title)}</span>
        <span class="t-meta muted">${esc(m.from)} → <b>${esc(m.to)}</b> · ${esc(m.reason)}</span>
        <span class="t-dur"></span><span class="t-actions"></span>
      </div>`
        )
        .join("")
    : `<p class="muted">Alles ist schon im Ziellayout.</p>`;
  $("reorg-apply").classList.toggle("hidden", p.moves.length === 0 || isReadOnly());
  renderDupes(p.duplicates);
  const passList = reorgPassList();
  $("reorg-amb-count").textContent = passList.length
    ? `${passList.length} Titel brauchen Katalogarbeit (Künstler/Album/Sprache/Genre) — LLM-Vorschläge:`
    : "Katalog sieht vollständig aus.";
  $("reorg-suggest").disabled = passList.length === 0 || isReadOnly();
  $("reorg-suggests").innerHTML = "";
  $("reorg-apply-suggest").classList.add("hidden");
}

// The LLM metadata pass: every track that is ambiguous OR still lacks
// language/genre (the two new layout dimensions).
function reorgPassList() {
  const p = reorgState.plan;
  if (!p) return [];
  const ambIds = new Set((p.ambiguous || []).map((a) => a.track_id));
  return (p.tracks || [])
    .filter((t) => ambIds.has(t.id) || !(t.language || "").trim() || !(t.genre || "").trim())
    .map((t, i) => ({
      index: i,
      track_id: t.id,
      title: t.title,
      artist: t.artist,
      album: t.album,
      genre: t.genre,
      language: t.language,
      collection: p.collection,
      kind: "music",
      file: t.file,
    }));
}

function renderDupes(groups) {
  const box = $("reorg-dupes");
  if (!groups.length) {
    box.innerHTML = `<p class="muted">Keine Duplikate.</p>`;
    return;
  }
  box.innerHTML = groups
    .map(
      (g, gi) => `
    <div class="dupe-group">
      <div class="muted" title="Fingerprint ${g.fingerprint}">Gruppe ${gi + 1} — ${g.tracks.length} identische Dateien:</div>
      ${g.tracks
        .map(
          (t, i) => `
        <div class="pl-row">
          <span class="t-no"><input type="radio" name="keep-${gi}" value="${t.track_id}" ${i === 0 ? "checked" : ""} title="Diese behalten"></span>
          <span class="t-title">${esc(t.title)}</span>
          <span class="t-meta muted">${esc(t.artist)} · ${esc(t.path)}${i === 0 ? " · <b>behalten</b>" : ""}</span>
          <span class="t-dur"></span>
          <span class="t-actions"><button class="act danger" data-dupe-del="${gi}" data-remove="${t.track_id}" title="Diese entfernen (mit den übrigen)">✕</button></span>
        </div>`
        )
        .join("")}
      <div class="row dup-actions"><button class="ghost danger" data-dupe-apply="${gi}">Duplikate dieser Gruppe entfernen</button></div>
    </div>`
    )
    .join("");
}

$("reorg-dupes").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-dupe-apply]");
  if (!b) return;
  const gi = Number(b.dataset.dupeApply);
  const g = reorgState.plan?.duplicates[gi];
  if (!g) return;
  const keep = document.querySelector(`input[name="keep-${gi}"]:checked`);
  if (!keep) return toast("Keine Datei als „behalten“ gewählt", true);
  const remove = g.tracks.filter((t) => String(t.track_id) !== keep.value).map((t) => t.track_id);
  if (!remove.length) return;
  if (!confirm(`${remove.length} Datei(en) löschen? (Inhaltlich identisch mit „${g.tracks.find((t) => String(t.track_id) === keep.value)?.title}“)`)) return;
  try {
    const r = await api("/dl/reorg/dedupe", { method: "POST", body: JSON.stringify({ keep_id: Number(keep.value), remove_ids: remove }) });
    toast(`${r.removed} entfernt${r.reindex ? ", Reindex gestartet" : ""}`);
    await doReorgPlan();
    loadLibrary();
  } catch (err) {
    toast(err.message, true);
  }
});

$("reorg-plan").addEventListener("click", doReorgPlan);

$("reorg-apply").addEventListener("click", async () => {
  const p = reorgState.plan;
  if (!p?.moves.length) return;
  if (!confirm(`${p.moves.length} Dateien verschieben (${(p.move_bytes / 1048576).toFixed(1)} MB)? IDs und Einbettungen bleiben erhalten.`)) return;
  $("reorg-apply").disabled = true;
  try {
    const r = await api("/dl/reorg/apply", {
      method: "POST",
      body: JSON.stringify({ layout: reorgLayoutSpec(), collection: $("reorg-collection").value }),
    });
    toast(`${r.moved} verschoben${r.failed?.length ? ` · ${r.failed.length} fehlgeschlagen` : ""}${r.reindex ? ", Reindex gestartet" : ""}`);
    if (r.failed?.length) console.warn(r.failed);
    await doReorgPlan();
    loadLibrary();
  } catch (err) {
    toast(err.message, true);
  } finally {
    $("reorg-apply").disabled = false;
  }
});

$("reorg-suggest").addEventListener("click", async () => {
  const passList = reorgPassList();
  if (!passList.length) return;
  $("reorg-suggest").disabled = true;
  $("reorg-amb-count").textContent = `LLM liest ${passList.length} Titel ein (OpenRouter) …`;
  try {
    const r = await api("/dl/reorg/suggest", { method: "POST", body: JSON.stringify({ tracks: passList }) });
    reorgState.suggestions = r.suggestions || [];
    reorgState.accepted = new Set();
    renderSuggestions();
  } catch (err) {
    $("reorg-amb-count").textContent = `${passList.length} Titel brauchen Katalogarbeit.`;
    toast(err.message, true);
  } finally {
    $("reorg-suggest").disabled = isReadOnly();
  }
});

function renderSuggestions() {
  const passList = reorgPassList();
  const box = $("reorg-suggests");
  if (!reorgState.suggestions.length) {
    box.innerHTML = `<p class="muted">Keine Vorschläge erhalten.</p>`;
    $("reorg-apply-suggest").classList.add("hidden");
    return;
  }
  box.innerHTML = reorgState.suggestions
    .map((sg) => {
      const t = passList.find((a) => a.track_id === sg.track_id) || passList[sg.index] || passList.find((a) => a.index === sg.index) || {};
      const show = (k) => (sg[k] ? `<b>${esc(sg[k])}</b>` : `<span class="muted">${esc(t[k] || "—")}</span>`);
      return `
    <div class="pl-row">
      <span class="t-no"><input type="checkbox" data-sugg-acc="${sg.index}" data-sugg-id="${esc(t.track_id ?? "")}" title="Übernehmen"></span>
      <span class="t-title">${esc(t.title || sg.title || "?")}</span>
      <span class="t-meta muted">Künstler: ${show("artist")} · Album: ${show("album")} · Sprache: ${show("language")} · Genre: ${show("genre")}${sg.reason ? ` · <i>${esc(sg.reason)}</i>` : ""}</span>
      <span class="t-dur"></span><span class="t-actions"></span>
    </div>`;
    }).join("");
  $("reorg-apply-suggest").classList.toggle("hidden", isReadOnly());
}

document.addEventListener("change", (e) => {
  const c = e.target.closest("input[data-sugg-acc]");
  if (!c) return;
  const i = c.dataset.suggAcc;
  if (c.checked) reorgState.accepted.add(i);
  else reorgState.accepted.delete(i);
});

$("reorg-apply-suggest").addEventListener("click", async () => {
  if (!reorgState.accepted.size) return toast("Nichts akzeptiert (Häkchen setzen)", true);
  const passList = reorgPassList();
  const items = [...reorgState.accepted]
    .map((i) => {
      const sg = reorgState.suggestions.find((s) => String(s.index) === String(i));
      const t = passList[Number(i)] || passList.find((a) => String(a.index) === String(i));
      if (!sg || !t) return null;
      // [track_id, title, artist, album, genre, language] — empty string =
      // leave the field untouched (the old value stays).
      return [
        t.track_id,
        sg.title || t.title || "",
        sg.artist ?? "",
        sg.album ?? "",
        sg.genre ?? "",
        sg.language ?? "",
      ];
    })
    .filter(Boolean);
  if (!items.length) return;
  if (!confirm(`${items.length} Titel neu tagen? (schreibt ID3-Tags: Titel, Künstler, Album, Genre, Sprache — dann Rescan + Reindex)`)) return;
  try {
    const r = await api("/dl/reorg/apply-tags", { method: "POST", body: JSON.stringify({ items }) });
    toast(`${r.tagged} getaggt${r.reindex ? ", Reindex gestartet" : ""}`);
    reorgState.accepted.clear();
    renderSuggestions();
    doReorgPlan();
    loadLibrary();
  } catch (err) {
    toast(err.message, true);
  }
});

// ── recent (per-user) ────────────────────────────────────────────────────

async function loadRecent() {
  const box = $("recent-list");
  let r;
  try {
    r = await api("/dl/recent");
  } catch {
    return;
  }
  const items = r.items || [];
  if (!items.length) {
    box.innerHTML = `<p class="muted">Noch nichts für dich — abgespielte Titel und Downloads erscheinen hier.</p>`;
    return;
  }
  box.innerHTML = items
    .map(
      (e) => `
    <div class="pl-row">
      <span class="t-no">${e.kind === "download" ? "⬇" : ""}</span>
      <span class="t-title">${esc(e.title)}</span>
      <span class="t-meta muted">${esc(e.artist || "")} · ${esc(e.at || "")}</span>
      <span class="t-dur"></span>
      <span class="t-actions">${e.track_id != null ? `<button class="act" data-recent-play="${e.track_id}" title="Nochmal abspielen">▶</button>` : ""}</span>
    </div>`
    )
    .join("");
}

document.addEventListener("click", (e) => {
  const b = e.target.closest("button[data-recent-play]");
  if (b) playTrackId(Number(b.dataset.recentPlay));
});

function isReadOnly() {
  return window.__me && window.__me.role !== "admin";
}

// ── offline (device cache, the mosaic offline.js helper) ────────────────

let offlineList = JSON.parse(localStorage.getItem("aria-offline") || "[]");

function fmtSize(bytes) {
  if (bytes == null) return "–";
  const mb = bytes / 1048576;
  return mb >= 1 ? mb.toFixed(1) + " MB" : Math.max(1, Math.ceil(bytes / 1024)) + " KB";
}

function offlineBadge() {
  toolsOfflineCount = offlineList.length;
  setBadgeCount($("offline-count"), offlineList.length);
  updateToolsCount();
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

// ── offline mode (PWA: the SW serves the shell + last JSON catalog) ──────
// navigator.onLine tracks the device's connection; the SW makes the app
// survive it. Server-bound controls (downloads, enrichment, organizer
// writes) are disabled while offline.

const OFFLINE_TOOLS = [
  "src-search", "enrich-scan", "reorg-plan", "reorg-suggest", "pl-download-all", "yt-add",
];

function setOfflineMode(off) {
  $("offline-banner").classList.toggle("hidden", !off);
  document.body.classList.toggle("offline-mode", off);
  for (const id of OFFLINE_TOOLS) {
    const el = $(id);
    if (el) el.disabled = off;
  }
}

window.addEventListener("online", () => {
  setOfflineMode(false);
  toast("Wieder online");
});
window.addEventListener("offline", () => setOfflineMode(true));

// ── auth ──────────────────────────────────────────────────────────────────

async function boot() {
  let me;
  let netDown = false;
  try {
    me = await api("/dl/me");
  } catch (e) {
    // A TypeError (or a fetch-level failure) means the SERVER is
    // unreachable — different from a 401 (server fine, not logged in).
    netDown = e instanceof TypeError || /fetch|network|failed to fetch/i.test(String(e && e.message || ""));
    me = { ok: false };
  }
  if (!me.ok) {
    if (netDown) {
      // The SW may still serve a CACHED /dl/me on the next load; until
      // then: explain that login needs one online visit.
      setOfflineMode(true);
      $("login-error").textContent =
        "Keine Verbindung zum Server. Einmalig online anmelden — danach öffnet die App auch offline (mit letztem Stand + gespeicherten Titeln).";
    }
    $("login").classList.remove("hidden");
    $("login-user").focus();
    return;
  }
  setOfflineMode(!navigator.onLine);
  const u = me.user || {};
  window.__me = u;
  const who = u.name || u.user || "?";
  // The tenant dim is the app's GROUP (the household) — one for a long
  // time; the user dim is personal (playlists, recent).
  $("acc-name").textContent = who;
  $("acc-name").title = `Angemeldet als ${u.user} (${u.role || "?"}) — Gruppe ${u.tenant || "?"}`;
  $("acc-sub").textContent = [u.role || "", u.tenant ? `Gruppe ${u.tenant}` : ""].filter(Boolean).join(" · ");
  $("acc-avatar").textContent = (who || "?").trim().charAt(0).toUpperCase();
  $("logout").classList.remove("hidden");

  loadCollections();
  loadLibrary();
  loadPlaylists();
  refreshCookiesBadge();
  refreshCookiesStatus();
  renderQueue(await api("/dl/queue").catch(() => ({ items: [] })));
  renderOffline();
  startQueuePolling();
  refreshEnrichBadge();
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

// ── views (sidebar / bottom nav: Medien · Suche · Playlists · Werkzeuge) ──
//
// Few, purpose-based sections (Spotify/Plex style): "Medien" is the whole
// listening surface (home cards + library drill-down + album), "Werkzeuge"
// gathers downloads, offline, enrichment, organizer, sounds, YT-login.

let currentView = "home";
let currentTool = "queue";

function showView(name) {
  currentView = name;
  document.querySelectorAll(".sidebar button[data-view], #bottom-nav button[data-view]").forEach((x) =>
    x.classList.toggle("active", x.dataset.view === name)
  );
  document.querySelectorAll(".view").forEach((x) => x.classList.toggle("active", x.id === `view-${name}`));
  if (name === "search") {
    if (!searchHits.length) doSearch();
    $("search-q").focus();
  } else if (name === "playlists") {
    loadRecent();
  } else if (name === "tools") {
    showTool(currentTool);
  } else if (name === "home") {
    stopDrill();
  }
  if (name !== "tools") stopSrcPreview();
}

function showTool(name) {
  currentTool = name;
  document.querySelectorAll("#tool-tabs button").forEach((x) =>
    x.classList.toggle("active", x.dataset.tool === name)
  );
  document.querySelectorAll(".tool").forEach((x) => x.classList.toggle("active", x.id === `tool-${name}`));
  if (name === "queue") refreshQueueNow();
  if (name === "offline") renderOffline();
  if (name === "reorg") loadReorgCollections();
  if (name === "sounds") loadSources();
  if (name === "enrich") refreshEnrichBadge();
  if (name !== "sounds") stopSrcPreview();
}

document.querySelectorAll(".sidebar button[data-view], #bottom-nav button[data-view]").forEach((b) =>
  b.addEventListener("click", () => showView(b.dataset.view))
);
document.querySelectorAll("#tool-tabs button[data-tool]").forEach((b) =>
  b.addEventListener("click", () => showTool(b.dataset.tool))
);

// ── browse (home cards, library drill-down, album view) ─────────────────
//
// Spotify/Plex style: the "Medien" view opens on a home of card rows
// (Sprachen, Genres, top Künstler). Clicking a card drills down —
// language → genre → albums → album detail. The old flat
// artist/album/details-list is kept as the "Künstler" drill mode.

let drill = { mode: "folders", path: [] };   // path = ["Deutsch", "Geistliches"]
let albumState = null;                        // {collection, artist, album} | null

function allLibraryTracks() {
  return (
    library?.collections?.flatMap((c) => c.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks))) || []
  );
}

async function loadLibrary() {
  try {
    library = await api("/dl/library");
  } catch (e) {
    $("home-rows").innerHTML = `<p class="error">${esc(e.message)}</p>`;
    return;
  }
  const n = allLibraryTracks().length;
  $("lib-total").textContent = n ? `${n} Titel` : "";
  updateToolsCount();
  renderHome();
}

// ── art resolution: /art/<id> (embedded) → /art/cache/<key> (fetched) →
// generated gradient tile (name-derived, always available offline). ──────
function tileColor(seed) {
  let h = 0;
  for (const c of seed) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  const hue = h % 360;
  const hue2 = (hue + 40 + (h % 50)) % 360;
  return `linear-gradient(135deg, hsl(${hue} 45% 30%), hsl(${hue2} 55% 18%))`;
}

function artImgHtml(name, srcs) {
  // srcs: ordered candidate URLs; the last "src" is the tile sentinel.
  let chain = srcs
    .map((u, i) => {
      const next =
        i + 1 < srcs.length
          ? `this.src=${JSON.stringify(srcs[i + 1])}`
          : `tileify(this, ${JSON.stringify(name)})`;
      return `onerror="this.onerror=null;${next}"`;
    })
    .join(" ");
  const first = srcs.length ? srcs[0] : "data:,";
  return `<img class="card-art" loading="lazy" src="${first}" ${chain} alt="">`;
}

function tileify(img, name) {
  const d = document.createElement("div");
  d.className = "card-art tile";
  d.style.background = tileColor(name);
  d.textContent = (name || "?").trim().charAt(0).toUpperCase();
  img.replaceWith(d);
}

function albumArtSrcs(album, key) {
  const srcs = [];
  if (album.art_id) srcs.push(`/art/${album.art_id}`);
  if (key) srcs.push(`/art/cache/${encodeURIComponent(key)}`);
  return srcs;
}

// A card: cover + name + sub + optional hover play.
function cardHtml(opts) {
  const { name, sub, art, play, data } = opts;
  const playBtn = play
    ? `<button class="card-play" data-play-ids="${play}" title="Abspielen">▶</button>`
    : "";
  return `<div class="card" data-card="${data || ""}">
    <div class="card-cover">${art || `<div class="card-art tile" style="background:${tileColor(name)}">${(name || "?").charAt(0).toUpperCase()}</div>`}${playBtn}</div>
    <div class="card-name" title="${esc(name)}">${esc(name)}</div>
    ${sub ? `<div class="card-sub muted">${esc(sub)}</div>` : ""}
  </div>`;
}

// ── home ──────────────────────────────────────────────────────────────────
function stopDrill() {
  drill = { mode: "folders", path: [] };
  albumState = null;
  $("library-drill").classList.add("hidden");
  $("album-view").classList.add("hidden");
  $("home-hero").classList.remove("hidden");
  $("home-rows").classList.remove("hidden");
}

function renderHome() {
  const box = $("home-rows");
  const hero = $("home-hero");
  if (!library.collections?.length) {
    hero.innerHTML = "";
    box.innerHTML = `<p class="muted">Die Bibliothek ist leer.</p>`;
    return;
  }
  const hour = new Date().getHours();
  const greet = hour < 5 ? "Gute Nacht" : hour < 11 ? "Guten Morgen" : hour < 18 ? "Guten Tag" : "Guten Abend";
  hero.innerHTML = `<div class="hero-greet"><h1>${greet}.</h1><p class="muted">Deine Musik, deine Predigten — ${library.total || allLibraryTracks().length} Titel in ${library.collections.length} Sammlungen.</p></div>`;

  const rows = [];
  const musik = library.collections.find((c) => c.name === "musik");
  if (musik) {
    // Sprachen (top-level folders of the reorged tree)
    const langs = folderLevels(musik, 1);
    if (langs.length)
      rows.push(rowHtml("Sprachen", langs.slice(0, 12).map((l) =>
        cardHtml({
          name: l.name,
          sub: `${l.count} Titel`,
          art: l.art,
          play: l.ids.join(","),
          data: `folder:${encodeURIComponent(l.name)}`,
        })
      )));
    // Genres (second level of the folder tree, aggregated across languages)
    const genres = folderLevels(musik, 2);
    if (genres.length)
      rows.push(rowHtml("Genres", genres.slice(0, 18).map((g) =>
        `<span class="chip" data-chip-genre="${encodeURIComponent(g.name)}">${esc(g.name)} <small>${g.count}</small></span>`
      ), true));
    // Top artists
    const arts = [...musik.artists].sort((a, b) => trackCount(b) - trackCount(a)).slice(0, 24);
    if (arts.length)
      rows.push(rowHtml("Künstler", arts.map((a) =>
        cardHtml({
          name: a.name,
          sub: `${trackCount(a)} Titel`,
          art: a.art_id ? artImgHtml(a.name, [`/art/${a.art_id}`]) : null,
          play: artistIds(a).join(","),
          data: `artist:${encodeURIComponent(a.name)}`,
        })
      )));
  }
  // Playlists shortcut row
  if (playlistsCache.length)
    rows.push(rowHtml("Playlists", playlistsCache.slice(0, 12).map((p) =>
      `<div class="card card-playlist" data-pl-open="${p.id}">
        <div class="card-cover pl-cover"><span>≡</span></div>
        <div class="card-name">${esc(p.name)}</div>
        <div class="card-sub muted">${p.items ? p.items.length : 0} Titel</div>
      </div>`
    )));
  box.innerHTML = rows.join("");
}

function rowHtml(title, cards, compact) {
  return `<section class="home-row${compact ? " compact" : ""}">
    <h2>${esc(title)}</h2>
    <div class="row-cards">${cards.join("")}</div>
  </section>`;
}

function trackCount(artist) {
  return artist.albums.reduce((n, al) => n + al.tracks.length, 0);
}
function artistIds(artist) {
  return artist.albums.flatMap((al) => al.tracks.map((t) => t.id));
}

// Aggregate tracks by folder level (rel_path segments, collection prefix
// dropped): level 1 = Sprache, level 2 = Genre.
function folderLevels(collection, level) {
  const tracks = collection.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks));
  const map = new Map();
  for (const t of tracks) {
    let segs = (t.rel_path || "").split("/").filter(Boolean);
    if (segs.length > 1 && segs[0] === collection.name) segs = segs.slice(1);
    const key = segs.slice(0, level).join(" / ") || "(root)";
    let e = map.get(key);
    if (!e) {
      e = { name: key.split(" / ").pop(), count: 0, ids: [], art: null };
      map.set(key, e);
    }
    e.count++;
    e.ids.push(t.id);
    if (!e.art && t.has_art) e.art = artImgHtml(key, [`/art/${t.id}`]);
  }
  return [...map.values()].sort((a, b) => b.count - a.count || a.name.localeCompare(b.name, "de"));
}

// ── drill-down (card grid) ────────────────────────────────────────────────
function openDrill(mode, path) {
  drill = { mode, path: path || [] };
  albumState = null;
  $("home-hero").classList.add("hidden");
  $("home-rows").classList.add("hidden");
  $("album-view").classList.add("hidden");
  $("library-drill").classList.remove("hidden");
  renderDrill();
}

function stopAlbum() {
  albumState = null;
  $("album-view").classList.add("hidden");
  if (drill.path.length || currentView === "home") {
    $("library-drill").classList.remove("hidden");
  }
}

function renderDrill() {
  const crumb = $("drill-crumb");
  const grid = $("drill-grid");
  const musik = library.collections.find((c) => c.name === "musik");
  const parts = [{ label: "Medien", act: () => stopDrill() }];
  if (drill.mode === "artists") parts.push({ label: "Künstler", act: () => openDrill("artists", []) });
  else if (drill.mode === "genre") parts.push({ label: drill.path[0], act: () => stopDrill() });
  else drill.path.forEach((p, i) =>
    parts.push({ label: p, act: () => openDrill("folders", drill.path.slice(0, i + 1)) })
  );
  crumb.innerHTML = parts
    .map((p, i) => `<button class="crumb" data-crumb="${i}">${esc(p.label)}</button>`)
    .join('<span class="crumb-sep">/</span>');
  crumb.querySelectorAll("[data-crumb]").forEach((b) =>
    b.addEventListener("click", () => parts[Number(b.dataset.crumb)].act())
  );

  const cards = drill.mode === "genre" ? genreCards(musik, drill.path[0]) : drillCards(musik);
  grid.innerHTML = cards.length
    ? cards.join("")
    : `<p class="muted">Keine Alben in diesem Genre — zurück zum Start.</p>`;
}

// Albums across all languages whose second folder level is this genre.
function genreCards(musik, genre) {
  if (!musik) return [];
  const tracks = musik.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks));
  const seen = new Map();
  for (const t of tracks) {
    let segs = (t.rel_path || "").split("/").filter(Boolean);
    if (segs.length > 1 && segs[0] === "musik") segs = segs.slice(1);
    if (segs[1] !== genre) continue;
    const key = `${t.artist}¦${t.album}`;
    if (!seen.has(key)) seen.set(key, { artist: t.artist, album: t.album, lang: segs[0], tracks: [] });
    seen.get(key).tracks.push(t);
  }
  return [...seen.values()].sort((a, b) => (a.lang + a.artist).localeCompare(b.lang + b.artist, "de")).map((al) =>
    cardHtml({
      name: al.album || "(ohne Album)",
      sub: `${al.lang} · ${al.artist}`,
      art: (al.tracks.find((t) => t.has_art) || {}).has_art
        ? artImgHtml(al.album, [`/art/${al.tracks.find((t) => t.has_art).id}`])
        : null,
      play: al.tracks.map((t) => t.id).join(","),
      data: `album:${encodeURIComponent(al.artist)}¦${encodeURIComponent(al.album)}`,
    })
  );
}

function drillCards(musik) {
  if (!musik) return [];
  const { mode, path } = drill;
  if (mode === "artists") {
    const arts = [...musik.artists].sort((a, b) => a.name.localeCompare(b.name, "de"));
    return arts.map((a) =>
      cardHtml({
        name: a.name,
        sub: `${trackCount(a)} Titel · ${a.albums.length} Alben`,
        art: a.art_id ? artImgHtml(a.name, [`/art/${a.art_id}`]) : null,
        play: artistIds(a).join(","),
        data: `artist:${encodeURIComponent(a.name)}`,
      })
    );
  }
  // folders mode: walk the rel_path tree to the current depth.
  // Layout: L1 Sprache → L2 Genre → L3 Künstler → L4 Album → files.
  const tracks = musik.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks));
  if (path.length >= 3) {
    // at artist depth: show the ALBUMS living in this artist folder
    const seen = new Map();
    for (const t of tracks) {
      let segs = (t.rel_path || "").split("/").filter(Boolean);
      if (segs.length > 1 && segs[0] === "musik") segs = segs.slice(1);
      if (segs.slice(0, 3).join("") !== path.join("")) continue;
      const key = `${t.artist}¦${t.album}`;
      if (!seen.has(key)) seen.set(key, { artist: t.artist, album: t.album, tracks: [] });
      seen.get(key).tracks.push(t);
    }
    return [...seen.values()].map((al) =>
      cardHtml({
        name: al.album || "(ohne Album)",
        sub: al.artist,
        art: (al.tracks.find((t) => t.has_art) || {}) .has_art
          ? artImgHtml(al.album, [`/art/${al.tracks.find((t) => t.has_art).id}`])
          : null,
        play: al.tracks.map((t) => t.id).join(","),
        data: `album:${encodeURIComponent(al.artist)}¦${encodeURIComponent(al.album)}`,
      })
    );
  }
  // otherwise: the folders at the next level
  const levels = folderLevelsAt(musik, path);
  return levels.map((l) =>
    cardHtml({
      name: l.name,
      sub: `${l.count} Titel`,
      art: l.art,
      play: l.ids.join(","),
      data: `folder:${encodeURIComponent(l.full)}`,
    })
  );
}

function folderLevelsAt(musik, prefix) {
  const tracks = musik.artists.flatMap((a) => a.albums.flatMap((al) => al.tracks));
  const map = new Map();
  for (const t of tracks) {
    let segs = (t.rel_path || "").split("/").filter(Boolean);
    if (segs.length > 1 && segs[0] === "musik") segs = segs.slice(1);
    if (prefix.length && segs.slice(0, prefix.length).join("\x00") !== prefix.join("\x00")) continue;
    const next = segs[prefix.length];
    if (!next) continue;
    const full = segs.slice(0, prefix.length + 1).join(" / ");
    let e = map.get(full);
    if (!e) {
      e = { name: next, full, count: 0, ids: [], art: null };
      map.set(full, e);
    }
    e.count++;
    e.ids.push(t.id);
    if (!e.art && t.has_art) e.art = artImgHtml(full, [`/art/${t.id}`]);
  }
  return [...map.values()].sort((a, b) => b.count - a.count || a.name.localeCompare(b.name, "de"));
}

// ── album detail ──────────────────────────────────────────────────────────
function openAlbum(artist, album) {
  const musik = library.collections.find((c) => c.name === "musik");
  const a = musik?.artists.find((x) => x.name === artist);
  const al = a?.albums.find((x) => x.name === album);
  if (!al) return;
  albumState = { artist, album };
  $("home-hero").classList.add("hidden");
  $("home-rows").classList.add("hidden");
  $("library-drill").classList.add("hidden");
  $("album-view").classList.remove("hidden");
  const ids = al.tracks.map((t) => t.id).join(",");
  const art = al.art_id ? artImgHtml(album, [`/art/${al.art_id}`, `/art/cache/${encodeURIComponent("musik|"+artist+"|"+album)}`]) : null;
  $("album-hero").innerHTML = `
    <div class="al-cover">${art || `<div class="card-art tile" style="background:${tileColor(album)}">${album.charAt(0).toUpperCase()}</div>`}</div>
    <div class="al-meta">
      <div class="al-kind muted">Album</div>
      <h1>${esc(album)}</h1>
      <div class="al-artist">${esc(artist)}${al.year ? ` <span class="muted">· ${esc(al.year)}</span>` : ""}</div>
      <div class="al-count muted">${al.tracks.length} Titel</div>
      <div class="row al-actions">
        <button class="big" data-act="playall-album" data-artist="${esc(artist)}" data-album="${esc(album)}">▶ Album abspielen</button>
        <button class="ghost" data-act="pladd-album" data-artist="${esc(artist)}" data-album="${esc(album)}">＋ Zur Playlist</button>
      </div>
    </div>`;
  $("album-tracks").innerHTML = al.tracks.map((t) => trackRowHtml(t)).join("");
  const back = () => openDrill("artists", []);
  const b = document.createElement("button");
  b.className = "ghost al-back";
  b.textContent = "←";
  b.title = "Zurück";
  b.addEventListener("click", back);
  $("album-hero").prepend(b);
}

function playIds(ids) {
  const byId = new Map(allLibraryTracks().map((t) => [t.id, t]));
  const list = ids.map((id) => byId.get(Number(id))).filter(Boolean);
  if (list.length) playContext(list, 0);
}

// Card grid click handling (delegated): play buttons + card navigation.
$("home-rows").addEventListener("click", (e) => handleCardClick(e));
$("drill-grid").addEventListener("click", (e) => handleCardClick(e));

function openGenre(name) {
  drill = { mode: "genre", path: [name] };
  albumState = null;
  $("home-hero").classList.add("hidden");
  $("home-rows").classList.add("hidden");
  $("album-view").classList.add("hidden");
  $("library-drill").classList.remove("hidden");
  renderDrill();
}

function handleCardClick(e) {
  const chip = e.target.closest("[data-chip-genre]");
  if (chip) {
    openGenre(decodeURIComponent(chip.dataset.chipGenre));
    return;
  }
  const playBtn = e.target.closest("[data-play-ids]");
  if (playBtn) {
    e.stopPropagation();
    playIds(playBtn.dataset.playIds.split(",").filter(Boolean));
    return;
  }
  const plOpen = e.target.closest("[data-pl-open]");
  if (plOpen) {
    openPlaylistView(Number(plOpen.dataset.plOpen));
    showView("playlists");
    return;
  }
  const card = e.target.closest(".card[data-card]");
  if (!card || !card.dataset.card) return;
  const ci = card.dataset.card.indexOf(":");
  const kind = card.dataset.card.slice(0, ci);
  const raw = card.dataset.card.slice(ci + 1);
  const val = decodeURIComponent(raw || "");
  if (kind === "folder") {
    // folder card: drill one level deeper into this path
    const path = val.split(" / ");
    if (path.length >= 3) {
      // clicking a deep folder re-renders albums for it
      openDrill("folders", path);
    } else {
      openDrill("folders", path);
    }
  } else if (kind === "artist") {
    // artist card → that artist's albums
    openArtistAlbums(val);
  } else if (kind === "album") {
    const [artist, album] = val.split("¦");
    openAlbum(artist, album);
  }
}

function openArtistAlbums(artist) {
  const musik = library.collections.find((c) => c.name === "musik");
  const a = musik?.artists.find((x) => x.name === artist);
  if (!a) return;
  $("home-hero").classList.add("hidden");
  $("home-rows").classList.add("hidden");
  $("album-view").classList.add("hidden");
  $("library-drill").classList.remove("hidden");
  $("drill-crumb").innerHTML = `<button class="crumb" data-act-crumb="artists">Medien</button><span class="crumb-sep">/</span><button class="crumb" data-act-crumb="artists">Künstler</button><span class="crumb-sep">/</span><b>${esc(artist)}</b>`;
  $("drill-crumb").querySelectorAll("[data-act-crumb]").forEach((b) =>
    b.addEventListener("click", () => openDrill("artists", []))
  );
  $("drill-grid").innerHTML = a.albums
    .map((al) =>
      cardHtml({
        name: al.name || "(ohne Album)",
        sub: al.year || `${al.tracks.length} Titel`,
        art: al.art_id ? artImgHtml(al.name, [`/art/${al.art_id}`]) : null,
        play: al.tracks.map((t) => t.id).join(","),
        data: `album:${encodeURIComponent(a.name)}¦${encodeURIComponent(al.name)}`,
      })
    )
    .join("");
}

// ── browse actions (restored global dispatcher + album/playlist helpers) ──
document.addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-act]");
  if (!b) return;
  const act = b.dataset.act;
  if (act === "play") playTrackId(Number(b.dataset.id));
  else if (act === "pladd") addTrackToPlaylist(Number(b.dataset.id));
  else if (act === "playall-ids") playIds(b.dataset.ids.split(",").filter(Boolean));
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
  if (!q) {
    $("search-results").innerHTML = `<p class="muted">Tippe oben in die Suchleiste — Titel, Künstler oder Album, z. B. „Oceans“.</p>`;
    return;
  }
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

$("search-q").addEventListener("keydown", (e) => e.key === "Enter" && doSearch());
$("search-q").addEventListener("focus", () => showView("search"));
$("search-mode").addEventListener("click", (e) => {
  if (e.target.closest("button[data-mode]")) showView("search");
});

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

function setBadgeCount(el, n) {
  if (!el) return;
  el.textContent = String(n);
  el.classList.toggle("hidden", n === 0);
}

let toolsQueueCount = 0;
let toolsOfflineCount = 0;
function updateToolsCount() {
  const n = toolsQueueCount + toolsOfflineCount;
  setBadgeCount($("tools-count"), n);
  setBadgeCount($("bn-tools-count"), n);
}

async function loadPlaylists(openId = null) {
  try {
    const r = await api("/dl/playlists");
    playlistsCache = r.playlists || [];
  } catch {
    playlistsCache = [];
  }
  // The tab badge counts YOUR playlists (the shared section is on top).
  setBadgeCount($("pl-count"), playlistsCache.filter((p) => p.is_mine).length);
  setBadgeCount($("bn-pl-count"), playlistsCache.filter((p) => p.is_mine).length);
  if (openId) return openPlaylistView(openId);
  renderPlList();
}

function plCardHtml(p) {
  const shared = !!p.shared;
  return `
    <div class="pl-card" data-pl="${p.id}">
      <div class="pl-card-main">
        <div class="pl-card-name">${esc(p.name)}${p.is_mine ? "" : ` <span class="owner-chip">von ${esc(p.owner)}</span>`}</div>
        <div class="muted">${p.count} Titel${shared ? " · 👥 geteilt" : ""}${p.updated_at ? " · " + esc(p.updated_at) : ""}</div>
      </div>
      ${
        p.is_mine
          ? `<button class="share-toggle ${shared ? "on" : ""}" data-share="${p.id}" title="${shared ? "Teilen aufheben" : "Mit der Gruppe teilen"}">${shared ? "👥" : "👤"}</button>`
          : `<span class="share-mark" title="Mit der Gruppe geteilt">👥</span>`
      }
    </div>`;
}

function renderPlList() {
  const mine = playlistsCache.filter((p) => p.is_mine);
  const shared = playlistsCache.filter((p) => !p.is_mine);
  $("pl-mine").innerHTML = mine.length
    ? mine.map(plCardHtml).join("")
    : `<p class="empty">🎵 Noch keine Playlists. Oben einen Namen eingeben und „Erstellen“ — mit dem 👥-Knopf teilst du sie aktiv mit der Gruppe.</p>`;
  $("pl-shared-title").classList.toggle("hidden", shared.length === 0);
  $("pl-shared").innerHTML = shared.map(plCardHtml).join("");
  $("pl-list").classList.remove("hidden");
  $("pl-detail").classList.add("hidden");
}

async function openPlaylistView(id) {
  const r = await api(`/dl/playlists/${id}`);
  openPlaylist = r.playlist;
  $("pl-list").classList.add("hidden");
  $("pl-detail").classList.remove("hidden");
  $("pl-name").textContent = openPlaylist.name;
  const bits = [`${(openPlaylist.items || []).length} Titel`, `von ${openPlaylist.owner}`];
  if (openPlaylist.shared) bits.push("👥 mit der Gruppe geteilt");
  $("pl-meta").textContent = bits.join(" · ");
  $("pl-desc").textContent = openPlaylist.description || "";
  // Group members may play a shared playlist — editing stays with the owner.
  const canEdit = openPlaylist.is_mine || !isReadOnly();
  $("pl-edit-row").classList.toggle("hidden", !canEdit);
  $("pl-share-toggle").textContent = openPlaylist.shared ? "👤 Teilen aufheben" : "👥 Mit Gruppe teilen";
  renderPlItems(canEdit);
}

function renderPlItems(canEdit = true) {
  const box = $("pl-items");
  const items = openPlaylist?.items || [];
  if (!items.length) {
    box.innerHTML = `<p class="empty">📀 Leere Playlist — Titel über „Suche“ oder die Bibliothek hinzufügen.</p>`;
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
        ${canEdit ? `
        <button class="act" data-act="pl-up" data-pos="${i}" title="Nach oben">↑</button>
        <button class="act" data-act="pl-down" data-pos="${i}" title="Nach unten">↓</button>
        <button class="act danger" data-act="pl-del" data-pos="${i}" title="Entfernen">✕</button>` : ""}
      </span>
    </div>`
    )
    .join("");
}

async function toggleShare(p) {
  const next = !p.shared;
  try {
    await api(`/dl/playlists/${p.id}/share`, { method: "POST", body: JSON.stringify({ shared: next }) });
    p.shared = next;
    toast(next ? `„${p.name}“ ist jetzt mit der Gruppe geteilt` : `Teilen von „${p.name}“ aufgehoben`);
    if (openPlaylist && openPlaylist.id === p.id) openPlaylistView(p.id);
    else renderPlList();
  } catch (e) {
    toast(e.message, true);
  }
}

function plPlayableTracks() {
  return (openPlaylist?.items || [])
    .filter((it) => it.kind === "track" && it.track_id != null && it.playable !== false && !it.missing)
    .map((it) => ({ id: it.track_id, title: it.title, artist: it.artist || "" }));
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

$("pl-list").addEventListener("click", (e) => {
  const share = e.target.closest("[data-share]");
  if (share) {
    e.stopPropagation();
    const p = playlistsCache.find((x) => x.id === share.dataset.share);
    if (p) toggleShare(p);
    return;
  }
  const card = e.target.closest(".pl-card");
  if (card) openPlaylistView(card.dataset.pl).catch((err) => toast(`Playlist nicht geöffnet: ${err.message}`, true));
});

$("pl-share-toggle").addEventListener("click", () => {
  if (!openPlaylist) return;
  const p = { id: openPlaylist.id, name: openPlaylist.name, shared: openPlaylist.shared };
  toggleShare(p);
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
  toolsQueueCount = count;
  setBadgeCount($("queue-count"), count);
  updateToolsCount();
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
      const admin = window.__me?.role === "admin";
      return `<tr>
        <td class="muted">${it.track ?? n}</td>
        <td class="q-title" title="${esc(it.title)}">${esc(it.title)}</td>
        <td class="muted">${esc(it.artist)}${it.album ? " / " + esc(it.album) : ""}</td>
        <td class="muted">${esc(it.collection)}${admin && it.owner ? ` · <span title="Eigentümer">${esc(it.owner)}</span>` : ""}</td>
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


// ── free sound sources (search / preview / download) ─────────────────────
// Sources: Openverse (no key), Freesound + Pixabay (free API keys). The
// downloaded file lands in the `sounds` collection with license + source
// written into its tags.
let srcResults = [];
let srcPreviewAudio = null;
let srcPreviewUrl = null;

function stopSrcPreview() {
  if (srcPreviewAudio) { srcPreviewAudio.pause(); srcPreviewAudio.src = ""; }
  srcPreviewUrl = null;
}

async function loadSources() {
  const sel = $("src-source");
  if (sel.options.length) return;
  try {
    const r = await api("/dl/sources");
    sel.innerHTML = r.sources
      .map((x) => `<option value="${esc(x.name)}"${x.enabled ? "" : " disabled"}>${esc(x.name)}${x.enabled ? "" : " (Key fehlt)"}</option>`)
      .join("");
  } catch (e) {
    toast(e.message, true);
  }
}

function fmtDur(sec) {
  if (!sec) return "";
  const s = Math.round(sec);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function srcRowHtml(t, i) {
  return `
  <div class="pl-row">
    <span class="t-no"></span>
    <span class="t-title">${esc(t.title || "(unbetitelt)")}</span>
    <span class="t-meta muted">${esc(t.creator || "—")}${fmtDur(t.duration_sec) ? " · " + fmtDur(t.duration_sec) : ""} · <span class="lic" title="${esc(t.license || "")}">${esc(t.license || "?")}</span></span>
    <span class="t-dur"></span>
    <span class="t-actions">
      <button class="act" data-src-prev="${i}" title="Anhören">${srcPreviewUrl === (t.preview_url || t.url) ? "⏸" : "▶"}</button>
      <button class="act ghost" data-src-dl="${i}" title="In die Bibliothek laden">⤓ Laden</button>
    </span>
  </div>`;
}

async function doSourceSearch() {
  const q = $("src-q").value.trim();
  if (!q) return toast("Suchbegriff eingeben", true);
  stopSrcPreview();
  $("src-search").disabled = true;
  $("src-summary").textContent = "Suche läuft …";
  try {
    const r = await api(
      `/dl/sources/search?source=${encodeURIComponent($("src-source").value)}` +
      `&q=${encodeURIComponent(q)}` +
      `&license=${encodeURIComponent($("src-license").value)}`
    );
    srcResults = r.results || [];
    $("src-summary").textContent = srcResults.length
      ? `${r.total ?? srcResults.length} Treffer — ${srcResults.length} angezeigt`
      : "Keine Treffer.";
    $("src-results").innerHTML = srcResults.length
      ? srcResults.map(srcRowHtml).join("")
      : `<p class="muted">—</p>`;
  } catch (e) {
    $("src-summary").textContent = "";
    srcResults = [];
    toast(e.message, true);
  } finally {
    $("src-search").disabled = false;
  }
}

function toggleSrcPreview(i) {
  const t = srcResults[i];
  if (!t) return;
  const url = t.preview_url || t.url;
  if (!url) return;
  if (srcPreviewUrl === url && srcPreviewAudio && !srcPreviewAudio.paused) {
    srcPreviewAudio.pause();
    srcPreviewUrl = null;
  } else {
    if (!srcPreviewAudio) srcPreviewAudio = new Audio();
    srcPreviewAudio.src = url;
    srcPreviewAudio.play().catch((e) => toast("Vorschau fehlgeschlagen: " + e.message, true));
    srcPreviewUrl = url;
  }
  $("src-results").innerHTML = srcResults.map(srcRowHtml).join("");
}

async function srcDownload(i) {
  const t = srcResults[i];
  if (!t) return;
  if (!confirm(`„${t.title}“ (${t.license || "?"}) in die Bibliothek (Sounds) laden?`)) return;
  try {
    const r = await api("/dl/sources/download", {
      method: "POST",
      body: JSON.stringify({
        url: t.url,
        title: t.title,
        creator: t.creator || "",
        license: t.license || "",
        attribution: t.landing_url ? `Quelle: ${t.landing_url}` : "",
      }),
    });
    toast(`Geladen: ${t.title}`);
    loadLibrary();
  } catch (e) {
    toast(e.message, true);
  }
}

$("src-search").addEventListener("click", doSourceSearch);
$("src-q").addEventListener("keydown", (e) => { if (e.key === "Enter") doSourceSearch(); });
$("src-results").addEventListener("click", (e) => {
  const p = e.target.closest("button[data-src-prev]");
  if (p) return toggleSrcPreview(Number(p.dataset.srcPrev));
  const d = e.target.closest("button[data-src-dl]");
  if (d) return srcDownload(Number(d.dataset.srcDl));
});

// ── enrichment (Katalog: MusicBrainz + Cover Art Archive) ────────────────
// Fill missing album covers/years from the internet. Flow: plan (which
// albums lack art) → per-album lookup (MusicBrainz match + CAA image,
// cached server-side) → user selects → apply (embed into the files).
let enrichAlbums = [];   // [{key, artist, album, year, tracks, has_art, state, entry, selected}]
let enrichRunning = false;
let enrichStop = false;

async function refreshEnrichBadge() {
  try {
    const r = await api("/dl/enrich/plan?collection=musik");
    enrichAlbums = (r.albums || []).map((a) => ({ ...a, state: "idle", entry: null, selected: false }));
    const missing = enrichAlbums.filter((a) => !a.has_art).length;
    const el = $("enrich-count");
    if (el) {
      el.textContent = String(missing);
      el.classList.toggle("hidden", missing === 0);
    }
    if (currentTool === "enrich") renderEnrichList();
  } catch {
    /* badge stays empty */
  }
}

function renderEnrichList() {
  const list = $("enrich-list");
  if (!list) return;
  if (!enrichAlbums.length) {
    list.innerHTML = `<p class="muted">Noch keine Daten — „Alben analysieren“.</p>`;
    return;
  }
  list.innerHTML = enrichAlbums
    .map((a, i) => enrichRowHtml(a, i))
    .join("");
}

function enrichRowHtml(a, i) {
  const done = a.state === "done";
  const hasArt = done && a.entry && a.entry.art_b64;
  const art = hasArt
    ? `<img class="en-art" src="data:${a.entry.art_mime || "image/jpeg"};base64,${a.entry.art_b64}" alt="">`
    : `<div class="en-art none" style="background:${tileColor(a.album)}">${esc((a.album || "?").charAt(0).toUpperCase())}</div>`;
  let match;
  if (a.state === "busy") match = `<span class="muted">suche …</span>`;
  else if (done && a.entry && a.entry.found)
    match = `<span class="muted">MusicBrainz: ${esc(a.entry.release_title || a.album)}${a.entry.year ? " · " + esc(a.entry.year) : ""} <small>(${Number(a.entry.score || 0).toFixed(2)})</small></span>`;
  else if (done) match = `<span class="muted">${esc((a.entry && a.entry.note) || "kein Treffer")}</span>`;
  else match = `<span class="muted">noch nicht gesucht</span>`;
  return `<div class="en-row">
    ${art}
    <div class="en-info">
      <b>${esc(a.album)}</b> <span class="muted">— ${esc(a.artist)} · ${a.tracks} Titel</span>
      <div>${match}</div>
    </div>
    <div class="en-actions">
      <label class="chk en-sel"><input type="checkbox" data-en-sel="${i}" ${a.selected ? "checked" : ""} ${hasArt ? "" : "disabled"}> einbetten</label>
      <button class="ghost" data-en-lookup="${i}" ${enrichRunning ? "disabled" : ""} title="Erneut suchen">${done ? "↻" : "🔍"}</button>
    </div>
  </div>`;
}

async function doEnrichLookup(i) {
  const a = enrichAlbums[i];
  if (!a) return;
  a.state = "busy";
  renderEnrichList();
  try {
    const r = await api("/dl/enrich/lookup", {
      method: "POST",
      body: JSON.stringify({ collection: "musik", artist: a.artist, album: a.album, key: a.key, force: a.state === "done" }),
    });
    a.entry = r.entry;
    a.state = "done";
  } catch (e) {
    a.entry = { found: false, note: e.message };
    a.state = "done";
  }
  renderEnrichList();
}

$("enrich-scan").addEventListener("click", async () => {
  if (enrichRunning) return;
  enrichRunning = true;
  enrichStop = false;
  $("enrich-scan").disabled = true;
  $("enrich-stop").classList.remove("hidden");
  const todo = enrichAlbums.filter((a) => !a.has_art && a.state !== "done");
  $("enrich-progress").textContent = todo.length
    ? `Suche ${todo.length} Alben (≈1 Sekunde pro Album, MusicBrainz-Rate) …`
    : "Keine Alben ohne Cover mehr zu suchen.";
  for (let i = 0; i < todo.length; i++) {
    if (enrichStop) break;
    $("enrich-progress").textContent = `Suche ${i + 1}/${todo.length}: ${todo[i].album}`;
    await doEnrichLookup(enrichAlbums.indexOf(todo[i]));
    await new Promise((r) => setTimeout(r, 1200)); // server paces anyway; keep UI calm
  }
  $("enrich-progress").textContent = enrichStop ? "Gestoppt." : "Fertig — Auswahl unten übernehmen.";
  enrichRunning = false;
  $("enrich-scan").disabled = false;
  $("enrich-stop").classList.add("hidden");
});

$("enrich-stop").addEventListener("click", () => { enrichStop = true; });

$("enrich-apply").addEventListener("click", async () => {
  const keys = enrichAlbums.filter((a) => a.selected && a.entry && a.entry.art_b64).map((a) => a.key);
  if (!keys.length) return toast("Kein Cover ausgewählt", true);
  if (!confirm(`Cover in ${keys.length} Album(e) in die Dateien einbetten (Jahr wird mitgeschrieben)?`)) return;
  const btn = $("enrich-apply");
  btn.disabled = true;
  try {
    const r = await api("/dl/enrich/apply", { method: "POST", body: JSON.stringify({ keys, update_year: true }) });
    toast(`Eingebettet: ${r.files} Datei(en) in ${r.applied} Album(en)` + (r.errors && r.errors.length ? ` — ${r.errors.length} Fehler` : ""));
    enrichAlbums.forEach((a) => { if (keys.includes(a.key)) { a.has_art = true; a.selected = false; } });
    btn.classList.add("hidden");
    refreshEnrichBadge();
    loadLibrary();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
  }
});

$("enrich-list").addEventListener("click", (e) => {
  const l = e.target.closest("[data-en-lookup]");
  if (l && !enrichRunning) return doEnrichLookup(Number(l.dataset.enLookup));
});

$("enrich-list").addEventListener("change", (e) => {
  const c = e.target.closest("[data-en-sel]");
  if (!c) return;
  enrichAlbums[Number(c.dataset.enSel)].selected = c.checked;
  const n = enrichAlbums.filter((a) => a.selected && a.entry && a.entry.art_b64).length;
  $("enrich-apply").classList.toggle("hidden", n === 0);
});
