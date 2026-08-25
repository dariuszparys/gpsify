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
| `XAI_API_KEY` | unset | Vision provider (required from `propose` onward) |
| `NOMINATIM_CONTACT` | unset | Email-shaped contact for public Nominatim |
| `GPSIFY_MODEL` | `grok-4.6` | Vision model |
| `GPSIFY_REASONING` | `low` | `low` \| `medium` \| `high` (`xhigh` is rejected) |
| `GPSIFY_TIMEOUT_S` | `600` | Client timeout (minimum 120) |
| `GPSIFY_VISION_PROVIDER` | `xai` | v1 is xAI only |
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
provider = "xai"
model = "grok-4.6"
reasoning_effort = "low"
timeout_s = 600
```

Environment variables override the TOML file.

## CLI

```text
gpsify ingest <folder> [--hint TEXT] [--recursive] [--relocate JOB_ID]
gpsify propose <job_id> [--limit N] [--dry-run] [--no-context] ...
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
