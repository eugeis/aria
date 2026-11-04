# Aria — your Echo, your hardware, your content

Aria is a **personal voice assistant for Amazon Alexa that runs entirely on
your hardware**: your own music and radio, your audiobooks, the Bible (via
the [bible](https://github.com/eugeis/bible) Mosaic app — 500+ translations),
your own sermons and seminar scripts, and a grounded LLM for everything else.

```
   Echo ──(Amazon STT)──> ASK JSON ──> https://aria.eisler-systems.de/alexa
                                                        │
                                              ┌─────────┴──────────┐
                                              │  aria-ask (adapter) │  hand-written Rust
                                              │  ASK protocol,      │  this repo: adapter/
                                              │  /stream/{token},   │
                                              │  player state, scan │
                                              └─────────┬──────────┘
                       loopback REST (adapter token)    │
                                               ┌─────────▼──────────┐
                                               │  aria (the brain)  │  a Mosaic app (tessera.yaml)
                                               │  `ask` (chat mode) │  ask = RAG over:
                                               │  `ask-bible` (agent│   • media KB (your scan metadata)
                                               │  `resolve-play`    │   • sermons KB (your scripts)
                                               │  workflows, RAG,   │  ask-bible = capped LLM agent ─┼──> bible app's /mcp
                                               │  MCP client        │   OpenRouter deepseek-v4-flash │     (verses, search)
                                               │  identity, deploy  │   (one call for ask;           │
                                               └────────────────────┘    one tool round for ask-bible)
   Echo <──(Amazon TTS speaks our SSML)── ASK response + AudioPlayer.Play(url=/stream/{token})
```

**Amazon is the ears and the mouth** (STT + TTS). **Everything else is yours.**

## What you can say

| You say | What happens |
|---|---|
| "Alexa, mein aria, spiel bohemian rhapsody" | Deterministic library resolve (album/artist/genre/fuzzy) → streams the MP3 |
| "…play queen" / "…play rock" | Artist / genre queue (shuffle-aware) |
| "…play psalmen" | The audiobook, from chapter one |
| "…continue psalmen" | Resumes the device's last chapter + offset |
| "…play the station rock fm" | Internet radio preset (piped through) |
| "…play something for working out" | The brain resolves it against your scan metadata (LLM + RAG) |
| "…play psalm 23" / "…play the sermon on grace" | Verse text (bible app) / your sermon recording, spoken or streamed |
| "…what did the sermon on grace say about receiving?" | Grounded answer over your sermon scripts |
| "…frage: was ist das wetter" / anything else | The LLM brain (grounded in your collections + the Bible app) |
| play / pause / stop / next / previous / jump to 2 minutes, shuffle, repeat one / all / off | The standard playback controls |

> **Honest caveat:** a custom skill cannot take over the built-in
> "Alexa, spiel X" (that requires being a certified music service). The
> invocation is **"mein aria …"** (en-US: "my aria …") — Amazon requires at
> least two words, and it is fixed once the skill is published. Say it
> first, then the command: "Alexa, mein aria, spiel …" / "…frage: …".

## Repository layout (the bible shape)

```
tessera.yaml            the BRAIN, declared (identity, model, the media + sermons
                        collections, the bible MCP client, the ask / resolve-play
                        workflows, the li7 deploy, the e2e suite)
knowledge/
  media/media.jsonl.gz  committed SAMPLE of the scan output (e2e + fresh boot);
                        the live one is generated on /data by the adapter
  sermons/              committed SAMPLE sermon script (the real ones are synced
                        from eugeis/sermons to /data/knowledge/sermons on li7)
  cassettes/aria-llm.json  the O10 LLM cassette — the e2e runs hermetically
                        (recorded responses, no network)
adapter/                the ASK SURFACE (hand-written Rust, its own workspace)
  src/                  main (serve/scan), router (ASK intents -> actions/brain),
                        stream (/alexa /stream /art), agent (the brain bridge),
                        scan (lofty walk -> media.jsonl.gz + reindex), config, state
  crates/aria-alexa/    the ASK v1 protocol (envelope, intents, SSML)
  crates/aria-audio/    the audio engine (library/collections, player, tokens, search)
  Dockerfile            the mosaic/aria-ask image
skill/
  interaction-model.json  the Alexa intent model (import in the dev console)
  skill-manifest.json     the skill manifest (endpoint + AUDIO_PLAYER)
deploy/li7/
  inject.rs           injects the aria-ask sidecar + media hostPath + ingress
                        split into the rendered helm chart
  SECRETS.md            the runner secret files + host dirs for li7
.github/workflows/
  ci.yml                brain: render+build+e2e (pinned MOSAIC_REV) · adapter: fmt+clippy+test
  deploy.yml            MANUAL: build+import both images, helm install, hostPath patch, smoke
docs/architecture.md    the design rationale
```

## The two halves

### The brain (tessera.yaml — the Mosaic app)

- **`media` collection** (`kind: entries`): the adapter's scan publishes one
  metadata record per track / album / artist / radio / audiobook / chapter /
  sermon recording — `text` = retrieval text, `slug` = stable id, everything
  else in `meta` (kind_media, collection, artist, album, book, file,
  duration_ms, …). Searchable via `POST /api/vectordb/media/search`
  (BM25 + local 256-dim vectors, meta filters) and the MCP
  `search_knowledge(kb=media)`.
- **`sermons` collection** (`kind: docs`): your sermon/seminar scripts as
  markdown — content RAG (themes, quotes, "what did it say about …").
- **`ask` workflow**: one voice turn — a tool-using LLM agent (short spoken
  answers by system prompt) over the two KBs + the bible app's MCP
  (`scripture_passage`, `search_knowledge`).
- **`resolve-play` workflow**: search over `media` + one structured LLM
  decision `{action, slug, reason}` — the fallback for natural-language play
  requests the deterministic resolver misses.
- **bible MCP client**: `app.mcp.servers[0]` = `https://bible.eisler-systems.de/mcp`,
  the bible `reader` bearer via `auth_env: MOAIC_MCP_BIBLE_AUTH` (env-bound —
  the credential never lands in the tessera or the generated source).
- **identity** (ADR 0012 default-deny): `admin` user (ops + the web chat
  lens), `adapter` service account (editor — runs the workflows + reindex).
- What you get for free with a Mosaic app: `POST /api/assistant/chat` (the
  web `/chat` lens), A2A (`/a2a`), the MCP server (`/mcp`), sessions,
  `/openapi.json`, the rendered Helm chart for li7.

### The adapter (aria-ask — the hand-written surface)

What a Mosaic app cannot declare (Law 1) and the Echo needs:

- **the ASK v1 protocol** — intent envelopes in, SSML + `AudioPlayer.Play`
  directives out, the skill client-id check;
- **raw audio streaming** — `/stream/{token}`: MP3/AAC progressive download
  with byte-precise resume (ID3v2 skip + ms→byte offset + HTTP Range merge),
  unguessable 48-hex tokens (12 h TTL) that ARE the stream URL, piped radio,
  `/art/{id}` cover art for Echo Show;
- **per-device player state** — queue, history (previous), shuffle, repeat,
  resume offset, persisted to `state.json`;
- **the scanner** — lofty tag reading over your collections (music /
  audiobook / sermon folder semantics), the deterministic
  `media.jsonl.gz` publisher, and the brain reindex trigger.

The ZeroClaw bridge of aria v1 is gone: the brain IS the Mosaic app. (If a
ZeroClaw surface is ever needed again, Mosaic's `gateway.routes` gives you a
byte-passthrough to its REST/SSE side with a 3-line tessera declaration.)

## Developing locally

```bash
# 1. the brain (needs the mosaic CLI — cargo build -p mosaic-cli in ../mosaic)
../mosaic/target/release/mosaic check .
../mosaic/target/release/mosaic build . -o out
(cd out && cargo build -p aria)
(cd out && ./target/debug/aria serve --bind 127.0.0.1:3000)   # + the e2e env vars, see the tessera

# 2. the e2e suite (hermetic — the O10 cassette replays the LLM calls)
../mosaic/target/release/mosaic test . -o out

# 3. the adapter
cd adapter && cargo test --workspace
# a standalone smoke (mock brain, a throwaway media dir): see /tmp/aria-smoke in the
# session notes, or:
./target/debug/aria-ask scan  --config /path/config.toml     # one scan + publish (+ reindex)
./target/debug/aria-ask serve --config /path/config.toml     # the ASK server (brain.mock = true)
```

The e2e cassette (`knowledge/cassettes/aria-llm.json`) is recorded against a
local mock provider; the e2e LLM base URL points at a dead local port on
purpose, so a missing/mismatched key fails fast instead of touching a real
provider. Re-record by running the three scenario requests against the app
with `MOAIC_LLM_CASSETTE` pointed at a fresh file and a live (or mock)
provider, then commit the result.

## Deploying (li7)

See **`deploy/li7/SECRETS.md`** (the runner secret files + the host
directories). Then run the **`deploy`** workflow (Actions → deploy →
Run workflow, `ref: main`). It:

1. renders the brain with the pinned `MOSAIC_REV`,
2. builds + imports `mosaic/aria:<sha>` and `mosaic/aria-ask:<sha>` into k3s,
3. runs `deploy/li7/inject.rs` (sidecar + media hostPath + service
   ports + the ingress path split: `/alexa`, `/stream`, `/art` → the adapter;
   everything else → the brain),
4. creates the secrets + the cert-manager certificate for
   `aria.eisler-systems.de` (the `*.eisler-systems.de` wildcard already points
   at li7's Traefik — no DNS work needed),
5. `helm upgrade --install` + the hostPath strategic-merge patch
   (`/data`, `/vector` on ext4; **re-apply after every upgrade** — the patch
   does not survive a re-render) + the rollout wait + in-pod smoke.

Drop audio into `/home/ee/audio/…` on li7 (the layout in SECRETS.md) and
`kubectl -n aria rollout restart deployment/aria` — the adapter's initial
scan publishes the metadata and the brain reindexes on its next boot.

## Registering with your Alexa account

The skill is a **custom skill** (not a certified music service — see the
caveat above). One-time console setup at [developer.amazon.com](https://developer.amazon.com)
→ Alexa Console → your developer account:

1. **Create the skill** (or open the existing one): Custom → Proactive/none
   are irrelevant — choose **Custom**, name it *Aria*.
2. **Endpoint**: set the HTTPS endpoint URI to
   `https://aria.eisler-systems.de/alexa` (must be a valid public TLS
   endpoint — the cert-manager cert makes that true). The console POSTs a
   test envelope there on save; it answers 200.
3. **Interaction model**: the skill supports two locales — en-US
   (`skill/interaction-model.json`) and de-DE
   (`skill/interaction-model-de-DE.json`). In the console select the
   locale, open the **JSON editor** tab, and paste the matching file
    (invocation name `mein aria`; PlayMusicIntent + AskIntent + shuffle/repeat/
    seek + the playback controllers + notifiers + Fallback). If your Echo is
   set to German, the **de-DE** locale is the one that matters — an
   en-US-only skill will not match on a German device.
4. **Manifest / publishing info**: under each locale set name *Aria* and
   the summary/description from `skill/skill-manifest.json` (both en-US and
   de-DE are written out there). The skill icons are already hosted —
   `https://aria.eisler-systems.de/skill-icons/icon-108.png` and
   `…/icon-512.png` (served by the adapter; baked into its binary). The
   manifest's `smallIconUri` / `largeIconUri` point at them.
5. **Enable the `AUDIO_PLAYER` interface** (Endpoint configuration →
   Interfaces). Required for `AudioPlayer.Play` + the playback notifiers.
6. **Pin the skill's Client ID** (so other skills can't hit the endpoint):
   copy the **Client ID** (`amzn1.ask.skill.…`, shown under Endpoint
   configuration) into the adapter's env —
   `kubectl -n aria set env deploy/aria -c aria-ask ARIA_CLIENT_ID=amzn1.ask.skill.…`
   (a `set env` triggers a rolling restart; the request check then rejects
   any skill with a different application id with an empty response).
   Leave it unset while you are still iterating — an empty value means the
   check is off.
7. **Test**: stay in the skill's **Development** stage — no certification
   needed for a personal skill; it works on your own account's Echo devices
   immediately. (Publishing to Production is optional and only makes sense
   for other people's accounts.)
 8. On the Echo: "Alexa, öffne mein aria" or "Alexa, mein aria, spiel …".

## Roadmap

- **Live TTS for text-only sermons** — the `StreamSource` registry already
  isolates the stream kinds; a `Tts` variant piping the voice-sidecar (the
  bible pod's Kokoro) output to the Echo would let any sermon script be read
  aloud on demand (pre-rendered audio files work today: drop them in a
  `sermon` collection).
- **A browser voice widget** — declare a `voice:` block (the bible pattern:
  local whisper + Kokoro sidecars) and the brain gains `/api/voice` +
  `/voice` for browser testing without the Echo.
- **ZeroClaw bridge** — `gateway.routes` (HTTP passthrough) if the general
  agent is ever wanted back behind the voice.
