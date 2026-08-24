from __future__ import annotations

import asyncio
import json
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar
from unittest.mock import patch

import pytest
from httpx import ASGITransport, AsyncClient
from typer.testing import CliRunner

from gpsify.cli import app as cli_app
from gpsify.config import load_settings
from gpsify.geocode import OSM_ATTRIBUTION, FakeGeocoder, GeocodeCache, GeocodeService
from gpsify.media import hash_file, thumb_path
from gpsify.review import (
    GEOCODE_MIN_Q_LEN,
    LOW_UI_RANK,
    REVIEW_HOST,
    create_app,
    parse_photo_filter,
)
from gpsify.review.app import PhotoFilter, STATIC_DIR
from gpsify.store import JobRepo, job_dir, options_hash
from gpsify.types import DecisionAction, GeoHit, PhotoStatus, PinSource
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
JOB_ID = "22222222-2222-4222-8222-222222222222"
PORTO = "Clérigos Church, Porto, Portugal"


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


def _settings(data_dir: Path, **overrides: Any):
    payload = {
        "_env_file": None,
        "data_dir": data_dir,
        "nominatim_contact": "dev@example.com",
        "geocoder_url": "http://127.0.0.1:8080/search",
        "map_tile_url": "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
    }
    payload.update(overrides)
    return load_settings(**payload)


def _porto_hit(query: str = PORTO) -> GeoHit:
    return GeoHit(
        query=query,
        display_name="Igreja dos Clérigos, Porto, Portugal",
        lat=PORTO_LAT,
        lng=PORTO_LNG,
        osm_type="way",
        osm_id="1",
        provider="fake",
    )


async def _create_repo(data_dir: Path) -> JobRepo:
    return await JobRepo.create(
        data_dir,
        job_id=JOB_ID,
        root_path="/photos/trip",
        root_canonical="/photos/trip",
        options_hash=options_hash(recursive=False),
        hint="weekend in Porto",
        config_json="{}",
    )


async def _add_photo(
    repo: JobRepo,
    *,
    suffix: str,
    status: PhotoStatus = PhotoStatus.proposed,
    cluster_id: str | None = None,
    datetime_original: str | None = "2019:06:14 18:04:00",
    path: Path | None = None,
) -> str:
    source = path if path is not None else Path(f"/photos/trip/IMG_{suffix}.jpg")
    digest = hash_file(source) if source.is_file() else "a" * 64
    size = source.stat().st_size if source.is_file() else 1024
    return await repo.insert_photo(
        path=str(source),
        relpath=source.name if path is not None else f"IMG_{suffix}.jpg",
        sha256=digest,
        size=size,
        mtime_ns=1,
        status=status,
        datetime_original=datetime_original,
        cluster_id=cluster_id,
    )


async def _proposal(
    repo: JobRepo,
    photo_id: str,
    *,
    ui_rank: float = 0.83,
    pin_source: PinSource = PinSource.geocoder,
    pin_lat: float | None = PORTO_LAT,
    pin_lng: float | None = PORTO_LNG,
    place_name: str | None = PORTO,
    model_agree: int | None = 1,
    lat_hint: float | None = None,
    lng_hint: float | None = None,
) -> str:
    guess = {
        "refusal": "none",
        "primary_place_name": place_name,
        "country_code": "PT",
        "granularity": "exact",
        "lat_hint": lat_hint,
        "lng_hint": lng_hint,
        "model_confidence": 0.8,
        "clues": [{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        "candidates": [
            {
                "place_name": "Lisbon, Portugal",
                "country_code": "PT",
                "confidence": 0.2,
                "why": "decoy",
            }
        ],
        "used_album_hint": False,
        "used_filename_hint": False,
    }
    return await repo.insert_proposal(
        photo_id=photo_id,
        provider="xai",
        model="grok-4.6",
        prompt_version="PROMPT_V1",
        raw_json=json.dumps(guess),
        place_name=place_name,
        granularity="exact",
        model_confidence=0.8,
        ui_rank=ui_rank,
        refusal="none",
        pin_lat=pin_lat,
        pin_lng=pin_lng,
        pin_source=pin_source,
        model_agree=model_agree,
    )


async def _client_for(
    data_dir: Path,
    repo: JobRepo,
    fake: FakeGeocoder | None = None,
) -> tuple[AsyncClient, FakeGeocoder, GeocodeCache, Any]:
    cache = await GeocodeCache(data_dir / "cache" / "geocode.sqlite").open()
    geocoder = fake if fake is not None else FakeGeocoder()
    service = GeocodeService(geocoder, cache, cap_uncached=False)
    settings = _settings(data_dir)
    app = create_app(
        settings=settings,
        job_id=JOB_ID,
        repo=repo,
        service=service,
        job_root=job_dir(data_dir, JOB_ID),
    )
    transport = ASGITransport(app=app)
    client = AsyncClient(transport=transport, base_url="http://127.0.0.1")
    return client, geocoder, cache, app


def test_parse_photo_filter_default_needs_review() -> None:
    assert parse_photo_filter(None) is PhotoFilter.needs_review
    assert parse_photo_filter("needs review") is PhotoFilter.needs_review
    assert parse_photo_filter("low") is PhotoFilter.low


def test_static_search_is_enter_only() -> None:
    js = (STATIC_DIR / "app.js").read_text(encoding="utf-8")
    html = (STATIC_DIR / "index.html").read_text(encoding="utf-8")
    assert "keydown" in js
    assert 'event.key !== "Enter"' in js
    assert 'addEventListener("input"' not in js
    assert "autocomplete" not in js.lower()
    assert "© OpenStreetMap contributors" in html
    assert GEOCODE_MIN_Q_LEN == 3


def test_review_api_job_and_filters(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            cluster_id = await repo.insert_cluster(parent=".", first_ts="2019-06-14", n=2)
            proposed = await _add_photo(repo, suffix="a", cluster_id=cluster_id)
            await _proposal(repo, proposed)
            low = await _add_photo(repo, suffix="b", cluster_id=cluster_id)
            await _proposal(repo, low, ui_rank=0.40)
            refused = await _add_photo(repo, suffix="c", status=PhotoStatus.refused)
            await _proposal(
                repo,
                refused,
                ui_rank=0.10,
                pin_source=PinSource.none,
                pin_lat=None,
                pin_lng=None,
            )
            client, _fake, cache, _app = await _client_for(data_dir, repo)
            async with client:
                job = await client.get("/api/job")
                assert job.status_code == 200
                payload = job.json()
                assert payload["id"] == JOB_ID
                assert payload["attribution"] == OSM_ATTRIBUTION
                assert payload["counts"]["proposed"] == 2
                needs = await client.get("/api/photos")
                assert needs.status_code == 200
                ids = {row["id"] for row in needs.json()["photos"]}
                assert ids == {proposed, low}
                low_resp = await client.get("/api/photos?status=low")
                low_ids = {row["id"] for row in low_resp.json()["photos"]}
                assert low_ids == {low, refused}
                refused_resp = await client.get("/api/photos?status=refused")
                assert {row["id"] for row in refused_resp.json()["photos"]} == {refused}
                all_resp = await client.get("/api/photos?status=all")
                assert len(all_resp.json()["photos"]) == 3
                unknown = await client.get("/api/photos?status=nope")
                assert unknown.status_code == 400
            await cache.close()

    run(body())


def test_accept_skip_and_pin_source_none(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            good = await _add_photo(repo, suffix="a")
            await _proposal(repo, good)
            none_id = await _add_photo(repo, suffix="b")
            await _proposal(
                repo,
                none_id,
                pin_source=PinSource.none,
                pin_lat=None,
                pin_lng=None,
            )
            client, _fake, cache, _app = await _client_for(data_dir, repo)
            async with client:
                accepted = await client.post(
                    f"/api/photos/{good}/decision",
                    json={"action": "accept"},
                )
                assert accepted.status_code == 200
                photo = await repo.get_photo(good)
                decision = await repo.get_current_decision(good)
                assert photo is not None and decision is not None
                assert photo["status"] == PhotoStatus.accepted.value
                assert decision["lat"] == pytest.approx(PORTO_LAT)
                assert decision["lng"] == pytest.approx(PORTO_LNG)
                skipped = await client.post(
                    f"/api/photos/{good}/decision",
                    json={"action": "skip"},
                )
                assert skipped.status_code == 200
                photo = await repo.get_photo(good)
                assert photo is not None
                assert photo["status"] == PhotoStatus.skipped_by_user.value
                rejected = await client.post(
                    f"/api/photos/{none_id}/decision",
                    json={"action": "accept"},
                )
                assert rejected.status_code == 400
                missing_coords = await client.post(
                    f"/api/photos/{none_id}/decision",
                    json={"action": "accept_edited"},
                )
                assert missing_coords.status_code == 400
                edited = await client.post(
                    f"/api/photos/{none_id}/decision",
                    json={"action": "accept_edited", "lat": 41.15, "lng": -8.61},
                )
                assert edited.status_code == 200
                receipts = await repo._db.execute("SELECT COUNT(*) AS n FROM write_receipt")
                row = await receipts.fetchone()
                assert row is not None and int(row["n"]) == 0
            await cache.close()

    run(body())


def test_cluster_apply_explicit_ids_and_caps(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            cluster_id = await repo.insert_cluster(
                parent=".", first_ts=None, n=31, undated=1
            )
            source = await _add_photo(repo, suffix="src", cluster_id=cluster_id)
            await _proposal(repo, source)
            sibling = await _add_photo(
                repo,
                suffix="sib",
                cluster_id=cluster_id,
                datetime_original=None,
            )
            await _proposal(repo, sibling, ui_rank=0.7)
            outsider = await _add_photo(repo, suffix="out")
            await _proposal(repo, outsider)
            await repo.add_decision(source, DecisionAction.accept)
            client, _fake, cache, _app = await _client_for(data_dir, repo)
            async with client:
                listing = await client.get(f"/api/clusters/{cluster_id}")
                assert listing.status_code == 200
                body_json = listing.json()
                assert body_json["needs_confirm"] is True
                assert sibling in body_json["candidate_ids"]
                assert source not in body_json["candidate_ids"]
                extra = await client.post(
                    f"/api/clusters/{cluster_id}/apply",
                    json={
                        "source_photo_id": source,
                        "photo_ids": [sibling, outsider],
                    },
                )
                assert extra.status_code == 400
                unconfirmed = await client.post(
                    f"/api/clusters/{cluster_id}/apply",
                    json={"source_photo_id": source, "photo_ids": [sibling]},
                )
                assert unconfirmed.status_code == 400
                applied = await client.post(
                    f"/api/clusters/{cluster_id}/apply",
                    json={
                        "source_photo_id": source,
                        "photo_ids": [sibling],
                        "confirm_n": 31,
                    },
                )
                assert applied.status_code == 200
                photo = await repo.get_photo(sibling)
                decision = await repo.get_current_decision(sibling)
                assert photo is not None and decision is not None
                assert photo["status"] == PhotoStatus.accepted_edited.value
                assert decision["action"] == DecisionAction.cluster_apply.value
                assert decision["lat"] == pytest.approx(PORTO_LAT)
            await cache.close()

    run(body())


def test_geocode_enter_only_contract(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            fake = FakeGeocoder()
            fake.add("Porto", _porto_hit("Porto"))
            client, geocoder, cache, _app = await _client_for(data_dir, repo, fake=fake)
            async with client:
                too_short = await client.post("/api/geocode", json={"q": "Po"})
                assert too_short.status_code == 400
                assert geocoder.queries == []
                house = await client.post(
                    "/api/geocode",
                    json={"q": "12 Oak Street, Portland"},
                )
                assert house.status_code == 400
                assert geocoder.queries == []
                get_search = await client.get("/api/geocode?q=Porto")
                assert get_search.status_code == 405
                assert geocoder.queries == []
                hit = await client.post("/api/geocode", json={"q": "Porto"})
                assert hit.status_code == 200
                payload = hit.json()
                assert payload["hit"]["lat"] == pytest.approx(PORTO_LAT)
                assert geocoder.queries == ["Porto"]
            await cache.close()

    run(body())


def test_lazy_thumb_by_photo_id(data_dir: Path, tmp_path: Path) -> None:
    async def body() -> None:
        jpeg = write_test_jpeg(tmp_path / "plain.jpg", size=(32, 16), color=(10, 20, 30))
        before = jpeg.read_bytes()
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _add_photo(repo, suffix="a", path=jpeg)
            await _proposal(repo, photo_id)
            client, _fake, cache, _app = await _client_for(data_dir, repo)
            async with client:
                missing = await client.get("/api/thumb/00000000-0000-4000-8000-000000000000")
                assert missing.status_code == 404
                response = await client.get(f"/api/thumb/{photo_id}")
                assert response.status_code == 200
                assert response.headers["content-type"].startswith("image/jpeg")
                assert response.content[:2] == b"\xff\xd8"
                preview = await client.get(f"/api/preview/{photo_id}")
                assert preview.status_code == 200
                assert preview.content[:2] == b"\xff\xd8"
            assert jpeg.read_bytes() == before
            assert thumb_path(job_dir(data_dir, JOB_ID), photo_id).is_file()
            await cache.close()

    run(body())


def test_index_and_no_cors_wildcard(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            client, _fake, cache, app = await _client_for(data_dir, repo)
            try:
                assert app.user_middleware == []
                async with client:
                    page = await client.get("/")
                    assert page.status_code == 200
                    assert "© OpenStreetMap contributors" in page.text
                    js = await client.get("/static/app.js")
                    assert js.status_code == 200
            finally:
                await cache.close()

    run(body())


def test_review_cli_binds_loopback_only(data_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    async def seed() -> None:
        repo = await _create_repo(data_dir)
        await repo.close()

    run(seed())
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))
    captured: dict[str, Any] = {}

    def fake_serve(starlette_app: Any, port: int) -> None:
        captured["host_const"] = REVIEW_HOST
        captured["port"] = port
        captured["app"] = starlette_app

    with (
        patch("gpsify.cli.serve_review", fake_serve),
        patch("gpsify.cli.webbrowser.open", lambda url: captured.setdefault("url", url)),
    ):
        result = runner.invoke(cli_app, ["review", JOB_ID, "--port", "8765"])
    assert result.exit_code == 0, result.output
    assert captured["port"] == 8765
    assert captured["host_const"] == "127.0.0.1"
    assert captured["url"].startswith("http://127.0.0.1:8765")
    assert "0.0.0.0" not in result.output
    help_result = runner.invoke(cli_app, ["review", "--help"])
    assert help_result.exit_code == 0
    assert "127.0.0.1" in help_result.stdout


def test_review_requires_nominatim_contact(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = load_settings(
        _env_file=None,
        data_dir=data_dir,
        nominatim_contact=None,
        geocoder_url="https://nominatim.openstreetmap.org/search",
        geocoder="nominatim",
    )
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: settings)
    result = runner.invoke(cli_app, ["review", JOB_ID])
    assert result.exit_code == 2
    combined = f"{result.stdout}{result.stderr}".lower()
    assert "nominatim_contact" in combined


def test_decide_cli_accept_and_skip(data_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    async def seed() -> str:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _add_photo(repo, suffix="a")
            await _proposal(repo, photo_id)
            return photo_id

    photo_id = run(seed())
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))
    accept = runner.invoke(
        cli_app,
        ["decide", JOB_ID, "--photo", photo_id, "--action", "accept"],
    )
    assert accept.exit_code == 0, accept.output
    assert "accept" in accept.stdout
    skip = runner.invoke(
        cli_app,
        ["decide", JOB_ID, "--photo", photo_id, "--action", "skip"],
    )
    assert skip.exit_code == 0, skip.output

    async def check() -> None:
        repo = await JobRepo.open(data_dir, JOB_ID)
        async with repo:
            photo = await repo.get_photo(photo_id)
            assert photo is not None
            assert photo["status"] == PhotoStatus.skipped_by_user.value

    run(check())
    bad = runner.invoke(
        cli_app,
        ["decide", JOB_ID, "--photo", photo_id, "--action", "cluster_apply"],
    )
    assert bad.exit_code == 2


def test_skip_all_low_preview_and_yes(data_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    async def seed() -> tuple[str, str]:
        repo = await _create_repo(data_dir)
        async with repo:
            low = await _add_photo(repo, suffix="low")
            await _proposal(repo, low, ui_rank=0.40)
            high = await _add_photo(repo, suffix="high")
            await _proposal(repo, high, ui_rank=0.90)
            return low, high

    low_id, high_id = run(seed())
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))
    preview = runner.invoke(cli_app, ["skip", JOB_ID, "--all-low"])
    assert preview.exit_code == 2, preview.output
    combined = f"{preview.stdout}{preview.stderr}"
    assert low_id in combined
    assert high_id not in combined
    assert str(LOW_UI_RANK) in combined or "0.55" in combined

    async def still_proposed() -> None:
        repo = await JobRepo.open(data_dir, JOB_ID)
        async with repo:
            photo = await repo.get_photo(low_id)
            assert photo is not None
            assert photo["status"] == PhotoStatus.proposed.value

    run(still_proposed())
    written = runner.invoke(cli_app, ["skip", JOB_ID, "--all-low", "--yes"])
    assert written.exit_code == 0, written.output
    assert "skipped 1" in written.stdout

    async def skipped() -> None:
        repo = await JobRepo.open(data_dir, JOB_ID)
        async with repo:
            photo = await repo.get_photo(low_id)
            high = await repo.get_photo(high_id)
            assert photo is not None and high is not None
            assert photo["status"] == PhotoStatus.skipped_by_user.value
            assert high["status"] == PhotoStatus.proposed.value
            receipts = await repo._db.execute("SELECT COUNT(*) AS n FROM write_receipt")
            row = await receipts.fetchone()
            assert row is not None and int(row["n"]) == 0

    run(skipped())
    missing_flag = runner.invoke(cli_app, ["skip", JOB_ID])
    assert missing_flag.exit_code == 2
