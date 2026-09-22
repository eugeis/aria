# eugeis

**Amazon Alexa as the voice front-end for your [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) agent.**

Ask your Echo questions, discuss things, let the agent act (with voice
approvals), and play your own music library or internet radio — all served
from your hardware, in Rust, with no ffmpeg, no mpd, no external daemons.

```
            STT + TTS (free, low latency)
   Echo device ──────────────────────────────── Amazon
        │  transcripts in / SSML out
        ▼
   eugeis (this project)          ZeroClaw gateway (ws://…/ws/chat)
   ┌─────────────────────┐  WS   ┌──────────────────────────┐
   │ /alexa  (ASK v1)    │──────▶│ agent loop: memory,      │
   │ /stream (music)     │       │ tools, security, SOPs    │
   │ /art (cover)        │◀──────│ approval prompts (voice) │
   └─────────────────────┘       └──────────────────────────┘
```

- **Amazon is the ears and mouth.** The Echo transcribes your voice and speaks
  whatever eugeis returns as SSML. No STT/TTS engines in the box, and the
  audio path Amazon handles is the fastest one available.
- **ZeroClaw is the brain.** eugeis keeps a persistent WebSocket to the
  gateway's `/ws/chat` endpoint, so every question goes through the same
  agent, memory, tools, and security policy as your other channels. When the
  agent asks for tool approval, eugeis voices the prompt
  ("ZeroClaw needs your approval: the shell tool wants to run… say yes") and
  forwards your answer back.
- **Music stays self-hosted.** A pure-Rust library scanner (`lofty`) indexes
  your MP3/AAC files; playback is a chunked HTTP stream with Range support,
  so pause/resume/seek work exactly like any Alexa music skill. Internet
  radio presets are piped through the same stream server. Zero transcoding
  for MP3/AAC.

## What you can say

| Utterance | What happens |
|---|---|
| "Alexa, ask eugeis what's the weather" | Forwarded to the ZeroClaw agent; answer spoken |
| "Alexa, <anything unmatched>" | Fallback → agent (when ASK provides the transcript) |
| "play bohemian rhapsody" / "play dark side of the moon" / "play queen" / "play jazz" | Song / album / artist / genre from your library |
| "play rock fm" | Radio preset from config |
| "play" / "pause" / "stop" / "next" / "previous" | Playback control |
| "jump to 2:30" | Seek within the current track |
| "shuffle" / "repeat this song" / "repeat all" | Queue modes |
| "yes" / "no" after an approval prompt | Approve / deny the agent's tool call |

## How it is built

Rust workspace, four crates:

| Crate | Role |
|---|---|
| `eugeis-alexa` | ASK v1 protocol types, intent classification, response/SSML builders |
| `eugeis-audio` | Library scan (`lofty`), search, per-device player state, playback tokens |
| `eugeis-zeroclaw` | Gateway WS client: turns with timeout, partial-reply degradation, approval bridging |
| `eugeis` (root) | axum server: `/alexa`, `/stream/{token}`, `/art/{id}`, `/health`; config; TLS |

Design notes and trade-offs: [`docs/architecture.md`](docs/architecture.md).

## Quick start

### 1. ZeroClaw (the brain)

Running ZeroClaw with a gateway and an agent you want to talk to. A
lightweight `voice` agent with a fast model gives the best latency:

```bash
zeroclaw quickstart        # if you haven't set it up yet
zeroclaw service install && zeroclaw service start
```

Note the gateway URL/port and create a gateway token
(`zeroclaw` dashboard → settings, or the config).

### 2. eugeis (the voice)

```bash
git clone https://github.com/eugeis/eugeis
cd eugeis
cargo build --release

mkdir -p ~/.eugeis
cp config/eugeis.example.toml ~/.eugeis/config.toml
# edit: public_base_url, client_id, library paths, zeroclaw gateway/alias/token
export EUGEIS_ZC_TOKEN="***"   # or put it in config

./target/release/eugeis
```

In mock mode (`zeroclaw.mock = true`) it runs without ZeroClaw and echoes
your questions — useful for the first Echo tests.

### 3. Public endpoint (required)

Alexa calls your skill over HTTPS from the internet. Either:

- **Caddy (recommended)** — put [`deploy/Caddyfile`](deploy/Caddyfile) in
  `/etc/caddy/`, point the domain at your box, done (auto TLS).
- **Native TLS** — set `server.tls_cert` / `server.tls_key` in config.

`server.public_base_url` must be the exact https URL the Echo will reach.

### 4. Amazon developer account

1. [Create a skill](https://developer.amazon.com/alexa/console/ask) →
   *Custom* → name `Eugeis`.
2. Import [`skill/interaction-model.json`](skill/interaction-model.json)
   (skill builder → *Import from file*). The manifest
   [`skill/skill-manifest.json`](skill/skill-manifest.json) already enables
   the **Audio Player** interface; import it too and fix the icon URIs.
3. Endpoint URL: `https://<your-domain>/alexa`.
4. Copy the **Client ID** (`amzn1.ask.skill.…`) into
   `server.client_id` in config.
5. Test mode: add your Amazon account to *Test Accounts*, say
   **"Alexa, ask eugeis hello"**.

### 5. Run it as a service

[`deploy/eugeis.service`](deploy/eugeis.service) is a systemd unit
(adjust paths).

## Configuration

See [`config/eugeis.example.toml`](config/eugeis.example.toml) — every key
is documented there. Environment overrides: `EUGEIS_CONFIG`,
`EUGEIS_ZC_TOKEN`, `EUGEIS_GATEWAY`, `EUGEIS_PUBLIC_URL`, `RUST_LOG`.

State (per-device queues, position) persists in `~/.eugeis/state.json`.

## Music library

- Supported: **MP3** and **AAC** (`.m4a`, `.aac`, `.mp4`). Both are streamed
  natively by Echo devices, so no transcoding is needed.
- `.flac` is indexed (so it's searchable) but not playable in v0.1 — convert
  once if you want it ("…based on something that fits good to it": MP3 is the
  common denominator for device streaming).
- Artist/album/track/genre tags are read with `lofty`; folders are used as a
  fallback artist. Cover art is served at `/art/{id}` for Echo Show.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

- Mock the agent: `zeroclaw.mock = true`.
- Fake music for stream tests: any MP3 in a `paths` folder.
- The `/health` endpoint reports library size and active streams.

## Roadmap

- [ ] FLAC → MP3 on-the-fly transcode (feature-gated, `lame` FFI)
- [ ] HLS for radio stations that only offer `.m3u8`
- [ ] Multiple agents per room (device → agent mapping)
- [ ] Direct operational intents (status / restart / SOP triggers) via the
      gateway REST API
- [ ] Docker image

## License

MIT — see [LICENSE](LICENSE).
