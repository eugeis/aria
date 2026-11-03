# li7 deployment secrets (aria)

The deploy workflow (`.github/workflows/deploy.yml`) reads these values from
files on the li7 runner (the `eugeis/mosaic` self-hosted runner, user `ee`).
**Never commit the values** — only these paths are referenced. Create each
file mode 600, one value per line (no trailing whitespace/newline needed —
the workflow strips it).

| File on li7 | Env var (k8s secret `name` = lowercased var, key = var) | Purpose |
|---|---|---|
| `/home/ee/.ssh/aria-jwt-secret` | `MOAIC_JWT_SECRET` → `moaic-jwt-secret` | JWT signing for the brain's identity |
| `/home/ee/.ssh/aria-admin-password` | `MOAIC_ADMIN_PASSWORD` → `moaic-admin-password` | the `admin` user (ops, web chat lens) |
| `/home/ee/.ssh/aria-adapter-token` | `MOAIC_ADAPTER_TOKEN` → `moaic-adapter-token` | the `adapter` service account (the ASK sidecar's credential — it rides on the same secret as `ARIA_BRAIN_TOKEN` in the sidecar) |
| `/home/ee/.ssh/aria-chat-key` | `MOAIC_CHAT_API_KEY` → `moaic-chat-api-key` | the vLLM bearer for the voice brain (same value family as the bible app's chat key) |
| `/home/ee/.ssh/bible-reader-token` | `MOAIC_MCP_BIBLE_AUTH` → `moaic-mcp-bible-auth` | the bible app's `reader` service-account token — the bearer the brain's MCP client presents to `https://bible.eisler-systems.de/mcp` (share it from bible's `MOAIC_READER_TOKEN` secret) |

Missing files do not fail the deploy — the matching surface fails soft
(workflow LLM answers, the bible MCP tools, or auth) until the secret exists.

## Host directories (create before the first deploy)

```
sudo mkdir -p /var/lib/aria-data /var/lib/aria-lancedb /var/lib/aria-media
```

- `/var/lib/aria-data` — ext4 (NOT exFAT). The brain corpus
  (`knowledge/media/media.jsonl.gz`, `knowledge/sermons/`), the ADR 0100
  persist seam, the sqlite db, the IAM state, and the adapter's `state.json`
  (+ its scan output under `knowledge/media/`).
- `/var/lib/aria-lancedb` — ext4 (NOT exFAT — LanceDB's commit protocol
  `linkat`s manifests; exFAT fails with EPERM and the vector table silently
  goes stale).
- `/var/lib/aria-media` — the audio collections. Layout the adapter scans:

  ```
  /var/lib/aria-media/
  ├── rock/                  (kind: music — root/Artist/Album/nn - title.mp3)
  │   └── Queen/
  │       └── A Night at the Opera/
  │           ├── 01 - Bohemian Rhapsody.mp3
  │           └── ...
  ├── bible-audio/           (kind: audiobook — one subfolder per book)
  │   └── Psalmen/
  │       ├── 001 - Psalm 1.mp3
  │       └── 002 - Psalm 2.mp3
  └── sermons-audio/         (kind: sermon — "YYYY-MM-DD - Title.mp3")
      └── 2026-05-10 - Grace Upon Grace.mp3
  ```

  MP3 and AAC (m4a/m4b) stream to the Echo; FLAC is indexed but not
  playable. Add more collections by editing the baked config
  (`adapter/config/aria.example.toml` → rebuild the `mosaic/aria-ask` image)
  or by mounting a config volume.

## Stage the corpus before the first boot

The brain ingests `knowledge/media` + `knowledge/sermons` under
`MOAIC_KNOWLEDGE_BASE=/data`. On a fresh host the directories are empty
(fail-soft boot — the app is ready, the KBs just have the committed sample
corpus baked into the image). Two ways to get real content in:

1. **Let the adapter do it (recommended):** the adapter's initial scan
   publishes `/data/knowledge/media/media.jsonl.gz` and POSTs
   `/api/ops/reindex`. Just drop audio into `/var/lib/aria-media` and
   restart the pod (`kubectl -n aria rollout restart deployment/aria`).
2. **Sermon scripts:** sync them to `/var/lib/aria-data/knowledge/sermons/`
   (markdown, the eugeis/sermons `content/` flattened) + restart the pod —
   or `kubectl -n aria exec deploy/aria -c aria -- wget -qO- -X POST -H
   'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_JWT"
   --post-data='{}' http://127.0.0.1:3000/api/ops/reindex`.
