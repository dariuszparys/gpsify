# gpsify

Visual geolocation of untagged photos. gpsify proposes a location from pixels, shows its work, and writes GPS **only after a human accepts**.

This repository is a macOS-first, local-first CLI plus a localhost review UI. The design is in [`docs/design.md`](docs/design.md). Product questions there are already resolved; implementation follows the PR plan in that document.

## Status

**PR 9 — status `--json` rollup and clean previews.** `gpsify status [job_id] --json` reads `job.stats_json` plus live photo counts and prints the default dest root (`jobs/<id>/copies/`). `gpsify clean <job_id>` deletes `previews/` and `thumbs/` and keeps sqlite + backups. Cost counters are written by `propose` / `apply`; this slice only exposes them.

GPS is never written without a stored current decision.

## Requirements

- Python 3.12+
- [`exiftool`](https://exiftool.org) on `PATH` — `brew install exiftool`. Treat this as required, not optional; see below.
- macOS-first (HEIC, Homebrew `exiftool`, Time Machine). Linux is best-effort; Windows is out of v1.

### Why `exiftool` is effectively required

gpsify runs without it, but EXIF reads fall back to Pillow / Piexif, which is **JPEG-only**. On a folder of real photos that means:

- **HEIC EXIF is skipped entirely.** An already-geotagged HEIC ingests as `untagged` and would be proposed a new location. This is the one that costs you data.
- HEIC and RAW capture times are missing, so those photos fall out of time clustering.
- RAW files read as no EXIF (a sibling `.xmp` sidecar is still read), and RAW preview extract is unavailable.
- `apply` fails every HEIC with `HEIC write requires exiftool`.

`ingest` warns when it is missing:

```
warning: exiftool not found; using Pillow/Piexif for JPEG EXIF only. HEIC GPS detection skipped.
```

JPEG-only folders are fully supported on the fallback path.

## Install

```bash
brew install exiftool
```

[uv](https://docs.astral.sh/uv/) is the only supported workflow for the Python side. It provisions the Python pinned in `.python-version` and installs the exact versions in `uv.lock`.

```bash
uv sync --extra heic
uv run gpsify --help
```

`heic` (`pillow-heif`) is the one optional runtime extra and covers HEIC **decode** for previews and vision. Omit `--extra heic` to match CI: JPEG/PNG/RAW still work and HEIC preview is `skipped_unsupported`.

## Configuration

Secrets stay out of git. Copy [`.env.example`](.env.example) to `.env` or export in the shell:

| Name | Default | Purpose |
| --- | --- | --- |
| `XAI_API_KEY` | unset | Key for `--provider xai` (required from `propose` onward) |
| `GEMINI_API_KEY` | unset | Key for `--provider gemini` |
| `NOMINATIM_CONTACT` | unset | Email-shaped contact for public Nominatim |
| `GPSIFY_MODEL` | `grok-4.6` | Vision model for `--provider xai` |
| `GPSIFY_GEMINI_MODEL` | `gemini-2.5-flash` | Vision model for `--provider gemini` |
| `GPSIFY_REASONING` | `low` | `low` \| `medium` \| `high` (`xhigh` is rejected) |
| `GPSIFY_TIMEOUT_S` | `600` | Client timeout (minimum 120) |
| `GPSIFY_VISION_PROVIDER` | `xai` | `xai` \| `gemini`; default for `--provider` |
| `GPSIFY_DATA_DIR` | `~/.gpsify` | Config, job index, and job trees |
| `GEOCODER_URL` | public Nominatim search | Switch geocoder without a code change |

Optional `~/.gpsify/config.toml` (mode `0600` on a real machine):

```toml
nominatim_contact = "you@example.com"
geocoder_url = "https://nominatim.openstreetmap.org/search"
geocoder = "nominatim"
map_tile_url = "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
geocode_lang = "en"

[vision]
provider = "xai"           # or "gemini"
model = "grok-4.6"         # used when provider = "xai"
gemini_model = "gemini-2.5-flash"
reasoning_effort = "low"
timeout_s = 600
```

Environment variables override the TOML file.

## Vision providers

Both providers send the same `PROMPT_V1` and are held to the same `VisionGuess`
schema, so runs are comparable. Each reads only its own key — selecting one
provider never falls back to the other's credentials.

| | `xai` (default) | `gemini` |
| --- | --- | --- |
| Key | `XAI_API_KEY` | `GEMINI_API_KEY` |
| Default model | `grok-4.6` | `gemini-2.5-flash` |
| Reasoning knob | `reasoning_effort` | `thinkingConfig.thinkingBudget`, mapped from the same `low`/`medium`/`high` |
| ~Cost per photo | ~$0.006–$0.015 | ~$0.0018 |
| No-retention | `store=False` on every call | **key-tier property — see below** |

```bash
export GEMINI_API_KEY=...
uv run gpsify propose <job_id> --provider gemini
```

`gemini-2.5-flash` is the default because
[`docs/research/vision-model-selection.md`](docs/research/vision-model-selection.md)
ranks it second on every public photo-geolocation benchmark it surveys
(GeoArena, EarthWhere, GeoRC) at roughly 3–8× lower list cost than `grok-4.6`.
Set `GPSIFY_GEMINI_MODEL=gemini-2.5-pro` for the accuracy ceiling — first on
those same boards, at roughly Grok-like spend.

### Which model — measure, don't read

The research note ranks `gemini-2.5-flash` on *other people's* photos. To settle
it on yours, `scripts/bakeoff.py` scores any set of models against photos whose
GPS you already have — the album's own EXIF is the answer key:

```bash
uv run python scripts/bakeoff.py ~/Pictures/porto-2019 \
    --model xai:grok-4.6 \
    --model gemini:gemini-2.5-flash \
    --model gemini:gemini-2.5-pro \
    --limit 20 --out bakeoff.json
```

Every model sees the byte-identical 1280 px preview, its place name goes through
the normal geocode path, and the distance from that pin to the photo's real
coordinates is the score. Output is Acc@1/25/200/750 km, median error, refusal
rate, observed token profile, and real dollars per photo. It writes no GPS and
mutates nothing; `GPSIFY_DRY_RUN=1` prints the plan and spends nothing.

One vision call per photo per model, so start with `--limit`.

### Gemini retention: use a billed key

Gemini has **no per-request `store=False`**. Retention is a property of the key's
tier, which the API does not report, so gpsify cannot check it for you and
prints a warning on every `--provider gemini` run. On the free tier Google may
use prompts and images to improve its products; billing-enabled keys are
excluded. Enable billing on the key before pointing it at real photos.

## CLI

```text
gpsify ingest <folder> [--hint TEXT] [--recursive] [--relocate JOB_ID]
gpsify propose <job_id> [--limit N] [--dry-run] [--no-context] [--provider xai|gemini] ...
gpsify review <job_id> [--port 8765]
gpsify decide <job_id> --photo ID --action accept|skip|accept_edited [--lat --lng]
gpsify apply <job_id> [--out DIR] [--in-place] [--dry-run]
gpsify undo <job_id> [--photo PATH]
gpsify skip <job_id> --all-low [--yes]
gpsify status [job_id] [--json]
gpsify clean <job_id>
```

Typical session:

```bash
export XAI_API_KEY=...
uv run gpsify ingest ~/Pictures/porto-2019 --hint "weekend in Porto, 2019"
uv run gpsify propose <job_id>
uv run gpsify review <job_id>
uv run gpsify apply <job_id>
```

`apply` with no flags writes copies under `~/.gpsify/jobs/<id>/copies/`. `--in-place` requires `--i-mean-it` or a TTY confirm.

## Tests

**GitHub Actions is the source of truth** for CI. The workflow runs `uv sync --locked` then `uv run pytest --disable-socket --allow-unix-socket` with `GPSIFY_DRY_RUN=1`. Unix sockets are allowed so asyncio/aiosqlite can create an event loop; TCP/UDP stay blocked. No live xAI, Nominatim, or GPS writes.

Locally:

```bash
uv run pytest --disable-socket --allow-unix-socket -q
```

HEIC and live RAW-extract tests skip when `pillow-heif` / `exiftool` are missing.

## License

[MIT](LICENSE)
