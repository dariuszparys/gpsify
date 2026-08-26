# Security Policy

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Report it through GitHub's private vulnerability reporting:
[**Report a vulnerability**](https://github.com/dariuszparys/gpsify/security/advisories/new).
That channel is private until a fix is published.

This is a personal project maintained in spare time. Expect an acknowledgement
within a week or so, not within hours.

## Supported versions

Only the tip of `main` is supported. There are no released versions and no
backport branches.

## What is in scope

gpsify handles two things worth protecting: **API keys** and **personal photos**.
Reports touching either are especially welcome. Concretely:

- A path by which a key (`XAI_API_KEY`, `GEMINI_API_KEY`) reaches disk, a log
  file, a URL, an error message, or a network request other than its own
  provider's endpoint.
- A path by which image bytes, EXIF, or a filesystem path leaves the machine to
  anywhere other than the selected vision provider and the configured geocoder —
  see [What this sends where](README.md#what-this-sends-where).
- Anything that makes the review UI reachable from outside `127.0.0.1`, or that
  serves original files rather than generated thumbnails.
- Anything that writes to a photo without a stored, current human decision, or
  that destroys an original without a recoverable backup.
- Path traversal in job handles, `--out` destinations, or the review UI's
  `photo_id` lookups.

## What is not in scope

- **Vision models return wrong locations.** That is the premise of the tool, not
  a vulnerability — which is why nothing is written without human review.
- The accuracy or availability of xAI, Google, or Nominatim.
- Anything requiring an attacker to already have write access to `~/.gpsify` or
  your shell environment.
- Findings against a modified `geocoder_url` or `map_tile_url` you pointed at a
  host you do not trust.

## Handling your own keys

gpsify never writes a key to its job store or its logs. Keep keys in `.env`
(gitignored) or the environment. If you suspect a key leaked, rotate it at the
provider first — that is faster and more complete than anything this project
can do for you.
