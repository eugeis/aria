# Aria architecture (the Mosaic era)

This document replaces the v1 (ZeroClaw bridge) architecture doc. The short
version: **the brain became a Mosaic app; the ASK surface stayed hand-written
Rust; the folder collections became a RAG knowledge base.**

## 1. The division of labor

| Concern | Owner | Why |
|---|---|---|
| STT (speech → text) | Amazon | The ASK request carries the transcript; no local whisper needed for the Alexa path |
| TTS (text → speech) | Amazon | We return SSML; `outputSpeech` is spoken by the device |
| The ASK v1 protocol (intents, directives, SSML pipeline, client-id check) | `aria-ask` adapter | Raw JSON protocol handling + byte-level details are not declarative (Mosaic Law 1) |
| Audio streaming (`/stream/{token}`, Range resume, ID3v2 skip, piped radio, cover art) | `aria-ask` adapter | Byte-precise HTTP streaming with per-device tokens — not a CQRS command, not a workflow node |
| Per-device player state (queue/history/shuffle/repeat/resume) | `aria-ask` adapter | Driven by PlaybackController + PlaybackNotifier requests; a state machine, not a declarative aggregate (shuffles, offsets) |
| Library scanning (lofty tags, folder semantics, collections) | `aria-ask` adapter | The only component that reads audio files (besides the Echo itself) |
| Knowledge (media metadata RAG, sermon content RAG) | the brain (Mosaic `vector_dbs`) | `kind: entries` JSONL + `kind: docs` markdown; BM25 + local 256-dim vectors, meta filters, delta manifest |
| The LLM turns (ask, ask-bible, resolve-play) | the brain (Mosaic workflows) | **chat mode** (search nodes + one `llm` call) for RAG answers, **agent mode** (an `agent` node over the bible MCP, capped at one tool round) for Bible questions — all on OpenRouter |
| The Bible (verses, translations, search) | the bible app (its `/mcp`) | 500+ modules already ingested there; consumed via the brain's MCP client (env-bound bearer) |
| Identity, deploy, docs, MCP server, web chat, A2A | the brain (auto-derived) | A Mosaic app projects all of these from the tessera |

## 2. Why the brain is a Mosaic app (and not a ZeroClaw bridge)

v1 bridged to ZeroClaw's gateway WS (turn slots, approval frames, ~90 tools)
because ZeroClaw was the only "brain" with memory + tools + RAG. The Mosaic
AI stack provides exactly the needed subset, declaratively:

- **knowledge RAG** over the user's own folders (the `entries` source kind is
  purpose-built for scan-generated metadata: one JSONL line per item,
  `text` + `slug` + full `meta`),
- **a grounded LLM** (workflow `agent` nodes: the tool loop over KB search +
  MCP calls),
- **durable sessions / memory** (ADR 0128) if conversation memory is ever
  wanted per Echo,
- **MCP client** to the bible app (and to anything else that grows an MCP
  server),
- **A2A + a web chat lens** for free (the owner can chat with the brain from
  a browser without an Echo).

What ZeroClaw added that v2 does *not* need: tool *approvals* spoken over the
voice (no dangerous tool calls in a music/assistant skill) and ~90 general
tools. If the general agent is ever wanted back, Mosaic's `gateway.routes`
byte-passthrough covers ZeroClaw's REST/SSE surface with a 3-line tessera
declaration — no framework change.

The two genuine Mosaic gaps found while building this were added to Mosaic
itself (eugeis/mosaic):

1. **MCP client auth** — `app.mcp.servers[].auth_env`: an env var holding the
   bearer presented to a streamable-HTTP MCP server. Without it, the
   default-deny bible `/mcp` was unreachable from another app (the generated
   client sent no auth header at all).
2. **Agent node system prompt** — a node-level `system` prop on the
   workflow `agent` node (the voice persona). `run_agent` already honored
   `spec.system` (O4 subagents); the emitter just never set it.

## 3. The collections model

The user's media are **one or more folder collections, each with a kind**:

```
[collections]
[[collections.items]]  name = "rock"        kind = "music"      path = "/media/rock"
[[collections.items]]  name = "bible-audio" kind = "audiobook"  path = "/media/bible-audio"
[[collections.items]]  name = "sermons"     kind = "sermon"     path = "/media/sermons-audio"
```

- **music** — recursive; `root/Artist/Album/nn - title.mp3`. Tags win; the
  folder hierarchy fills missing artist (1st segment) / album (2nd segment);
  the filename fills a missing title (`01 - Title` prefix stripped).
- **audiobook** — one immediate subfolder per book; its files are the
  chapters (ordered by track number, then filename). Narrator = the artist
  tag. "Play \<book\>" starts at chapter one; "continue \<book\>" resumes
  the device's last chapter + offset (per-device, persisted in state.json).
- **sermon** — recursive; any audio file is one recording;
  `YYYY-MM-DD - Title` filenames keep the date out of the spoken title.

MP3 + AAC (m4a/m4b) stream to the Echo; FLAC is indexed (searchable) but not
playable.

## 4. The scan → knowledge pipeline

```
adapter rescan (startup, then every rescan_secs)
  │  lofty walk over the collections
  ▼
in-memory Library (deterministic sort → stable track ids)
  │  + radio presets from config
  ▼
media records (trk:/alb:/art:/rad:/abk:/abk:…:ch:/serm: slugs, all meta)
  │  sorted by slug, keys sorted, gzip mtime=0 (ADR 0156 form)
  ▼
/data/knowledge/media/media.jsonl.gz   (shared hostPath with the brain)
  │  unchanged file ⇒ no write (byte comparison)
  ▼  (on change)
POST http://127.0.0.1:3000/api/ops/reindex   (the adapter's service token)
  ▼
brain delta-ingest (ADR 0019: unchanged files keep entries + embeddings)
```

Fresh host: the image carries only the binaries (house pattern — the
knowledge base lives on the host, seeded once per `deploy/li7/SECRETS.md`).
The brain boots fine with an empty `knowledge/` (fail-soft ingest); the
adapter's scans fill in the real catalog — and an EMPTY scan never
publishes, so a missing media dir cannot wipe the catalog from the brain.

The slugs are the contract between the two halves: the brain's
`resolve-play` returns a slug; the adapter maps `trk:NNNN`/`serm:NNNN` →
track id, `alb:`/`art:`/`abk:` → the library's own lookups, `rad:` → the
radio presets.

## 5. The voice turn (the 8-second rule)

Alexa expects a skill response in ~8 s. The paths:

1. **Playback intents** (play/next/pause/…): pure adapter — deterministic
   resolver in µs, no LLM. (The LLM fallback for natural-language play
   requests budgets the same 7.5 s and degrades to "not found" — the
   deterministic path already ran first.)
2. **Ask / Fallback** (free speech): the adapter picks the **mode** by the
   utterance —
   * **Bible questions** (book names / "psalm" / "passage" / …, a cheap
     term heuristic in the adapter) → `ask-bible`, **agent mode**: one
     capped `agent` node over the bible MCP (`max_turns: 2` = one tool
     round + the answer). The model must *choose* the passage parameters,
     so it needs the tool loop — but capped, ~6 s worst case.
   * **Everything else** → `ask`, **chat mode**: two `search` nodes
     (media metadata + sermon scripts, both sub-second) → a `mapper` node
     merges the two results → **one** `llm` call. No tool loop, no
     runaway re-searching (that is what produced the 13 s empty answer),
     ~3 s measured end-to-end.

   On timeout/error the adapter speaks a canned line; the brain's own
   errors are typed (`{status: {body}}`) so the adapter can distinguish
   "unreachable" from "brain error".

All LLM calls run on OpenRouter (deepseek/deepseek-v4-flash — the
best preis/leistung in the tool-calling tier; a voice turn costs ~half a
cent). There is no vLLM: li7 has no access to the datalab GPU box.
There is no partial-streaming (v1 had it against the ZeroClaw WS); one
generation is fast enough for a short spoken answer, and the fallback
lines cover the tail.

## 6. Deployment (li7)

One pod, two containers (the bible shape — a generated chart + li7-only
glue injected by `deploy/li7/inject.rs`):

- **aria** (the brain, port 3000): `/api/*`, `/mcp`, `/a2a`, `/chat` (web
  lens), `/readyz`…
- **aria-ask** (the adapter, port 8090): `/alexa`, `/stream/*`, `/art/*`,
  `/health`

Ingress (Traefik, cert-manager `letsencrypt-prod`, the
`*.eisler-systems.de` wildcard): the three ASK paths → 8090, everything
else → 3000. Volumes: `host-data` (ext4, `/data` in **both** containers —
the brain ingests, the adapter publishes) + `host-vector` (ext4, `/vector`
in the brain — LanceDB cannot commit on exFAT) + `media` (hostPath, `/media`
in the adapter — the audio files). Secrets per `deploy/li7/SECRETS.md`
(env-bound, never in the repo/tessera).

## 7. Failure modes

| Failure | Behavior |
|---|---|
| Brain down (pod) | Adapter speaks "I can't reach my brain right now"; playback (deterministic) still works |
| LLM down / OpenRouter credits exhausted | `ask` / `ask-bible` return a typed brain error → the adapter speaks a canned line; `resolve-play` → "not found" (the deterministic path already ran). Playback stays fully working |
| Bible app down | The `ask` agent's bible tools fail per-call; the LLM answers from the local KBs or says it can't (the tools surface the error, the agent is told to admit it) |
| Missing secret (MCP bearer) | The bible MCP calls fail closed with a typed error naming the env var |
| Scan finds no files (empty media dir) | An empty scan **never publishes** — it cannot wipe the catalog the brain already has. The previous corpus stays until a real scan lands |
| FLAC / unsupported file | Indexed (searchable), "not streamable to the Echo" when play is requested |
| Wrong skill hitting `/alexa` | Empty 200 (client-id check) — no leakage, no error page |
| Stream token expired (>12 h) | 404 → the device's PlaybackFailed advances the queue (the old behavior, kept) |
