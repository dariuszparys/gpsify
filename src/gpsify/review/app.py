"""Starlette review API + static Leaflet UI (PR 7; grid + selection apply, PR 12).

Binds ``127.0.0.1`` only. Thumbs and previews are lazy and looked up by
``photo_id`` UUID — originals are never mounted. No EXIF writes.

Two routes copy one photo's accepted pin onto others. ``/api/clusters/{id}/apply``
is scoped to one cluster; ``/api/pin/apply`` takes an arbitrary grid selection,
which can span clusters. Both go through ``_apply_source_pin`` so K9's rails hold
either way: ``proposed``-only candidates, an explicit id list where any extra id
is a 400 rather than a silent drop, and a typed confirm when the batch is large,
undated, or mostly untimestamped. A selection is just a cluster the human drew,
so it gets the same caps.
"""

from __future__ import annotations

import json
from contextlib import asynccontextmanager
from enum import Enum
from pathlib import Path
from typing import Any, Never

from pydantic import BaseModel, Field, ValidationError
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import FileResponse, JSONResponse, Response
from starlette.routing import Mount, Route
from starlette.staticfiles import StaticFiles

from gpsify.config import Settings
from gpsify.exif import classify_media
from gpsify.geocode import (
    OSM_ATTRIBUTION,
    GeocodeCache,
    GeocodeService,
    geocode_cache_path,
    looks_like_house_address,
    require_geocoder_ready,
)
from gpsify.geocode.nominatim import NominatimGeocoder
from gpsify.media import MediaError, ensure_preview, ensure_thumb
from gpsify.store import DecisionError, JobRepo, StoreError, job_dir
from gpsify.types import (
    DecisionAction,
    Granularity,
    PhotoStatus,
    VisionGuess,
)

REVIEW_HOST = "127.0.0.1"
LOW_UI_RANK = 0.55
GEOCODE_MIN_Q_LEN = 3
CLUSTER_N_CAP = 30
CLUSTER_UNSTAMPED_FRACTION = 0.25
STATIC_DIR = Path(__file__).resolve().parent / "static"


class ReviewError(Exception):
    """User-facing review / decide / skip failure (CLI exit 2)."""


class PhotoFilter(str, Enum):
    needs_review = "needs_review"
    low = "low"
    refused = "refused"
    accepted = "accepted"
    all = "all"


class DecisionBody(BaseModel):
    action: DecisionAction
    lat: float | None = None
    lng: float | None = None
    place_name: str | None = None


class ClusterApplyBody(BaseModel):
    source_photo_id: str
    photo_ids: list[str] = Field(min_length=1)
    confirm_n: int | None = None


class PinApplyBody(BaseModel):
    """An arbitrary grid selection. Same shape as a cluster apply, no cluster id."""

    source_photo_id: str
    photo_ids: list[str] = Field(min_length=1)
    confirm_n: int | None = None


class GeocodeBody(BaseModel):
    q: str
    country_code: str | None = None


def _unreachable(value: Never) -> Never:
    raise ReviewError(f"unhandled value: {value!r}")


def _error(message: str, status: int = 400) -> JSONResponse:
    return JSONResponse({"error": message}, status_code=status)


def _json(payload: Any, status: int = 200) -> JSONResponse:
    return JSONResponse(payload, status_code=status)


def parse_photo_filter(raw: str | None) -> PhotoFilter:
    if raw is None or raw.strip() == "":
        return PhotoFilter.needs_review
    key = raw.strip().lower().replace(" ", "_")
    try:
        return PhotoFilter(key)
    except ValueError:
        raise ReviewError(f"unknown status filter {raw!r}") from None


def _parse_guess(raw_json: str | None) -> VisionGuess | None:
    if not raw_json:
        return None
    try:
        return VisionGuess.model_validate_json(raw_json)
    except ValidationError:
        return None


def _photo_matches(
    photo: dict[str, Any],
    proposal: dict[str, Any] | None,
    filt: PhotoFilter,
) -> bool:
    status = photo["status"]
    match filt:
        case PhotoFilter.needs_review:
            return status == PhotoStatus.proposed.value
        case PhotoFilter.low:
            if proposal is None or proposal.get("ui_rank") is None:
                return False
            return float(proposal["ui_rank"]) < LOW_UI_RANK
        case PhotoFilter.refused:
            return status == PhotoStatus.refused.value
        case PhotoFilter.accepted:
            return status in {
                PhotoStatus.accepted.value,
                PhotoStatus.accepted_edited.value,
            }
        case PhotoFilter.all:
            return True
        case _:
            _unreachable(filt)


def serialize_proposal(row: dict[str, Any] | None) -> dict[str, Any] | None:
    if row is None:
        return None
    guess = _parse_guess(row.get("raw_json"))
    clues = [c.model_dump() for c in guess.clues] if guess is not None else []
    candidates = [c.model_dump() for c in guess.candidates] if guess is not None else []
    return {
        "id": row["id"],
        "place_name": row.get("place_name"),
        "granularity": row.get("granularity"),
        "model_confidence": row.get("model_confidence"),
        "ui_rank": row.get("ui_rank"),
        "refusal": row.get("refusal"),
        "pin_lat": row.get("pin_lat"),
        "pin_lng": row.get("pin_lng"),
        "pin_source": row.get("pin_source"),
        "model_agree": row.get("model_agree"),
        "clues": clues,
        "candidates": candidates,
        "lat_hint": guess.lat_hint if guess is not None else None,
        "lng_hint": guess.lng_hint if guess is not None else None,
    }


def serialize_decision(row: dict[str, Any] | None) -> dict[str, Any] | None:
    if row is None:
        return None
    return {
        "id": row["id"],
        "action": row["action"],
        "lat": row.get("lat"),
        "lng": row.get("lng"),
        "place_name": row.get("place_name"),
        "decided_at": row.get("decided_at"),
    }


def serialize_photo(
    photo: dict[str, Any],
    proposal: dict[str, Any] | None,
    decision: dict[str, Any] | None,
) -> dict[str, Any]:
    return {
        "id": photo["id"],
        "relpath": photo["relpath"],
        "status": photo["status"],
        "cluster_id": photo.get("cluster_id"),
        "datetime_original": photo.get("datetime_original"),
        "has_gps": bool(photo.get("has_gps")),
        "existing_lat": photo.get("existing_lat"),
        "existing_lng": photo.get("existing_lng"),
        "proposal": serialize_proposal(proposal),
        "decision": serialize_decision(decision),
    }


def cluster_apply_candidates(members: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [row for row in members if row["status"] == PhotoStatus.proposed.value]


def batch_needs_confirm(
    n: int,
    candidates: list[dict[str, Any]],
    applying: list[dict[str, Any]],
) -> bool:
    """K9's caps over any batch: too big, undated, or mostly untimestamped.

    ``n`` is the size of the group being reasoned about — a cluster's member
    count, or the size of a hand-drawn selection. A wrong pin copied onto 400
    undated exports is the failure this exists to make loud.
    """
    if n > CLUSTER_N_CAP:
        return True
    if applying and all(not row.get("datetime_original") for row in applying):
        return True
    if not candidates:
        return False
    missing = sum(1 for row in candidates if not row.get("datetime_original"))
    return (missing / len(candidates)) > CLUSTER_UNSTAMPED_FRACTION


def cluster_needs_confirm(
    cluster: dict[str, Any],
    candidates: list[dict[str, Any]],
    applying: list[dict[str, Any]],
) -> bool:
    """``batch_needs_confirm`` scoped to a cluster's own member count."""
    return batch_needs_confirm(int(cluster["n"]), candidates, applying)


async def list_low_rank_photos(repo: JobRepo) -> list[dict[str, Any]]:
    """Proposed photos whose current ``ui_rank`` is below the low-rank cutoff."""
    photos = await repo.list_photos()
    proposals = {row["photo_id"]: row for row in await repo.list_current_proposals()}
    out: list[dict[str, Any]] = []
    for photo in photos:
        if photo["status"] != PhotoStatus.proposed.value:
            continue
        proposal = proposals.get(photo["id"])
        if proposal is None or proposal.get("ui_rank") is None:
            continue
        rank = float(proposal["ui_rank"])
        if rank < LOW_UI_RANK:
            out.append(
                {
                    "id": photo["id"],
                    "relpath": photo["relpath"],
                    "ui_rank": rank,
                    "place_name": proposal.get("place_name"),
                }
            )
    return out


def _state(request: Request) -> Any:
    return request.app.state


async def api_job(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    settings: Settings = _state(request).settings
    job = await repo.get_job()
    if job is None:
        return _error("job not found", 404)
    counts = await repo.count_photos_by_status()
    return _json(
        {
            "id": job["id"],
            "root_path": job["root_path"],
            "hint": job.get("hint"),
            "counts": counts,
            "attribution": OSM_ATTRIBUTION,
            "map_tile_url": settings.map_tile_url,
        }
    )


async def api_photos(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    try:
        filt = parse_photo_filter(request.query_params.get("status"))
    except ReviewError as exc:
        return _error(str(exc))
    photos = await repo.list_photos()
    proposals = {row["photo_id"]: row for row in await repo.list_current_proposals()}
    decisions = {row["photo_id"]: row for row in await repo.list_current_decisions()}
    records = []
    for photo in photos:
        proposal = proposals.get(photo["id"])
        if not _photo_matches(photo, proposal, filt):
            continue
        records.append(serialize_photo(photo, proposal, decisions.get(photo["id"])))
    return _json({"photos": records, "filter": filt.value})


async def api_photo_detail(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    photo_id = request.path_params["photo_id"]
    photo = await repo.get_photo(photo_id)
    if photo is None:
        return _error("photo not found", 404)
    proposal = await repo.get_current_proposal(photo_id)
    decision = await repo.get_current_decision(photo_id)
    return _json(serialize_photo(photo, proposal, decision))


async def _serve_derived(request: Request, *, preview: bool) -> Response:
    repo: JobRepo = _state(request).repo
    photo_id = request.path_params["photo_id"]
    photo = await repo.get_photo(photo_id)
    if photo is None:
        return _error("photo not found", 404)
    source = Path(str(photo["path"]))
    if not source.is_file():
        return _error("source missing", 404)
    job_root: Path = _state(request).job_root
    try:
        dest = (
            ensure_preview(
                job_root,
                photo_id,
                source,
                expected_sha256=str(photo["sha256"]),
                kind=classify_media(source),
            )
            if preview
            else ensure_thumb(
                job_root,
                photo_id,
                source,
                expected_sha256=str(photo["sha256"]),
                kind=classify_media(source),
            )
        )
    except MediaError as exc:
        return _error(str(exc), 404)
    return FileResponse(dest, media_type="image/jpeg")


async def api_thumb(request: Request) -> Response:
    return await _serve_derived(request, preview=False)


async def api_preview(request: Request) -> Response:
    return await _serve_derived(request, preview=True)


async def api_cluster(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    cluster_id = request.path_params["cluster_id"]
    cluster = await repo.get_cluster(cluster_id)
    if cluster is None:
        return _error("cluster not found", 404)
    members = await repo.list_photos_in_cluster(cluster_id)
    proposals = {row["photo_id"]: row for row in await repo.list_current_proposals()}
    payload_members = []
    for photo in members:
        proposal = proposals.get(photo["id"])
        payload_members.append(
            {
                "photo_id": photo["id"],
                "status": photo["status"],
                "ui_rank": None if proposal is None else proposal.get("ui_rank"),
                "place": None if proposal is None else proposal.get("place_name"),
                "has_datetime": bool(photo.get("datetime_original")),
                "relpath": photo["relpath"],
            }
        )
    candidates = cluster_apply_candidates(members)
    return _json(
        {
            "id": cluster["id"],
            "parent": cluster["parent"],
            "n": cluster["n"],
            "undated": cluster["undated"],
            "first_ts": cluster.get("first_ts"),
            "needs_confirm": cluster_needs_confirm(cluster, candidates, candidates),
            "candidate_ids": [row["id"] for row in candidates],
            "members": payload_members,
        }
    )


async def api_decision(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    photo_id = request.path_params["photo_id"]
    try:
        body = DecisionBody.model_validate(await request.json())
    except (json.JSONDecodeError, ValidationError) as exc:
        return _error(f"invalid body: {exc}")
    match body.action:
        case DecisionAction.accept | DecisionAction.skip | DecisionAction.accept_edited:
            pass
        case DecisionAction.cluster_apply:
            return _error("cluster_apply belongs on POST /api/clusters/{id}/apply")
        case _:
            _unreachable(body.action)
    try:
        decision_id = await repo.add_decision(
            photo_id,
            body.action,
            lat=body.lat,
            lng=body.lng,
            place_name=body.place_name,
        )
    except DecisionError as exc:
        return _error(str(exc))
    except StoreError as exc:
        return _error(str(exc), 404)
    decision = await repo.get_current_decision(photo_id)
    photo = await repo.get_photo(photo_id)
    return _json(
        {
            "decision_id": decision_id,
            "photo": serialize_photo(
                photo or {"id": photo_id, "relpath": "", "status": ""},
                await repo.get_current_proposal(photo_id),
                decision,
            ),
        }
    )


async def _apply_source_pin(
    repo: JobRepo,
    *,
    source_photo_id: str,
    photo_ids: list[str],
    confirm_n: int | None,
    pool: list[dict[str, Any]],
    n: int,
    label: str,
) -> Response:
    """Copy the source photo's accepted pin onto ``photo_ids``.

    ``pool`` is the set the request is allowed to touch — a cluster's members, or
    the selected photos themselves. Validation order matters and is preserved
    from the cluster route: the source must exist in the pool, must carry a
    current decision, and that decision must have coordinates, before any id is
    checked. An id outside the candidate set is a 400, never a silent drop, so a
    stale UI can never quietly widen a write (K9).
    """
    by_id = {row["id"]: row for row in pool}
    if source_photo_id not in by_id:
        return _error(f"source_photo_id is not in this {label}")
    source_decision = await repo.get_current_decision(source_photo_id)
    if source_decision is None:
        return _error("source photo has no current decision")
    lat = source_decision.get("lat")
    lng = source_decision.get("lng")
    if lat is None or lng is None:
        return _error("source decision has no coordinates")
    candidates = cluster_apply_candidates(pool)
    allowed = {row["id"] for row in candidates}
    requested = list(dict.fromkeys(photo_ids))
    extra = [pid for pid in requested if pid not in allowed]
    if extra:
        return _error(f"photo_ids not in proposed candidate set: {extra}")
    applying = [by_id[pid] for pid in requested]
    if batch_needs_confirm(n, candidates, applying) and confirm_n != n:
        return _error(f"{label} apply requires confirm_n equal to {label} size ({n})")
    decision_ids: list[str] = []
    try:
        for pid in requested:
            decision_ids.append(
                await repo.add_decision(
                    pid,
                    DecisionAction.cluster_apply,
                    lat=float(lat),
                    lng=float(lng),
                    place_name=source_decision.get("place_name"),
                    proposal_id=source_decision.get("proposal_id"),
                )
            )
    except DecisionError as exc:
        return _error(str(exc))
    return _json({"decision_ids": decision_ids, "count": len(decision_ids)})


async def api_cluster_apply(request: Request) -> Response:
    repo: JobRepo = _state(request).repo
    cluster_id = request.path_params["cluster_id"]
    try:
        body = ClusterApplyBody.model_validate(await request.json())
    except (json.JSONDecodeError, ValidationError) as exc:
        return _error(f"invalid body: {exc}")
    cluster = await repo.get_cluster(cluster_id)
    if cluster is None:
        return _error("cluster not found", 404)
    members = await repo.list_photos_in_cluster(cluster_id)
    return await _apply_source_pin(
        repo,
        source_photo_id=body.source_photo_id,
        photo_ids=body.photo_ids,
        confirm_n=body.confirm_n,
        pool=members,
        n=int(cluster["n"]),
        label="cluster",
    )


async def api_pin_apply(request: Request) -> Response:
    """Apply one accepted pin to a hand-drawn grid selection, across clusters.

    The pool is the selection itself plus the source, so the caps in
    ``batch_needs_confirm`` measure what the human actually picked. A selection
    is not safer than a cluster just because a human drew it — 40 undated photos
    still demand a typed confirm.
    """
    repo: JobRepo = _state(request).repo
    try:
        body = PinApplyBody.model_validate(await request.json())
    except (json.JSONDecodeError, ValidationError) as exc:
        return _error(f"invalid body: {exc}")
    wanted = list(dict.fromkeys([body.source_photo_id, *body.photo_ids]))
    pool: list[dict[str, Any]] = []
    for pid in wanted:
        photo = await repo.get_photo(pid)
        if photo is None:
            return _error(f"unknown photo_id: {pid}", 404)
        pool.append(photo)
    return await _apply_source_pin(
        repo,
        source_photo_id=body.source_photo_id,
        photo_ids=body.photo_ids,
        confirm_n=body.confirm_n,
        pool=pool,
        n=len(dict.fromkeys(body.photo_ids)),
        label="selection",
    )


async def api_geocode(request: Request) -> Response:
    service: GeocodeService = _state(request).service
    try:
        body = GeocodeBody.model_validate(await request.json())
    except (json.JSONDecodeError, ValidationError) as exc:
        return _error(f"invalid body: {exc}")
    query = body.q.strip()
    if len(query) < GEOCODE_MIN_Q_LEN:
        return _error(f"q must be at least {GEOCODE_MIN_Q_LEN} characters")
    if looks_like_house_address(query):
        return _error("refusing to geocode a house-address string")
    result = await service.lookup_primary(
        query,
        body.country_code,
        Granularity.unknown,
    )
    if result.skipped == "house_address":
        return _error("refusing to geocode a house-address string")
    hit = result.hit
    return _json(
        {
            "hit": None if hit is None else hit.model_dump(),
            "cache_hit": result.cache_hit,
            "skipped": result.skipped,
        }
    )


async def index_page(request: Request) -> Response:
    del request
    return FileResponse(STATIC_DIR / "index.html", media_type="text/html")


def create_app(
    *,
    settings: Settings,
    job_id: str | None = None,
    repo: JobRepo | None = None,
    service: GeocodeService | None = None,
    job_root: Path | None = None,
) -> Starlette:
    """Build the review app. Tests inject ``repo`` / ``service``; CLI uses lifespan."""

    @asynccontextmanager
    async def lifespan(app: Starlette):
        owned_repo = False
        owned_cache: GeocodeCache | None = None
        owned_client: NominatimGeocoder | None = None
        if app.state.repo is None:
            if job_id is None:
                raise ReviewError("job_id is required to start review")
            require_geocoder_ready(settings)
            opened = await JobRepo.open(settings.data_dir, job_id)
            cache = await GeocodeCache(geocode_cache_path(settings.data_dir)).open()
            client = NominatimGeocoder(
                settings.geocoder_url,
                contact=settings.nominatim_contact,
                provider=settings.geocoder,
            )
            app.state.repo = opened
            app.state.job_root = job_dir(settings.data_dir, job_id)
            app.state.service = GeocodeService(
                client,
                cache,
                lang=settings.geocode_lang,
                cap_uncached=False,
            )
            owned_repo = True
            owned_cache = cache
            owned_client = client
        yield
        if owned_repo and app.state.repo is not None:
            await app.state.repo.close()
        if owned_cache is not None:
            await owned_cache.close()
        if owned_client is not None:
            await owned_client.aclose()

    routes = [
        Route("/", index_page),
        Route("/api/job", api_job),
        Route("/api/photos", api_photos),
        Route("/api/photos/{photo_id}", api_photo_detail),
        Route("/api/photos/{photo_id}/decision", api_decision, methods=["POST"]),
        Route("/api/thumb/{photo_id}", api_thumb),
        Route("/api/preview/{photo_id}", api_preview),
        Route("/api/clusters/{cluster_id}", api_cluster),
        Route("/api/clusters/{cluster_id}/apply", api_cluster_apply, methods=["POST"]),
        Route("/api/pin/apply", api_pin_apply, methods=["POST"]),
        Route("/api/geocode", api_geocode, methods=["POST"]),
        Mount("/static", StaticFiles(directory=STATIC_DIR), name="static"),
    ]
    app = Starlette(routes=routes, lifespan=lifespan)
    app.state.settings = settings
    app.state.job_id = job_id
    app.state.repo = repo
    app.state.service = service
    app.state.job_root = job_root
    return app


def serve_review(app: Starlette, port: int) -> None:
    """Bind loopback only. There is no LAN / ``0.0.0.0`` flag in v1."""
    import uvicorn

    uvicorn.run(app, host=REVIEW_HOST, port=port, access_log=False)
