# gpsify — decisions and schema

The numbered decisions below (**K1–K18**) are the project's safety rails. Code,
tests, and `CLAUDE.md` cite them by number, so this file is the place to look
one up. They were settled while designing v1 and are cited, not relitigated —
if a change needs to break one, the decision is what has to change first.

Schema v1 is **complete and frozen** (`PRAGMA user_version=1`). Additive
migrations only.

## Key Decisions

| # | Decision | Rationale |
| --- | --- | --- |
| K1 | **Python 3.12+ CLI + localhost review UI**, not Electron/Tauri and not CLI-only | Review needs a map pin. Python owns EXIF, HEIC decode, Pillow, Pydantic, and SQLite. A 127.0.0.1 Starlette app plus Leaflet is enough for a personal tool; Electron/Tauri adds packaging cost with no product gain in v1. Pure CLI cannot show a pin. |
| K2 | **xAI (`grok-4.6`) is the default vision provider; Gemini is the second** | Decision (2026-08-12): Grok first, one provider at a time. Gemini followed (`--provider gemini`, `GEMINI_API_KEY`, default `gemini-2.5-flash`) behind the same `VisionProvider` protocol and the same `PROMPT_V1`/`VisionGuess` contract. Grok stays the default; neither provider reads the other's key. |
| K3 | **Never trust model lat/lng alone**; geocode the place name | Models hallucinate coordinates. The configured geocoder converts a named place to a real point. Disagreement is shown, not silently averaged. |
| K4 | **Human confirmation is mandatory**; no auto-write, even at high UI rank | A wrong pin is worse than no pin. |
| K5 | **Non-destructive by default**: `gpsify apply <job_id>` writes copies to `~/.gpsify/jobs/<id>/copies/<relpath>`. `--out DIR` overrides that dest. `--in-place` is explicit and mutually exclusive with `--out`. | Originals stay untouched unless the user opts in. Omitting both flags is not a usage error — dest is the job `copies/` tree, and `status` prints it. |
| K6 | **JPEG first-class (EXIF+XMP); PNG XMP-in-file; HEIC decode + EXIF+XMP; RAW in v1 via preview + XMP sidecar** | Decision (2026-08-12). xAI accepts only jpeg/png, so RAW is never uploaded. Propose from an embedded preview JPEG; no usable preview → `skipped_no_preview`. Apply never mutates RAW bytes. |
| K7 | **`exiftool` is the write engine**, invoked in **batches**; Piexif is JPEG-only test fallback | `exiftool` is the interoperability gold standard. Per-file process spawn will miss the ingest/apply latency targets. Do not reimplement GPS IFD packing. |
| K8 | **SQLite job store under `~/.gpsify/jobs/<job_id>/` plus a required global index** | One job = one folder ingest. The index (`~/.gpsify/gpsify.sqlite`) maps resume keys → `job_id`. Undo, resume, and review do not depend on sidecars next to originals. |
| K9 | **Time + folder clustering in v1**; no visual embeddings | Cheap and explainable. Apply-to-cluster is gated (explicit ids, `proposed` only, size/timestamp caps) so one wrong church cannot poison 400 undated exports. |
| K10 | **Public Nominatim is interactive + cached unique strings only**, not an unbounded bulk API | OSMF policy forbids autocomplete, requires identifiable UA + attribution, discourages bulk, and requires apps to switch geocoder without a software update. `geocoder_url` in config is the switch. |
| K11 | **`job_id` is a UUID printed once**; resume is by index, not by guessing | Re-ingest of the same canonical root + options upserts; a new path is a new job unless `--relocate`. |
| K12 | **`apply` reads only the current `decision` row** (never `proposal.pin_*`). `add_decision` may copy the proposal pin **at accept time** and must persist finite lat/lng for every writeable action. | Proposal pins are suggestions. The human decision is the write contract. Exactly one `current=1` decision per photo. Null-coord accepts are rejected, not written. |
| K13 | **Every xAI image call sets `store_messages=False` / `store=False` and an explicit `reasoning_effort`** | Responses API stores conversations 30 days by default; image-understanding docs warn requests may fail unless storage is off. `grok-4.6` reasoning cannot be disabled and defaults to `"high"` — omitting the knob silently explodes cost/latency. `xhigh` exists and is out of v1. |
| K14 | **macOS-first**; refuse `*.photoslibrary` roots | First albums are iPhone HEIC and/or DSLR RAW. Mutating a Photos.app bundle is a non-goal and will corrupt the library. |
| K15 | **Undo JPEG/HEIC/PNG from a full-file backup taken before `exiftool`. Undo RAW by deleting or restoring the XMP sidecar only.** | Never copy a multi-ten-MB RAW as an undo blob. Never hardlink. Never `-overwrite_original` on a RAW. |
| K16 | **Review UI binds `127.0.0.1` only**; no `--i-understand-lan` in v1 | A LAN bind with no auth would publish thumbs and place guesses on the local network. Loopback is the trust boundary. |
| K17 | **RAW write is sidecar-only** (`foo.CR2` → `foo.xmp`). Copies mode copies the RAW and writes the sidecar next to the copy. `--in-place` writes only the sidecar next to the original. | Decision (2026-08-12). Lightroom reads XMP sidecars; rewriting RAW containers is unsafe and unnecessary. |
| K18 | **A grid selection is a cluster the human drew, and inherits K9's caps.** `POST /api/pin/apply` copies one accepted pin onto an arbitrary multi-select, which may span clusters. It shares `_apply_source_pin` with the cluster route: `proposed`-only candidates, an explicit id list where an extra id is a 400 rather than a silent drop, and a typed confirm at >30, all-undated, or >25% untimestamped. | A Lightroom-style grid makes bulk pinning easy, which is exactly why the rails must not be optional. Selecting by hand is not evidence the pin is right — 40 undated photos are as dangerous picked as clustered. **Accept** stays uncapped because each photo keeps its *own* pin; nothing is copied. |

## Schema v1 (SQLite)

`jobs/<id>/job.sqlite`, WAL, `PRAGMA foreign_keys=ON`, `PRAGMA user_version=1`. The whole schema is created up front; nothing added columns to it later.

```sql
CREATE TABLE job (
  id TEXT PRIMARY KEY,
  root_path TEXT NOT NULL,
  root_canonical TEXT NOT NULL,
  options_hash TEXT NOT NULL,
  hint TEXT,
  created_at TEXT NOT NULL,
  config_json TEXT NOT NULL,
  stats_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE cluster (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES job(id),
  parent TEXT NOT NULL,
  first_ts TEXT,
  n INTEGER NOT NULL,
  undated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE photo (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES job(id),
  path TEXT NOT NULL,
  relpath TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  status TEXT NOT NULL,
  error TEXT,
  datetime_original TEXT,
  make TEXT,
  model TEXT,
  has_gps INTEGER NOT NULL DEFAULT 0,
  existing_lat REAL,
  existing_lng REAL,
  cluster_id TEXT REFERENCES cluster(id),
  updated_at TEXT NOT NULL,
  UNIQUE(job_id, path)
);

CREATE TABLE proposal (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  created_at TEXT NOT NULL,
  current INTEGER NOT NULL DEFAULT 0,   -- exactly one current=1 per photo when any exist
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  raw_json TEXT NOT NULL,
  place_name TEXT,
  granularity TEXT,
  model_confidence REAL,
  ui_rank REAL,
  refusal TEXT,
  pin_lat REAL,
  pin_lng REAL,
  pin_source TEXT,                       -- geocoder | user | none
  model_agree INTEGER,
  geocode_json TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  latency_ms INTEGER
);

CREATE UNIQUE INDEX proposal_one_current
  ON proposal(photo_id) WHERE current = 1;

CREATE TABLE decision (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  proposal_id TEXT REFERENCES proposal(id),
  action TEXT NOT NULL,                  -- accept | accept_edited | skip | cluster_apply
  lat REAL,
  lng REAL,
  place_name TEXT,
  current INTEGER NOT NULL DEFAULT 0,
  decided_at TEXT NOT NULL
);

CREATE UNIQUE INDEX decision_one_current
  ON decision(photo_id) WHERE current = 1;

CREATE TABLE write_receipt (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  decision_id TEXT NOT NULL REFERENCES decision(id),
  dest_path TEXT NOT NULL,
  in_place INTEGER NOT NULL,
  status TEXT NOT NULL,                  -- pending | ok | failed | undone
  error TEXT,
  written_at TEXT,
  written_sha256 TEXT,
  backup_path TEXT,
  UNIQUE(photo_id, dest_path)
);
```

Global index: `~/.gpsify/gpsify.sqlite` as specified by K11 above. It is **required**, not optional.

`job.stats_json` is merged at the end of `propose` / `apply` (counts, p50/p95 latency, estimated USD, geocode cache hit rate, refusal histogram).

**Spend is cumulative for the life of the job.** `propose` adds this run's counts, tokens, USD, geocode counters, and refusal histogram to the previous totals and bumps `runs`; it never replaces them. A later run — including one that plans nothing because every photo is already `proposed` — must not zero the tokens and dollars an earlier run really spent. The latest invocation is kept on its own under `propose.last_run` (same shape, this run only), and p50/p95 keep the last run that actually made calls. `--dry-run` returns before any stats write.

### `add_decision` contract

`repo.add_decision(photo_id, action, lat=None, lng=None, proposal_id=None, place_name=None)` is the **only** mutator for decisions. It flips `current` flags and updates `photo.status`. Apply tests call this helper directly; they do not need Starlette.

Coordinate rules (this is **not** `apply` reading proposal pins — it is the decision helper resolving what the human accepted):

| `action` | lat/lng |
| --- | --- |
| `skip` | must be null |
| `accept` | if omitted, copy `pin_lat`/`pin_lng`/`place_name` from the **current proposal** iff `pin_source` is `geocoder` or `user` and both coords are finite; **reject** (raise / HTTP 400) if there is no current proposal, `pin_source=none`, or coords are missing/non-finite |
| `accept_edited` | caller **must** supply finite lat/lng; reject otherwise (no silent copy) |
| `cluster_apply` | caller **must** supply finite lat/lng copied from the *source decision* (already validated); reject if source decision has null coords |

A persisted `accept` / `accept_edited` / `cluster_apply` row always has finite `lat` and `lng`. `apply` never has to guess.

JSON exports (`gpsify status --json`) are derived. SQLite is the source of truth.

**Migration strategy:** `user_version` pragma. v1 ships schema version 1 **complete**. Additive migrations only after v1. Never rewrite proposal `raw_json` in place (append a new `proposal` row and flip `current`).

