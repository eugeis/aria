# aria

![CI](https://github.com/eugeis/aria/actions/workflows/ci.yml/badge.svg)

**Amazon Alexa as the voice front-end for your [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) agent.**

Ask your Echo questions, discuss things, let the agent act (with voice
approvals), and play your own music library or internet radio — all served
from your hardware, in Rust, with no ffmpeg, no mpd, no external daemons.

```
            STT + TTS (free, low latency)
    Echo device ──────────────────────────────── Amazon
         │  transcripts in / SSML out
         ▼
    aria (this project)          ZeroClaw gateway (ws://…/ws/chat)
    ┌─────────────────────┐  WS   ┌──────────────────────────┐
    │ /alexa  (ASK v1)    │──────▶│ agent loop: memory,      │
    │ /stream (music)     │       │ tools, security, SOPs    │
    │ /art (cover)        │◀──────│ approval prompts (voice) │
    └─────────────────────┘       └──────────────────────────┘
```

- **Amazon is the ears and mouth.** The Echo transcribes your voice and speaks
  whatever aria returns as SSML. No STT/TTS engines in the box, and the
  audio path Amazon handles is the fastest one available.
- **ZeroClaw is the brain.** aria keeps a persistent WebSocket to the
  gateway's `/ws/chat` endpoint, so every question goes through the same
  agent, memory, tools, and security policy as your other channels
  (Telegram, dashboard, …). When the agent asks for tool approval, aria
  voices the prompt ("ZeroClaw needs your approval: the shell tool wants to
  run… say yes") and forwards your answer back.
- **Music stays self-hosted.** A pure-Rust library scanner (`lofty`) indexes
  your MP3/AAC files; playback is a chunked HTTP stream with Range support,
  so pause/resume/seek work exactly like any Alexa music skill. Internet
  radio presets are piped through the same stream server. Zero transcoding
  for MP3/AAC.

## What you can say

| Utterance | What happens |
|---|---|
| "Alexa, ask aria what's the weather" | Forwarded to the ZeroClaw agent; answer spoken |
| "Alexa, \<anything unmatched\>" | Fallback → agent (when ASK provides the transcript) |
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
| `aria-alexa` | ASK v1 protocol types, intent classification, response/SSML builders |
| `aria-audio` | Library scan (`lofty`), search, per-device player state, playback tokens |
| `aria-zeroclaw` | Gateway WS client: turns with timeout, partial-reply degradation, approval bridging |
| `aria` (root) | axum server: `/alexa`, `/stream/{token}`, `/art/{id}`, `/health`; config; TLS |

Design notes and trade-offs: [`docs/architecture.md`](docs/architecture.md).

---

## Installation

**Target setup (the one this guide assumes):** one Linux VM that already runs
ZeroClaw (gateway + Telegram bot, …). aria is installed on that same VM, so
the gateway is local; only the Alexa-facing HTTPS port needs to be reachable
from the internet.

### What you need

| # | Thing | Why |
|---|---|---|
| 1 | Linux VM (x86_64) with ZeroClaw running | the brain; aria talks to its gateway over `ws://` |
| 2 | ZeroClaw **gateway enabled** with a known **port** and **bearer token** | `GET /ws/chat` endpoint |
| 3 | An agent alias in ZeroClaw to talk to | e.g. your main agent, or a dedicated fast `voice` agent (lower latency) |
| 4 | A music folder with **MP3/AAC** files (`.mp3`, `.m4a`, `.aac`, `.mp4`) | streamed natively to the Echo, no transcoding |
| 5 | A **public HTTPS URL** for the VM (domain + Caddy, or a tunnel) | Amazon calls the skill endpoint from the internet |
| 6 | A free **Amazon developer account** | to register the skill |
| 7 | Rust toolchain (≥ 1.85) — or use the prebuilt release binary | to build aria (see "Get the binary") |

### 1. Get the aria binary

Option A — prebuilt static binary (no Rust needed):

```bash
# from https://github.com/eugeis/aria/releases (aria-<ver>-x86_64-linux-musl.tar.gz)
curl -LO https://github.com/eugeis/aria/releases/latest/download/aria-0.1.0-x86_64-linux-musl.tar.gz
tar xzf aria-*.tar.gz
sudo install -m 755 aria /usr/local/bin/aria
```

Option B — build from source:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
git clone https://github.com/eugeis/aria
cd aria
cargo build --release        # -> target/release/aria
sudo install -m 755 target/release/aria /usr/local/bin/aria
```

Check it runs:

```bash
aria --help
```

### 2. Configure

```bash
sudo mkdir -p /etc/aria
sudo cp config/aria.example.toml /etc/aria/config.toml   # (from the repo, or the release tarball)
sudoedit /etc/aria/config.toml
```

The keys that matter for the VM setup:

```toml
[server]
bind = "127.0.0.1:8080"                    # Caddy sits in front (see step 4)
public_base_url = "https://<your-domain>"  # EXACT public URL the Echo will use
client_id = "amzn1.ask.skill.<...>"        # from the Alexa console (step 5); rejects other skills

[library]
paths = ["/srv/music"]                     # your music folder (MP3/AAC)
rescan_secs = 3600                         # rescan hourly; 0 = only at startup

[zeroclaw]
gateway = "ws://127.0.0.1:3000"            # your ZeroClaw gateway, same VM
# token = "…"                              # or: Environment=ARIA_ZC_TOKEN=… in the unit file
agent_alias = "voice"                      # agent to talk to (see note below)
turn_timeout_ms = 7500                     # Alexa answers must land within ~8 s

[radio.stations]                           # optional presets
"rock fm" = "https://…/stream.mp3"
```

Notes:

- **Gateway port & token** — from your ZeroClaw configuration (the same
  gateway your other channels use; the dashboard's settings page shows both).
  The token can live in the config or as `ARIA_ZC_TOKEN` in the systemd unit
  (preferred — keeps secrets out of the TOML).
- **Agent choice** — you can point `agent_alias` at the same agent your
  Telegram bot uses, or create a dedicated lightweight agent with a fast
  model for snappier voice answers.
- **`public_base_url`** — used to build the `/stream/…` and `/art/…` URLs
  handed to the Echo. It must be reachable from your Echo devices.

Environment overrides (systemd-friendly): `ARIA_CONFIG`, `ARIA_ZC_TOKEN`,
`ARIA_GATEWAY`, `ARIA_PUBLIC_URL`, `RUST_LOG`.

### 3. Quick sanity check (before exposing anything)

```bash
ARIA_CONFIG=/etc/aria/config.toml /usr/local/bin/aria
# in another shell:
curl -s localhost:8080/health | python3 -m json.tool
# {"ok":true,"library_tracks":…,"playable_tracks":…,…}
```

You can also drive the ASK endpoint by hand (mock mode, `zeroclaw.mock =
true`, needs no gateway at all):

```bash
curl -s -X POST localhost:8080/alexa -H 'content-type: application/json' -d '{
  "version":"1.0",
  "session":{"new":true,"sessionId":"s1","application":{"applicationId":"x"},"user":{"userId":"u"}},
  "context":{"System":{"device":{"deviceId":"laptop"}}},
  "request":{"type":"IntentRequest","requestId":"r1","locale":"en-US",
    "intent":{"name":"PlayMusicIntent","slots":{"query":{"name":"query","value":"queen"}}}}
}'
# -> JSON with an AudioPlayer.Play directive; the stream URL in it is
#    directly downloadable: curl -O <that-url>
```

Stop the test instance (Ctrl-C) before installing the service.

### 4. Public HTTPS endpoint (required by Amazon)

Amazon's servers must reach `/alexa` over HTTPS, and your Echo must reach
`/stream/…`. Pick one:

**A. Caddy on the VM (recommended, needs a domain pointing at the VM):**

```bash
sudo apt install caddy
# /etc/caddy/Caddyfile  (from deploy/Caddyfile — adjust the domain):
<your-domain> {
    encode gzip
    request_header -X-Forwarded-For
    reverse_proxy 127.0.0.1:8080
}
sudo systemctl reload caddy
```

Caddy obtains the Let's Encrypt certificate automatically. If your VM is
behind NAT without a port forward, use the tunnel variant:

**B. Cloudflare Tunnel (no open ports):**

```bash
# install cloudflared, `cloudflared tunnel login`, create tunnel "aria"
# config: ingress -> http://127.0.0.1:8080 for your domain
sudo systemctl enable --now cloudflared
```

Then `public_base_url = "https://<your-tunnel-domain>"`.

Verify from outside: `curl -s https://<your-domain>/health`.

### 5. Register the Alexa skill

1. [Alexa developer console](https://developer.amazon.com/alexa/console/ask) →
   **Create Skill** → *Custom* (Alexa Skills Kit) → name it **Aria**
   (invocation name: `aria`).
2. **Skill builder → Interaction Model → Import from file** →
   [`skill/interaction-model.json`](skill/interaction-model.json) → save &
   distribute (every locale you want).
3. **Skill builder → Endpoints**: Endpoint URL
   `https://<your-domain>/alexa`, *Use a custom SSL certificate* off (your
   domain cert is fine), set **SSL certificate** only for native certs.
4. **Interfaces** → tick **Audio Player** (required for music).
   [`skill/skill-manifest.json`](skill/skill-manifest.json) has this
   pre-enabled if you import the whole skill instead of building it in the
   console; you'll need to upload the skill icons it references.
5. Copy the **Client ID** (`amzn1.ask.skill.…`, under *Skill IDs*) into
   `server.client_id` in `/etc/aria/config.toml` and restart aria — this
   makes the endpoint ignore requests from any other skill.
6. **Test** tab → add your Amazon login to *Development* test accounts.

### 6. Run as a service

```bash
sudo cp deploy/aria.service /etc/systemd/system/aria.service
sudoedit /etc/systemd/system/aria.service    # paths, ARIA_ZC_TOKEN, user
sudo systemctl daemon-reload
sudo systemctl enable --now aria
journalctl -u aria -f
```

The unit uses `Environment=ARIA_ZC_TOKEN=*** — the only secret you
should put outside the TOML.

### 7. Say it

On any Echo logged into the test account:

> **"Alexa, ask aria hello."**

Then: *play some music*, *ask aria what is the capital of France*, *pause*,
*next*, *jump to 30 seconds*…

When everything works, publish the skill (in *Development* it's private to
your test accounts).

---

## Configuration reference

See [`config/aria.example.toml`](config/aria.example.toml) — every key is
documented inline and every key has a built-in default.

| Section | Keys | Purpose |
|---|---|---|
| `[server]` | `bind`, `public_base_url`, `client_id`, `tls_cert`, `tls_key`, `data_dir` | Listener, public URL for stream links, skill authentication, native TLS (alternative to Caddy), where `state.json` lives |
| `[library]` | `paths`, `rescan_secs` | Folders scanned for MP3/AAC (FLAC indexed, not streamed in v0.1); rescan interval |
| `[zeroclaw]` | `gateway`, `token`, `agent_alias`, `turn_timeout_ms`, `voice_note`, `mock` | Gateway WS URL + bearer token, agent to talk to, per-turn budget (clamped 1000–7900 ms), system note prepended to utterances, dev echo mode |
| `[radio]` | `stations` | Preset name → stream URL (MP3/AAC, or a plain HTTP audio stream) |
| `[logging]` | `level` | `off..trace` (or `RUST_LOG`) |

State (per-device queues, positions, tokens) persists in `data_dir/state.json`
(default `~/.aria/state.json`).

## Music library

- **MP3 and AAC** (`.m4a`, `.aac`, `.mp4`) stream natively to Echo devices —
  no transcoding.
- `.flac` is indexed (searchable) but not playable in v0.1.
- ID3 tags give title/artist/album/genre; missing tags fall back to folder
  and file names. Cover art is served at `/art/{id}` for Echo Show devices.
- Search order for a query: radio preset → album name → artist → genre →
  fuzzy title/artist/album/genre match.

## Operations

```bash
curl -s https://<your-domain>/health            # library size, active tokens, uptime
journalctl -u aria -p warning -f                # logs
sudo systemctl restart aria                     # re-reads config
rm ~/.aria/state.json                           # forget per-device playback state
```

Adding music: drop files in a `paths` folder; the rescan picks them up
(`rescan_secs`) or on restart.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # 40+ unit tests, incl. stream/seek/tag edge cases
```

GitHub Actions runs all of the above on every push/PR, plus an end-to-end
**smoke job** that starts the real binary against a synthetic library and
drives it with ASK v1 requests (launch → play → download the stream, incl.
Range). Tagging `v*` produces a static musl release binary
([`.github/workflows/`](.github/workflows/)).

Local end-to-end without an Echo: run with `zeroclaw.mock = true` and use
the `curl` example in step 3.

## Roadmap

- [ ] FLAC → MP3 on-the-fly transcode (feature-gated, `lame` FFI)
- [ ] HLS for radio stations that only offer `.m3u8`
- [ ] Multiple agents per room (device → agent mapping)
- [ ] Direct operational intents (status / restart / SOP triggers) via the
      gateway REST API
- [ ] Docker image

## License

MIT — see [LICENSE](LICENSE).
