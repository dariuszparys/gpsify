# gpsify

Visual geolocation of untagged photos. gpsify proposes a location from pixels, shows its work, and writes GPS **only after a human accepts**.

This repository is a macOS-first, local-first CLI plus a localhost review UI. The design is in [`docs/design.md`](docs/design.md). Product questions there are already resolved; implementation follows the PR plan in that document.

## Status

**PR 1 — project skeleton.** The `gpsify` console script installs and prints `--help`. Settings load from the environment and `~/.gpsify/config.toml`. Shared Pydantic types live in `gpsify.types`. Ingest, propose, review, apply, and the rest of the commands are stubs (exit 2) until later slices.

GPS is never written in this slice. There is no network I/O from the app yet.

## Requirements

- Python 3.12+
- macOS-first (HEIC, Homebrew `exiftool`, Time Machine). Linux is best-effort; Windows is out of v1.

## Install

```bash
python -m venv .venv
source .venv/bin/activate
python -m pip install -e ".[dev]"
gpsify --help
```

With [uv](https://docs.astral.sh/uv/):

```bash
uv sync --group dev
uv run gpsify --help
```

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

Typical session (once later PRs land):

```bash
export XAI_API_KEY=...
gpsify ingest ~/Pictures/porto-2019 --hint "weekend in Porto, 2019"
gpsify propose <job_id>
gpsify review <job_id>
gpsify apply <job_id>
```

`apply` with no flags writes copies under `~/.gpsify/jobs/<id>/copies/`. `--in-place` is explicit.

## Tests

**GitHub Actions is the source of truth** for CI. The workflow runs `pytest --disable-socket` with `GPSIFY_DRY_RUN=1`. No live xAI, Nominatim, or GPS writes.

Locally:

```bash
python -m pip install -e ".[dev]"
python -m pytest --disable-socket -q
```

## License

[MIT](LICENSE)
