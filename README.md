# gpsify

Visual geolocation of untagged photos. gpsify proposes a location from pixels, shows its work, and writes GPS **only after a human accepts**.

This repository is a macOS-first, local-first CLI plus a localhost review UI. The design is in [`docs/design.md`](docs/design.md). Product questions there are already resolved; implementation follows the PR plan in that document.

## Status

**PR 9 — status `--json` rollup and clean previews.** `gpsify status [job_id] --json` reads `job.stats_json` plus live photo counts and prints the default dest root (`jobs/<id>/copies/`). `gpsify clean <job_id>` deletes `previews/` and `thumbs/` and keeps sqlite + backups. Cost counters are written by `propose` / `apply`; this slice only exposes them.

GPS is never written without a stored current decision.

## Requirements

- Python 3.12+
- macOS-first (HEIC, Homebrew `exiftool`, Time Machine). Linux is best-effort; Windows is out of v1.

## Install

[uv](https://docs.astral.sh/uv/) is the only supported workflow. It provisions the Python pinned in `.python-version` and installs the exact versions in `uv.lock`.

```bash
uv sync --extra heic
uv run gpsify --help
```

`heic` is optional. Omit `--extra heic` to match CI: JPEG/PNG/RAW still work and HEIC preview is `skipped_unsupported`. RAW preview extract needs `exiftool` (`brew install exiftool`).

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
