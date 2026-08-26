# Contributing to gpsify

Thanks for looking. This is a personal, macOS-first tool, so the bar for a
change is "does it fit the shape of the thing" rather than "is it useful in
general". Please open an issue before a large PR — it is cheaper than finding
out after the fact that a change collides with a decision in
[`docs/decisions.md`](docs/decisions.md).

## Setup

You need [`exiftool`](https://exiftool.org) on `PATH` and
[uv](https://docs.astral.sh/uv/). Nothing else.

```bash
brew install exiftool
uv sync --extra heic
uv run gpsify --help
```

**uv is the only supported workflow.** Do not `pip install`, do not
`python -m venv`, and do not run a bare `python` or `pytest` — everything goes
through `uv run`. The lockfile is authoritative and CI runs `uv sync --locked`,
so a hand-edited `pyproject.toml` will fail the build.

Add dependencies with the tool, never by hand:

```bash
uv add some-package                # runtime
uv add --group dev some-package    # tooling
```

## Tests

Run exactly what CI runs:

```bash
uv run pytest --disable-socket --allow-unix-socket
```

`--allow-unix-socket` is required — asyncio and aiosqlite need it to create an
event loop. TCP and UDP stay blocked so a test can never reach xAI or Nominatim.

The suite sets `GPSIFY_DRY_RUN=1` for every test. Two markers lift or extend
that: `@pytest.mark.writes_exif` allows real `exiftool` writes under `tmp_path`,
and `@pytest.mark.live` marks tests that need real APIs and are never run in CI.

Build fixtures with `tests/jpeg_factory.py`. **Never write GPS onto a
checked-in fixture.** Drive the pipeline through `FakeVisionProvider` and
`FakeGeocoder`, and exercise apply through `repo.add_decision` + `run_apply`
rather than over HTTP.

CI is pytest-only. There is no linter or type checker configured, so match the
style of the file you are editing rather than reformatting it.

## Before you change behavior

Read the invariants in [`CLAUDE.md`](CLAUDE.md). They are not style preferences
— each one traces to a numbered decision in
[`docs/decisions.md`](docs/decisions.md), and breaking one is a product bug. The
ones that bite most often:

- **`apply` reads the current `decision` row only**, never a proposal pin (K12).
- **No automatic write at any confidence** (K4). Ranking sorts the UI; it never
  gates a write.
- **RAW bytes are never rewritten** (K17) — RAW gets an `.xmp` sidecar.
- **Every vision call names its reasoning budget and opts out of retention**
  where the provider allows it (K13).
- **The review UI binds `127.0.0.1` only** (K16), and its JS has no `input`
  listener — search is submit-on-Enter by OSMF policy (K10).
- **Schema v1 is frozen.** Do not add columns.

If a change genuinely needs to move one of those lines, say so in the issue and
update `docs/decisions.md` in the same PR.

## Commits and PRs

Conventional-commit style, one self-contained change per commit:

```
feat: geocode-only pass for pins the uncached cap skipped
fix: a provider content block is refused, not a retryable error
docs: require exiftool and fix the environment-dependent XMP fixture
```

Keep the PR scoped to one thing, make sure `uv.lock` is committed alongside any
dependency change, and confirm the test line above passes locally before you
open it.
