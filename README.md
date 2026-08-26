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
gpsify run <folder> [--hint TEXT] [--recursive] [--limit N] [--provider xai|gemini]
                    [--port 8765] [--dry-run] [--no-review]
gpsify ingest <folder> [--hint TEXT] [--recursive] [--relocate JOB_ID]
gpsify propose <job> [--limit N] [--dry-run] [--no-context] [--provider xai|gemini] ...
gpsify geocode <job> [--limit N] [--dry-run] [--geocode-uncached-max N]
gpsify review <job> [--port 8765]
gpsify decide <job> --photo ID --action accept|skip|accept_edited [--lat --lng]
gpsify apply <job> [--out DIR] [--in-place] [--dry-run]
gpsify undo <job> [--photo PATH]
gpsify skip <job> --all-low [--yes]
gpsify status [job] [--json]
gpsify clean <job>
```

Typical session — one command from folder to review:

```bash
export XAI_API_KEY=...
uv run gpsify run ~/Pictures/porto-2019 --hint "weekend in Porto, 2019"
# review in the browser, then:
uv run gpsify apply ~/Pictures/porto-2019
```

`run` chains `ingest` → `propose` → `review`. It spends real money on vision calls
with no confirmation prompt; `--dry-run` and `--limit N` are the escape hatches, and
the running `est_usd` prints as it goes. `--no-review` stops after propose. It never
writes GPS — only `apply` does.

The staged commands still work, and `ingest` still prints the `job_id`:

```bash
uv run gpsify ingest ~/Pictures/porto-2019 --hint "weekend in Porto, 2019"
uv run gpsify propose ~/Pictures/porto-2019
uv run gpsify review ~/Pictures/porto-2019
```

### Job handles

`<job>` above is a **job UUID, a folder path, or `last`** — you never have to copy a
UUID. A folder resolves through the global index (`~/.gpsify/gpsify.sqlite`) by the
same canonical path `ingest` registered; `last` is the most recent ingest. Because the
resume key covers `{recursive: bool}`, one folder ingested both flat and `--recursive`
holds two jobs — gpsify then takes the most recent and says which it picked:

```text
$ gpsify status ~/Pictures/porto-2019
2 jobs share this folder; using the most recent: 9f060a67-… (recursive)
```

A UUID whose `job.sqlite` is on disk stays addressable even if its index row is gone,
so a lost index orphans nothing. `ingest --relocate JOB_ID` is the one place that still
needs a literal UUID: it rebinds a job to a new path, so resolving it *by* path would
be circular.

`apply` with no flags writes copies under `~/.gpsify/jobs/<id>/copies/`. `--in-place` requires `--i-mean-it` or a TTY confirm.

## Review UI

`gpsify review <job>` (or the tail of `gpsify run`) serves a loopback-only grid at
`127.0.0.1:8765`. Nothing it does writes a file — closing the tab writes nothing, and
only `gpsify apply` touches a photo.

Photos are grouped the way they were shot: same folder, capture times within 45 minutes
(K9). A group header reads `Fri 12 Jul · 09:10–09:28 · album root · 7 photos`, and a
group with no capture times is flagged `⚠ No capture time` — those are the ones the
apply caps exist to protect.

Two actions look similar and are not:

| | What it does | Capped? |
| --- | --- | --- |
| **Accept** (`a`) | Every selected photo keeps its **own** proposal pin | No — nothing is copied between photos |
| **Apply pin** (`g`) | **One** photo's pin is copied onto the rest of the selection | Yes — K9's caps |

Apply-pin demands a typed confirmation when the batch is over 30 photos, when every
target lacks a capture time, or when more than 25% of them do. The server re-checks
that regardless of what the UI decided, and an id outside the `proposed` candidate set
is rejected rather than silently dropped.

Keyboard: arrows or `j`/`k` move · `Space` full preview · `⇧`-click ranges ·
`⌘`/`Ctrl`-click toggles · `⌘`/`Ctrl A` selects all · `a` accept · `s` skip ·
`e` edit pin · `g` apply pin · `1`–`5` filters · `?` the full list.

Place search geocodes **on Enter only** — never as you type (OSMF policy, K10). Leaflet
is vendored under `static/vendor/`, so the page itself needs no CDN; map tiles still
come from `map_tile_url`, which you can point at a local tile server.

### When propose stops at the geocoder cap

The geocoder is the only automatic pin source, so a `propose` run that hits the
public-Nominatim uncached cap leaves proposals at `pin_source=none` — the vision
work is done, but a bare `accept` has no coordinate to copy. `gpsify geocode`
finishes just that half against the current `geocoder_url`, re-using the guesses
already stored. It never calls a vision provider, so it costs nothing:

```bash
uv run gpsify geocode <job> --dry-run          # how many uncached lookups are left?
uv run gpsify geocode <job_id> --geocode-uncached-max 200
```

`geocoder_url` must point at a **Nominatim-shaped** endpoint. `NominatimGeocoder`
is the only client implemented, so a self-hosted Nominatim works, but Photon,
Mapbox, and Google do not — their responses parse to zero hits, which the
pipeline records (and caches) as a miss rather than an error. A few hundred
lookups at the built-in 1 rps is well inside personal use of the public
instance; a self-hosted Nominatim switches the cap off entirely.

## Tests

**GitHub Actions is the source of truth** for CI. The workflow runs `uv sync --locked` then `uv run pytest --disable-socket --allow-unix-socket` with `GPSIFY_DRY_RUN=1`. Unix sockets are allowed so asyncio/aiosqlite can create an event loop; TCP/UDP stay blocked. No live xAI, Nominatim, or GPS writes.

Locally:

```bash
uv run pytest --disable-socket --allow-unix-socket -q
```

HEIC and live RAW-extract tests skip when `pillow-heif` / `exiftool` are missing.

## License

[MIT](LICENSE)
