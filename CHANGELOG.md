# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has
a release.

## [Unreleased]

Nothing is tagged yet. Everything below is the state of `main`.

### Added

- **Pipeline.** `ingest → propose → review → apply → undo`, plus `run` to chain
  the first three in one command, and a `geocode` pass that finishes proposals
  the uncached geocoder cap left without a pin — without repeating a vision
  call.
- **Folder ingest** that walks, classifies, hashes, reads EXIF in batched
  `exiftool` calls, and builds time-and-folder clusters. Refuses
  `*.photoslibrary` bundles.
- **Two vision providers** behind one protocol: xAI `grok-4.6` (default) and
  Google `gemini-2.5-flash`, held to the same prompt and the same response
  schema so runs are comparable. Each reads only its own key.
- **Switchable geocoder** with a unique-string cache, 1 rps pacing, an
  identifiable User-Agent, and a per-run cap on uncached lookups.
- **Coordinate reconciliation** between the model's own guess and the geocoded
  place name — disagreement is shown, never silently averaged.
- **Lightroom-style review UI** on `127.0.0.1`: a keyboard-driven grid, group
  headers, a map inspector, lightbox preview, multi-select, and copying one
  accepted pin onto a selection behind explicit caps.
- **Non-destructive apply.** Writes to copies by default; `--in-place` requires
  an explicit confirmation. JPEG/HEIC get EXIF+XMP, PNG gets XMP, and RAW gets
  a sibling `.xmp` sidecar — RAW pixel data is never rewritten.
- **Undo** from a full-file backup, or by removing the sidecar for RAW.
- **Job handles.** Every command that takes a job accepts a UUID, a folder path,
  or `last`.
- **`status --json`** rollup with cumulative spend, latency percentiles, geocode
  cache hit rate, and a refusal histogram; `clean` to drop generated previews
  and thumbs while keeping SQLite and backups.
- **`GPSIFY_DRY_RUN`** as a global kill switch for paid calls and writes, plus
  per-command `--dry-run` and `--limit`.
- **`scripts/bakeoff.py`** to score models against an album whose EXIF GPS is
  the answer key, reporting real accuracy, tokens, and dollars.

### Fixed

- A provider-side content block is recorded as a refusal, not a retryable
  error — retrying one only re-bills a call that can fail the same way.
- `propose` spend counters accumulate over the life of a job instead of being
  overwritten by a later run.
- `ingest` reports every photo it saw, and warns when a non-recursive run finds
  only subdirectories.
- The test suite is isolated from a developer's local `.env`.
- CI pins `astral-sh/setup-uv` to an existing version.
