# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`gpsify` proposes GPS coordinates for untagged photos from their pixels and writes tags **only after a human accepts**. macOS-first, local-first: a Typer CLI plus a loopback-only Starlette review UI. Python 3.12+.

[`docs/design.md`](docs/design.md) is the spec, not background reading. It carries the numbered decisions (K1–K17), the complete SQLite schema, the per-file apply algorithm, and the PR plan. Module docstrings reference PR numbers from that plan. Behavior changes belong in the design doc too; the doc's product questions are resolved and final.

## Commands

**`exiftool` must be on `PATH`** (`brew install exiftool`). Without it, EXIF reads fall back to Pillow/Piexif — JPEG-only — so HEIC EXIF is skipped entirely, a **geotagged HEIC ingests as `untagged`** and would be proposed a new location, HEIC/RAW capture times drop out of clustering, and `apply` fails every HEIC with `HEIC write requires exiftool`. `ingest` warns when it is missing; the fallback exists for JPEG-only folders and the tests, not for real libraries.

**uv is the only supported workflow.** Never `pip install`, `python -m venv`, or a bare `python`/`pytest` — run everything through `uv run`.

```bash
uv sync --extra heic                                      # provisions Python 3.12, installs from uv.lock
uv run gpsify --help

uv run pytest --disable-socket --allow-unix-socket        # CI parity
uv run pytest --disable-socket --allow-unix-socket tests/test_apply.py::test_name
uv run pytest -m live                                     # opt-in, hits real APIs
```

`--allow-unix-socket` is required: asyncio/aiosqlite need it to create an event loop. TCP/UDP stay blocked so nothing reaches xAI or Nominatim. `.github/workflows/ci.yml` is the source of truth and runs `uv sync --locked` then that same pytest line with `GPSIFY_DRY_RUN=1`. CI is pytest-only — no linter or type checker is configured.

Dependencies: add with `uv add` (`uv add --group dev` for tooling), never by hand-editing `pyproject.toml`, so `uv.lock` stays in step — CI runs `--locked` and fails on drift. Dev tooling lives in `[dependency-groups]`; `heic` (`pillow-heif`) is the only runtime extra, and CI deliberately syncs without it so the HEIC-skip paths get exercised.

`pythonpath = ["src"]` in `pyproject.toml` means tests import `gpsify` without an install step; `tests/` is on `sys.path` too, so `from jpeg_factory import ...` works.

Exercise the CLI without touching real data: `GPSIFY_DATA_DIR=/tmp/scratch uv run gpsify ingest <folder>`.

`scripts/bakeoff.py` scores models against an album of already-geotagged photos (their EXIF GPS is the answer key) and prints Acc@km, median error, real token usage, and real USD. It spends money on live API calls, so it is never part of the suite; `tests/test_bakeoff.py` covers its scoring math only.

Exit codes: `0` ok · `1` partial (some photos errored/failed) · `2` config or usage error · `3` `apply` wrote nothing because nothing was accepted.

## Pipeline

```
ingest → propose → review (or decide) → apply → undo
```

`ingest` walks and classifies a folder, hashes every file, reads EXIF in batched `exiftool` calls, and builds time+folder clusters. `propose` sends a lazily built 1280px preview to the selected vision provider (`--provider xai|gemini`), geocodes the returned *place name*, reconciles the two coordinate sources, and writes a `proposal` row. `review`/`decide` write a `decision` row. `apply` writes GPS from that decision. `undo` reverses it from a full-file backup.

Modules map one-to-one onto those stages (`ingest.py`, `propose.py`, `apply.py`, `undo.py`, `status.py`), with `media.py` (decode/transpose/resize), `exif.py` (all `exiftool` read and write), `cluster.py`, `reconcile.py`, plus the `providers/`, `geocode/`, `store/`, and `review/` packages.

Two SQLite databases, both outside the repo under `~/.gpsify` (or `GPSIFY_DATA_DIR`):

- `gpsify.sqlite` — **required** global index mapping resume key → `job_id`. Resume key is `canonical(root)` + `options_hash`, where `options_hash` hashes `{recursive: bool}` **only**; `--hint` is deliberately not part of it.
- `jobs/<job_id>/job.sqlite` — one job's photos, proposals, decisions, and write receipts, alongside `previews/`, `thumbs/`, `backups/`, `copies/`, `logs/`.

## Invariants that must not be broken

These are safety rails, each traceable to a K-decision in the design doc. Breaking one is a product bug, not a style change.

- **`apply` reads the current `decision` row only** — never `proposal.pin_*` (K12). `repo.add_decision` is the *only* decision mutator; it resolves what the human accepted (copying the proposal pin on a bare `accept`), enforces exactly one `current=1` per photo, and rejects null/non-finite coordinates. A persisted accept always has finite lat/lng, so `apply` never guesses.
- **No auto-write at any confidence.** `ui_rank` sorts and colors the UI; it never gates a write. `skip --all-low` is preview-and-confirm.
- **Every image call names its reasoning budget explicitly, and no call opts into retention** (K13). `VisionCallOptions` validates on construction and `assert_safe_vision_options` re-checks before the send; both wrappers refuse otherwise. On xAI that is `store=False`/`store_messages=False` plus `reasoning_effort` — omitting the knob silently bills `high` on `grok-4.6`. On Gemini it is `thinkingConfig.thinkingBudget`, mapped from the same `low`/`medium`/`high`; an omitted budget lets the model pick and bill its own. `xhigh` is rejected. `tests/test_xai_flags.py` and `tests/test_provider_plugin.py` assert this against the real wrappers, not the fake.
- **Gemini has no per-request `store=False`.** Retention follows the key's tier and the API does not report it, so `propose` cannot verify it and warns on every `--provider gemini` run instead. Do not present a Gemini run as non-retaining; the README tells the user to enable billing on the key.
- **A provider reads only its own key** (`XAI_API_KEY` / `GEMINI_API_KEY`) and its own model setting (`GPSIFY_MODEL` / `GPSIFY_GEMINI_MODEL`). Never fall back to the other provider's credentials, and keep `estimate_usd` keyed by model — the running USD line is wrong by ~3× if a Gemini run prices at Grok rates.
- **RAW bytes are never rewritten** (K17). RAW goes out as an extracted embedded preview and comes back as a sibling `.xmp` sidecar; the receipt's `dest_path` *is* the sidecar. Never back up a RAW — back up the previous sidecar or nothing.
- **`photo.sha256` is always the ingest (pre-tag) hash.** Post-write identity lives on `write_receipt.written_sha256`. Dest inspection therefore runs *before* any ingest-hash abort, or `--in-place` retries would wrongly report `source_changed`.
- **Schema v1 is complete and frozen** (`PRAGMA user_version=1`). Do not add columns; additive migrations only after v1. Never rewrite a proposal's `raw_json` — append a row and flip `current`.
- **Ingest never writes `previews/` or `thumbs/`.** They are built just-in-time by `propose` and the review UI so `--limit 10` does not materialize 2,000 JPEGs.
- **The review UI binds `127.0.0.1` only.** There is no LAN flag. Thumbs are served by `photo_id` UUID; originals are never mounted.
- **Geocoding respects the OSMF policy** (K10): primary place name only, submit-on-Enter (no typeahead), ≤1 rps, identifiable User-Agent, cached by normalized key, and a 50-uncached-per-run cap. Never geocode a `privacy_sensitive`/`policy_refusal` result or a house-address-shaped string. `geocoder_url` must stay switchable via config.
- **Refuse `*.photoslibrary` roots** (K14) and never mutate a Photos.app bundle.
- **`--dry-run` means no spend and no mutation.** For `propose`, no xAI or public-geocoder HTTP (cache reads are fine); for `apply`, print the would-be argfile and write nothing.

## Conventions

- Every module starts with `from __future__ import annotations` and a docstring naming its PR slice and normative rules.
- Each stage exposes one `async def run_*(...)` entrypoint returning a summary dataclass, plus a module-local `*Error` exception. `cli.py` is a thin shell: `asyncio.run(run_x(...))`, catch `(XError, StoreError)`, `typer.echo(f"error: {exc}", err=True)`, `raise typer.Exit(code=2)`. Long-running commands take a `progress=lambda message: typer.echo(message)` callback rather than printing themselves.
- Exhaustive `match` over enums ends with `case _: _unreachable(value)` where `_unreachable(value: Never) -> Never`. Keep that arm when adding enum members so type checking catches the gap.
- `gpsify.types` holds the shared Pydantic wire contracts (`VisionGuess`, `GeoHit`, `PromptContext`, `VisionCallOptions`) and the string enums persisted to SQLite. The store persists those values and does not redefine them.
- Settings precedence is environment → `.env` → `~/.gpsify/config.toml` → defaults, wired through `settings_customise_sources`. `[vision]` TOML keys are flattened onto flat field names.
- `GPSIFY_DRY_RUN=1` blocks live HTTP (xAI, geocoder) and `exiftool` writes. `tests/conftest.py` sets it for the whole suite; only `@pytest.mark.writes_exif` lifts it, and `@pytest.mark.live` marks tests that need real APIs.
- Tests build fixtures with `tests/jpeg_factory.py` (never write GPS onto checked-in fixtures), drive the pipeline through `FakeVisionProvider` / `FakeGeocoder`, and exercise apply via `repo.add_decision` + `run_apply` rather than HTTP.
- Never log base64, image bytes, or full home paths. Log `photo_id`, status, place name, `ui_rank`.
- Commits are conventional-commit style, one per PR-plan slice (`feat: xAI grok-4.6 vision proposals (#6)`).
