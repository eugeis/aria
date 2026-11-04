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
| `/home/ee/.ssh/openrouter` | `MOAIC_CHAT_API_KEY` → `moaic-chat-api-key` | the OpenRouter key for the voice brain — house policy: ALL mosaic apps run their operations on OpenRouter (the in-house vLLM box is dev/data-prep only). This is the SAME file the pages app uses; the deploy workflow points at it directly. |
| `/home/ee/.ssh/bible-reader-token` | `MOAIC_MCP_BIBLE_AUTH` → `moaic-mcp-bible-auth` | the bible app's `reader` service-account token — the bearer the brain's MCP client presents to `https://bible.eisler-systems.de/mcp` (share it from bible's `MOAIC_READER_TOKEN` secret) |

Missing files do not fail the deploy — the matching surface fails soft
(workflow LLM answers, the bible MCP tools, or auth) until the secret exists.

## Host directories (create before the first deploy)

```
sudo mkdir -p /var/lib/aria-data /var/lib/aria-lancedb
```

- `/var/lib/aria-data` — ext4 (NOT exFAT). The brain corpus
  (`knowledge/media/media.jsonl.gz`, `knowledge/sermons/`), the ADR 0100
  persist seam, the sqlite db, the IAM state, and the adapter's `state.json`
  (+ its scan output under `knowledge/media/`).
- `/var/lib/aria-lancedb` — ext4 (NOT exFAT — LanceDB's commit protocol
  `linkat`s manifests; exFAT fails with EPERM and the vector table silently
  goes stale).
- `/home/ee/audio` — the audio collections (mounted at `/media` in the pod
  via the injector). The adapter scans the two collections wired in the
  baked config (`adapter/config/aria.example.toml`):

  ```
  /home/ee/audio/
  ├── musik/      (kind: music — folders map to artist/album when tags
  │                are missing; ~330 files)
  ├── predigt/    (kind: sermon — every audio file is one recording;
  │                ~380 files, subfolders are series)
  ├── bible/      (per-translation audio — not yet wired: the
  │                TNI/NN_Book layout needs an audiobook mapping)
  ├── books/ deusch-lernen/ seminar/   (available for new collections)
  └── ...
  ```

  MP3 and AAC (m4a/m4b) stream to the Echo; FLAC is indexed but not
  playable. Add a collection by editing the baked config's
  `[collections]` items (→ rebuild the `mosaic/aria-ask` image).

## No vLLM access from li7 (2026-11)

The original voice brain was the in-house vLLM box (datalab GPU) behind a
loopback `ssh -R` tunnel on li7. **li7 no longer has access to it (and will
not again)**, so:

* the app runs its LLM entirely on **OpenRouter** (the `chat` registry
  entry; the key is `/home/ee/.ssh/openrouter` below);
* the `vllm-pod-bridge.service` socat and the chart `hostAliases` entry
  that made the tunnel pod-visible are **removed** (the unit was disabled
  and deleted on li7 on 2026-11-04);
* if a self-hosted model box appears later (your own hardware or a new
  server), it re-enters as a normal `model_registry` entry with its own
  base URL — no bridge machinery needed as long as li7 can reach it
  directly.

## Memory budget (do not raise requests casually)
## Memory budget (do not raise requests casually)

li7 has **61.5 Gi allocatable** and co-tenants (bible ~51 Gi, pages ~8 Gi,
mosaic-app 1 replica, …) already reserve ~60 Gi. The aria pod requests
**0.5 Gi (brain) + 0.125 Gi (adapter)** — sized to schedule even on a full
cold node restart. The rollout strategy is no-surge (`maxSurge: 0`,
`maxUnavailable: 1`, injected): an old aria pod and its replacement may
never both be scheduled. A post-helm `kubectl patch` of the pod template
spawns a second ReplicaSet and can deadlock the rollout — the injector
bakes everything into the template so one `helm upgrade` is final.

## Stage the corpus before the first boot

The rendered image carries ONLY the `aria` binary (the house pattern, same
as bible) — the knowledge base lives on the host and is ingested from
`MOAIC_KNOWLEDGE_BASE=/data` at boot. Seed the committed sample corpus once
(15 media records + the sample sermon script):

```
sudo mkdir -p /var/lib/aria-data/knowledge/{media,sermons}
sudo cp <rendered-out>/knowledge/media/media.jsonl.gz /var/lib/aria-data/knowledge/media/
sudo cp <rendered-out>/knowledge/sermons/*.md        /var/lib/aria-data/knowledge/sermons/
sudo chown -R $(id -u):$(id -g) /var/lib/aria-data/knowledge   # app runs as root; keep it host-user-owned for easy cp
kubectl -n aria rollout restart deployment/aria
```

Real content gets in two ways:

1. **The adapter (media metadata):** drop audio into `/home/ee/audio`;
   the periodic rescan publishes `/data/knowledge/media/media.jsonl.gz` and
   POSTs `/api/ops/reindex`. An EMPTY scan never publishes — a missing or
   empty media dir cannot wipe the catalog from the brain.
2. **Sermon scripts:** sync them to `/var/lib/aria-data/knowledge/sermons/`
   (markdown, the eugeis/sermons `content/` flattened) + restart the pod —
   or POST `/api/ops/reindex` with the `adapter` service token.
