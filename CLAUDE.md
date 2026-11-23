# CLAUDE.md

This file provides guidance to coding agents working in this repository.

## What this is

`gpsify` proposes GPS coordinates for untagged photos from their pixels and writes tags **only after a human accepts**. macOS-first, local-first: a single-binary Rust CLI (`clap`, `tokio`, `rusqlite`) plus a loopback-only Axum review UI with embedded static Leaflet assets (`rust-embed`).

[`docs/decisions.md`](docs/decisions.md) is not background reading. It carries the numbered decisions (K1–K18) that the invariants below cite by number, plus the complete SQLite schema. A behavior change that touches a K-decision belongs in that file too.

## Commands

**`exiftool` must be on `PATH`** (`brew install exiftool` on macOS, `apt install libimage-exiftool-perl` on Debian/Ubuntu). `exiftool` is required for all GPS writes (`apply`), and ingest refuses HEIC/RAW/PNG folders without it. A pure-Rust `kamadak-exif` JPEG read-only fallback runs when `exiftool` is absent for JPEG ingest/doctor tests.
`gpsify doctor` checks dependencies and configuration without network calls.

```bash
cargo build --release                      # builds target/release/gpsify and bakeoff
cargo run -- --help                        # run CLI from source

cargo fmt --check                          # formatting gate
cargo clippy --all-targets -- -D warnings  # lint gate (zero warnings)
cargo test                                 # unit, integration, exiftool, wiremock, and headless Chrome tests
```

`.github/workflows/ci.yml` is the source of truth and runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked` (with `GPSIFY_DRY_RUN=1` in the workflow environment; headless Chrome tests run on `macos-latest`), and `cargo build --release --locked`.

Dependencies are managed in `Cargo.toml` and pinned in `Cargo.lock`. On non-macOS hosts, optional HEIC decode is gated behind `--features heic` (`libheif-rs`); on macOS, native `CoreFoundation` + `ImageIO` frameworks handle oriented HEIC/JPEG/PNG decode and resize with zero third-party native image libraries.

Exercise the CLI without touching real data: `GPSIFY_DATA_DIR=/tmp/scratch cargo run -- ingest <folder>`.

`src/bin/bakeoff.rs` scores models against an album of already-geotagged photos (their EXIF GPS is the answer key) and prints Acc@km, median error, real token usage, and real USD. It spends money on live API calls unless `GPSIFY_DRY_RUN=1` is set; its unit tests (`cargo test --bin bakeoff`) cover parsing and scoring math offline.

Exit codes: `0` ok · `1` partial (some photos errored/failed) · `2` config or usage error · `3` `apply` wrote nothing because nothing was accepted.

## Pipeline

```
run  ─────────────────────────────────────┐  (ingest → propose → review, one command)
ingest → propose → review (or decide) → apply → undo
                ↳ geocode (fills pins the uncached cap skipped; no vision spend)
```

**Every `<job_id>` argument is a job handle: a UUID, a folder path, or `last`**
(`store::index::resolve_job_handle`). A folder resolves against the global index by the
canonical path `ingest` registered; one folder ingested both flat and `--recursive`
holds two jobs, and the resolver takes the most recent *and prints which it picked*.
A UUID whose `job.sqlite` is on disk stays addressable with no index row. `ingest --relocate`
is the one exception and still takes a literal UUID.

`ingest` walks and classifies a folder (`walkdir`, parallel `rayon` SHA-256), reads EXIF in batched `exiftool` calls (with `kamadak-exif` JPEG fallback), and builds time+folder clusters (`cluster.rs`). `propose` sends a lazily built 1280px preview to the selected vision provider (`--provider xai|gemini`), geocodes the returned *place name*, reconciles the two coordinate sources, and writes a `proposal` row. `review`/`decide` write a `decision` row. `apply` writes GPS from that decision via batched `exiftool`. `undo` reverses it from a full-file backup (or restores/removes RAW `.xmp` sidecars). `geocode` (`propose::run_geocode`) re-runs only the geocode half for proposals left at `pin_source=none`, reading the stored `VisionGuess` back out of `proposal.raw_json` so no vision call is repeated.

Modules in `src/`:
- `types.rs`: shared `serde` + `schemars` wire contracts (`VisionGuess`, `GeoHit`, `PromptContext`, `VisionCallOptions`) and string enums persisted to SQLite.
- `config.rs`: settings precedence (`env` → `.env` → `~/.gpsify/config.toml` → defaults; `[vision]` TOML flattening; `GPSIFY_DRY_RUN` env-only guard).
- `store/`: `db.rs` (Schema v1 DDL), `index.rs` (global `gpsify.sqlite`), `repo.rs` (`JobRepo`), `lock.rs` (`propose.lock` PID guard).
- `exif.rs`, `media.rs`, `media_macos.rs`, `cluster.rs`, `ingest.rs`: ingestion, lazy 1280px/256px oriented JPEG previews & thumbs, clustering.
- `providers/`: `mod.rs` (`VisionProvider`, `FakeVisionProvider`), `xai.rs`, `gemini.rs`.
- `geocode/`: `mod.rs` (`GeocodeService`, `FakeGeocoder`, K10 rails), `cache.rs`, `nominatim.rs`.
- `reconcile.rs`, `propose.rs`: primary-place reconciliation, `ui_rank`, `run_propose`, `run_geocode`.
- `review/`: `mod.rs`, `security.rs`, `routes.rs`, serving vendored assets from `static/`.
- `apply.rs`, `undo.rs`, `status.rs`, `main.rs`, `bin/bakeoff.rs`.

Two SQLite databases live outside the repo under `~/.gpsify` (or `GPSIFY_DATA_DIR`):
- `gpsify.sqlite` — **required** global index mapping resume key → `job_id`. Resume key is `canonical(root)` + `options_hash`, where `options_hash` hashes `{recursive: bool}` **only**; `--hint` is deliberately not part of it.
- `jobs/<job_id>/job.sqlite` — one job's photos, proposals, decisions, and write receipts, alongside `previews/`, `thumbs/`, `backups/`, `copies/`, `logs/`.

## Invariants that must not be broken

These are safety rails, each traceable to a K-decision in `docs/decisions.md`. Breaking one is a product bug, not a style change.

- **`apply` reads the current `decision` row only** — never `proposal.pin_*` (K12). `JobRepo::add_decision` is the *only* decision mutator; it resolves what the human accepted (copying the proposal pin on a bare `accept`), enforces exactly one `current=1` per photo, and rejects null/non-finite coordinates.
- **A provider-side content block is `refused`, never `error`.** xAI `message.refusal` and Gemini `blockReason`/blocked `finishReason` raise `VisionError::Blocked`; `propose` records `refusal=policy_refusal` with no pin.
- **No auto-write at any confidence.** `ui_rank` sorts and colors the UI; it never gates a write. `skip --all-low` is preview-and-confirm.
- **Every image call names its reasoning budget explicitly, and no call opts into retention** (K13). `VisionCallOptions` validates on construction and `assert_safe_vision_options` re-checks before sending; both wrappers refuse otherwise. On xAI: `store=false`, `store_messages=false`, and explicit `reasoning_effort`. On Gemini: explicit `thinkingConfig.thinkingBudget` mapped from `low`/`medium`/`high`. `xhigh` is rejected. `tests/providers.rs` enforces this against the real wrappers.
- **Gemini has no per-request `store=False`.** Retention follows the key's tier and the API does not report it, so `propose` warns on every Gemini run.
- **A provider reads only its own key** (`XAI_API_KEY` / `GEMINI_API_KEY`) and its own model setting (`GPSIFY_MODEL` / `GPSIFY_GEMINI_MODEL`), and `estimate_usd` prices by model.
- **RAW bytes are never rewritten** (K17). RAW goes out as an extracted embedded preview and comes back as a sibling `.xmp` sidecar; the receipt's `dest_path` *is* the sidecar. Never back up a RAW container — back up the previous sidecar or nothing.
- **`photo.sha256` is always the ingest (pre-tag) hash.** Post-write identity lives on `write_receipt.written_sha256`. Dest inspection runs *before* any ingest-hash abort so `--in-place` retries don't wrongly report `source_changed`.
- **Schema v1 is complete and frozen** (`PRAGMA user_version=1`). Do not add columns; additive migrations only after v1. Never rewrite a proposal's `raw_json` — append a row and flip `current`.
- **Ingest never writes `previews/` or `thumbs/`.** They are built just-in-time by `propose` and the review UI.
- **The review UI binds `127.0.0.1` only** (K16). Thumbs are served by `photo_id` UUID; originals are never mounted. Assets under `static/` are embedded cleanly via `rust-embed` with vendored Leaflet (`static/vendor/`) and zero remote CDN references (`static_assets_are_local_only`).
- **Copying one pin onto many photos is capped; accepting many photos is not** (K9, K18). `POST /api/pin/apply` and `POST /api/clusters/{id}/apply` share server-side candidate & batch-confirmation checks (`>30`, all undated, or `>25%` untimestamped). Both write `DecisionAction::ClusterApply` (`cluster_apply`).
- **No `input` listener in `static/*.js`, and no `.oninput`.** `static_search_is_enter_only` scans every first-party JS module and requires Enter-only search (`event.key !== "Enter"` in `static/app.js`, K10).
- **Geocoding respects the OSMF policy** (K10): primary place name only, submit-on-Enter, ≤1 rps, identifiable User-Agent, normalized SQLite cache, and a 50-uncached-per-run cap on public Nominatim. Never geocode a `privacy_sensitive`/`policy_refusal` result or a house-address-shaped string.
- **Refuse `*.photoslibrary` roots** (K14).
- **`--dry-run` / `GPSIFY_DRY_RUN=1` means no spend and no mutation.** Live HTTP wrappers and `exiftool` write calls honor `GPSIFY_DRY_RUN=1` (explicitly overridden only in local `wiremock` / `exif` write tests).
- **Never log base64, image bytes, or full home paths.** Log `photo_id`, status, place name, `ui_rank`.
- **Browser E2E tests** live in `tests/browser.rs` using `chromiumoxide` headless Chrome driving the embedded Axum review server on loopback.
- **Commits** follow conventional-commit style, one self-contained change per commit.
