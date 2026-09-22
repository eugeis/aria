# Architecture

## Why a standalone bridge instead of a ZeroClaw channel

ZeroClaw's channels are *message* adapters: inbound text in, outbound text
out, handled by the agent loop. An Alexa skill needs three things a channel
model doesn't cover:

1. **An always-on inbound HTTPS endpoint** that Amazon calls (JSON in, JSON
   out), with its own session semantics (skill sessions, device context).
2. **A raw audio streaming server**: `AudioPlayer` directives point Echo
   devices at stream URLs that aria must serve (chunked HTTP, Range
   resume, expected-token validation).
3. **A device-facing state machine**: queue/position/shuffle/repeat per
   device, playback tokens, player reports (`PlaybackStarted/…`).

So aria is a separate Rust service that owns the Alexa protocol and the
audio plane, and delegates *thinking* to ZeroClaw through its documented
gateway WebSocket API (`GET /ws/chat`). ZeroClaw stays unmodified and can
follow upstream; aria is the thin, swappable voice front.

### The WS contract (as implemented in `aria-zeroclaw`)

- Connect: `ws://host/ws/chat?agent_alias=<alias>&session_id=alexa-<device>&token=<bearer>`
  (subprotocol `zeroclaw.v1`; bearer also accepted via header/query).
- Client → server: `{"type":"message","content":"…"}`,
  `{"type":"approval_response","request_id":"…","decision":"approve|deny|always"}`
- Server → client: `{"type":"chunk","content":"<delta>"}`,
  `{"type":"done","full_response":"…","cost_usd":…}`,
  `{"type":"approval_request","request_id":"…","tool":"…","arguments_summary":"…","timeout_secs":n}`

One WS session per Alexa device, keyed by device id, created lazily, kept
alive, reconnected with capped backoff. The stable `session_id` gives the
device persistent agent memory across skill sessions.

## Why Amazon does STT/TTS

The latency-critical path (mic → text, text → speech) is handled at
Amazon's edge:

- In: ASK sends the **transcript** in the `IntentRequest`. No Whisper/local
  STT in the box.
- Out: aria returns `outputSpeech` (SSML); Amazon TTS speaks it. No Piper/
  ElevenLabs in the box.

What crosses the wire to ZeroClaw is a text turn — the same as any other
channel. The agent's own TTS providers remain available for channels that
need them; the voice path here deliberately doesn't use them (lower latency,
zero cost, zero config).

## The 8-second rule

ASK requires the skill endpoint to respond within ~8 s or the Echo times
out. A full LLM turn can exceed that, so `aria-zeroclaw` runs each turn
under `turn_timeout_ms` (default 7500, clamped ≤ 7900):

- **Done in time** → speak `full_response`.
- **Timeout with streamed text** → speak the partial (chunks accumulate
  live; a sentence spoken is better than silence).
- **Timeout with nothing** → canned "taking longer than expected".
- **Approval request arrives mid-turn** → the turn returns early; the prompt
  is spoken; the gateway turn stays alive until the user answers.

The recommended complement is a dedicated `voice` agent in ZeroClaw with a
fast model and a short system prompt — latency is a config decision.

## Approval bridging

ZeroClaw's security model gates risky tool calls behind approvals. On the
gateway WS those surface as `approval_request` frames; the reply is an
`approval_response` on the same socket. aria maps that onto voice turns:

```
turn N:   user: "restart the web server"
          agent → approval_request(shell: systemctl restart …)
          aria → "ZeroClaw needs your approval: the shell tool wants to
                    run 'systemctl restart…'. Say yes to allow it."
turn N+1: user: "yes"
          aria → approval_response(approve) → "Approved. I'll tell you when
                    it's done."
          agent finishes in background → late `done`
turn N+2: user: anything (agent turn)
          aria speaks the cached late reply first, then the new answer.
```

State lives in `AppState.approvals` (device → pending) and
`AppState.cached_reply` (device → late reply). While a turn waits for an
approval, further agent turns on that device are refused by the gateway's
per-session queue — aria answers "there's a pending approval, say yes or
no" instead.

## Music engine

- **Scan**: recursive walk of configured roots; `lofty` reads tags +
  duration + bitrate (pure Rust, no tag daemons). FLAC is indexed but
  `playable=false` (v0.1 streams only what Echo decodes natively).
- **Search**: word-AND over title/artist/album/genre with field weights;
  album/artist/genre get exact-name fast paths. No embeddings: for voice
  queries ("bohemian rhapsody queen") precision beats recall, and this stays
  O(n) with no model in memory.
- **Playback tokens**: 48-char random hex, bound to (device, source, start
  offset), 12 h TTL. The token is the `audioItem.stream.token` **and** the
  URL path — the stream endpoint is unguessable and self-describing.
- **Streaming**:
  - Files: chunked (256 KiB) HTTP, `Content-Type: audio/mpeg|audio/aac`,
    **Range supported** — resume = device reopens the same URL with
    `Range: bytes=N-`; seek/first-play = byte offset derived from
    `offset_ms × avg_bitrate` (MP3 frames make small seek error inaudible).
  - Radio: `reqwest` byte stream piped through (MP3/AAC passthrough).
- **Queue machine** (`PlayerState` per device): ordered queue + position,
  history for "previous", `Repeat::{Off,One,All}`, shuffle (remaining queue),
  persisted to `state.json`. `PlaybackNearlyFinished` preloads the next
  stream, so back-to-back playback has no dead air.

## Security

- `/alexa` validates `client.application.clientId` against
  `server.client_id` (other skills hitting the URL get a silent end).
- Stream URLs are unguessable tokens; art URLs are bounded by track id.
- The ZeroClaw bearer token stays local (env or config, never in logs).
- TLS: Caddy in front (recommended) or native rustls in aria.
- Hardened systemd unit provided.

## Failure modes

| Failure | Behavior |
|---|---|
| ZeroClaw down | WS reconnects with backoff; agent turns report "can't reach ZeroClaw" after 10 s |
| Turn hangs | `STALE_TURN_AFTER` (5 min) frees the slot; per-turn timeout bounds user wait |
| Approval timeout (gateway side) | Gateway auto-denies; aria's pending prompt becomes stale on the next decision |
| Music file deleted mid-play | Stream 404s → Echo fires `PlaybackFailedRequest` → aria advances the queue |
| Library rescan | Atomic swap of the `Arc<RwLock<Library>>`; in-flight streams keep their file handles |
| aria restarts | Queues/position restored from `state.json`; old stream tokens expire (TTL) |
