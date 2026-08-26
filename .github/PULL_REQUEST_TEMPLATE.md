## What this changes

<!-- One or two sentences. Link the issue if there is one. -->

## Checklist

- [ ] `uv run pytest --disable-socket --allow-unix-socket` passes locally
- [ ] Dependencies (if any) were added with `uv add` and `uv.lock` is committed
- [ ] No new columns on the SQLite schema — v1 is frozen
- [ ] If this changes behavior covered by a numbered decision (K1–K18),
      `docs/decisions.md` is updated in this PR
- [ ] No API key, image bytes, or full home path is written to a log or an
      error message
- [ ] Commit messages are conventional-commit style

## Anything reviewers should look at closely

<!-- Optional. Tradeoffs you were unsure about, or a rail you had to bend. -->
