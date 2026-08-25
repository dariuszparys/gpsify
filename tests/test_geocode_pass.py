"""``run_geocode`` finishes the geocode half of propose without spending vision.

The uncached cap only applies to a non-``FakeGeocoder`` pointed at public
Nominatim, so these tests drive a local double that is not ``FakeGeocoder``.
No socket is opened: the double answers in-process.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Coroutine
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TypeVar

import pytest
from typer.testing import CliRunner

from gpsify.cli import app
from gpsify.config import load_settings
from gpsify.ingest import run_ingest
from gpsify.propose import ProposeError, run_geocode, run_propose
from gpsify.providers import FakeVisionProvider
from gpsify.store import DecisionError, JobRepo
from gpsify.types import (
    DecisionAction,
    GeoHit,
    Granularity,
    PhotoStatus,
    PinSource,
    RefusalReason,
    VisionGuess,
)
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
PORTO = "Clérigos Church, Porto, Portugal"
PUBLIC_NOMINATIM = "https://nominatim.openstreetmap.org/search"


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


@dataclass
class CappableGeocoder:
    """A ``Geocoder`` that is not ``FakeGeocoder``, so the uncached cap applies.

    ``GeocodeService`` exempts ``FakeGeocoder`` from the cap, which is exactly
    the path these tests need to exercise.
    """

    name: str = "cappable"
    hits: dict[str, list[GeoHit]] = field(default_factory=dict)
    queries: list[str] = field(default_factory=list)

    def add(self, query: str, *hits: GeoHit) -> None:
        self.hits[query] = list(hits)

    async def search(
        self,
        query: str,
        *,
        country_code: str | None = None,
        lang: str = "en",
    ) -> list[GeoHit]:
        _ = (country_code, lang)
        self.queries.append(query)
        return list(self.hits.get(query, []))


def _settings(data_dir: Path, **overrides: Any):
    payload: dict[str, Any] = {
        "_env_file": None,
        "data_dir": data_dir,
        "xai_api_key": "test-key",
        "nominatim_contact": "dev@example.com",
        # Public Nominatim so the uncached cap is live, as in a real run.
        "geocoder_url": PUBLIC_NOMINATIM,
        "reasoning_effort": "low",
        "timeout_s": 120.0,
        "model": "grok-4.6",
    }
    payload.update(overrides)
    return load_settings(**payload)


def _guess(**overrides: Any) -> VisionGuess:
    payload: dict[str, Any] = {
        "refusal": RefusalReason.none,
        "primary_place_name": PORTO,
        "country_code": "PT",
        "granularity": Granularity.exact,
        "model_confidence": 0.8,
        "clues": [{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        "candidates": [],
        "used_album_hint": False,
        "used_filename_hint": False,
    }
    payload.update(overrides)
    return VisionGuess.model_validate(payload)


def _porto_hit() -> GeoHit:
    return GeoHit(
        query=PORTO,
        display_name="Igreja dos Clérigos, Porto, Portugal",
        lat=PORTO_LAT,
        lng=PORTO_LNG,
        osm_type="way",
        osm_id="1",
        provider="cappable",
    )


def _ingest_plain(folder: Path, data_dir: Path, **kw: Any):
    folder.mkdir(parents=True, exist_ok=True)
    write_test_jpeg(
        folder / "plain.jpg",
        datetime_original="2019:06:14 18:04:00",
        color=(40, 120, 180),
    )
    return run(run_ingest(folder, data_dir=data_dir, **kw))


async def _noop_sleep(_: float) -> None:
    return None


def _propose_capped(
    tmp_path: Path, data_dir: Path, guess: VisionGuess, **settings_kw: Any
) -> str:
    """Propose one photo with the uncached cap at zero: a pinless proposal.

    A guess the geocoder is never allowed to see (refusal, house address) never
    reaches the cap, so the cap warning is asserted per-test, not here.
    """
    ingested = _ingest_plain(tmp_path / "album", data_dir)
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir, **settings_kw),
            vision=FakeVisionProvider(guess_result=guess),
            geocoder=CappableGeocoder(),
            concurrency=1,
            geocode_uncached_max=0,
            sleep=_noop_sleep,
        )
    )
    return ingested.job_id


async def _current(data_dir: Path, job_id: str) -> tuple[dict[str, Any], dict[str, Any]]:
    async with await JobRepo.open(data_dir, job_id) as repo:
        photo = (await repo.list_photos())[0]
        proposal = await repo.get_current_proposal(photo["id"])
        assert proposal is not None
        return dict(photo), dict(proposal)


def test_cap_leaves_a_pinless_proposal_a_bare_accept_cannot_use(
    tmp_path: Path, data_dir: Path
) -> None:
    """The starting condition: vision spent, no pin, nothing to accept."""
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    photo, proposal = run(_current(data_dir, job_id))
    assert photo["status"] == PhotoStatus.proposed.value
    assert proposal["pin_source"] == PinSource.none.value
    assert json.loads(str(proposal["geocode_json"]))["skipped_geocode"] == "uncached_cap"

    async def accept() -> None:
        async with await JobRepo.open(data_dir, job_id) as repo:
            with pytest.raises(DecisionError, match="pin_source"):
                await repo.add_decision(photo["id"], DecisionAction.accept)

    run(accept())


def test_geocode_fills_the_pin_and_never_calls_vision(
    tmp_path: Path, data_dir: Path
) -> None:
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    _, before = run(_current(data_dir, job_id))
    geocoder = CappableGeocoder()
    geocoder.add(PORTO, _porto_hit())

    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=geocoder,
            geocode_uncached_max=50,
        )
    )
    assert (summary.planned, summary.pinned, summary.still_unpinned) == (1, 1, 0)
    assert summary.warnings == []
    assert geocoder.queries == [PORTO]

    photo, after = run(_current(data_dir, job_id))
    assert after["id"] != before["id"], "a re-geocode appends a row, never rewrites one"
    assert after["pin_source"] == PinSource.geocoder.value
    assert (after["pin_lat"], after["pin_lng"]) == (PORTO_LAT, PORTO_LNG)
    assert after["raw_json"] == before["raw_json"], "the stored guess is reused verbatim"
    # No vision call happened, so the new row must not restate the old spend.
    assert (after["input_tokens"], after["output_tokens"]) == (0, 0)
    assert float(after["ui_rank"]) > float(before["ui_rank"])
    assert photo["status"] == PhotoStatus.proposed.value

    async def accept() -> None:
        async with await JobRepo.open(data_dir, job_id) as repo:
            await repo.add_decision(photo["id"], DecisionAction.accept)
            decision = await repo.get_current_decision(photo["id"])
            assert decision is not None
            assert (decision["lat"], decision["lng"]) == (PORTO_LAT, PORTO_LNG)

    run(accept())


def test_geocode_promotes_a_photo_the_cap_pushed_below_the_rank_cutoff(
    tmp_path: Path, data_dir: Path
) -> None:
    """A missing pin costs 0.08 of ui_rank, enough to refuse a borderline photo."""
    guess = _guess(granularity=Granularity.city, model_confidence=0.1)
    job_id = _propose_capped(tmp_path, data_dir, guess)
    photo, _ = run(_current(data_dir, job_id))
    assert photo["status"] == PhotoStatus.refused.value

    geocoder = CappableGeocoder()
    geocoder.add(PORTO, _porto_hit())
    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=geocoder,
            geocode_uncached_max=50,
        )
    )
    assert (summary.pinned, summary.unrefused) == (1, 1)

    photo, proposal = run(_current(data_dir, job_id))
    assert photo["status"] == PhotoStatus.proposed.value
    assert proposal["pin_source"] == PinSource.geocoder.value


def test_geocode_never_queries_a_refusal(tmp_path: Path, data_dir: Path) -> None:
    """K10: a privacy_sensitive place name is never sent to the geocoder."""
    guess = _guess(
        refusal=RefusalReason.privacy_sensitive,
        primary_place_name="someone's house",
    )
    job_id = _propose_capped(tmp_path, data_dir, guess)
    photo, _ = run(_current(data_dir, job_id))
    assert photo["status"] == PhotoStatus.refused.value

    geocoder = CappableGeocoder()
    geocoder.add("someone's house", _porto_hit())
    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=geocoder,
            geocode_uncached_max=50,
        )
    )
    assert summary.planned == 0
    assert geocoder.queries == []


def test_geocode_never_queries_a_house_address(tmp_path: Path, data_dir: Path) -> None:
    guess = _guess(
        primary_place_name="12 Oak Street, Portland",
        granularity=Granularity.street,
    )
    job_id = _propose_capped(tmp_path, data_dir, guess)
    geocoder = CappableGeocoder()
    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=geocoder,
            geocode_uncached_max=50,
        )
    )
    assert summary.planned == 0
    assert geocoder.queries == []


def test_capped_again_leaves_the_existing_row_untouched(
    tmp_path: Path, data_dir: Path
) -> None:
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    _, before = run(_current(data_dir, job_id))

    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=CappableGeocoder(),
            geocode_uncached_max=0,
        )
    )
    assert (summary.pinned, summary.still_unpinned) == (0, 1)
    assert summary.warnings, "expected the cap warning again"

    _, after = run(_current(data_dir, job_id))
    assert after["id"] == before["id"], "nothing was learned; no row should be appended"


def test_a_pinned_proposal_is_not_re_geocoded(tmp_path: Path, data_dir: Path) -> None:
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    geocoder = CappableGeocoder()
    geocoder.add(PORTO, _porto_hit())
    settings = _settings(data_dir)
    run(run_geocode(job_id, settings=settings, geocoder=geocoder, geocode_uncached_max=50))
    assert geocoder.queries == [PORTO]

    again = run(
        run_geocode(job_id, settings=settings, geocoder=geocoder, geocode_uncached_max=50)
    )
    assert again.planned == 0
    assert geocoder.queries == [PORTO]


def test_dry_run_counts_the_cold_lookups_without_network(
    tmp_path: Path, data_dir: Path
) -> None:
    """The number to compare against ``--geocode-uncached-max``, for free."""
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    geocoder = CappableGeocoder()
    messages: list[str] = []
    summary = run(
        run_geocode(
            job_id,
            settings=_settings(data_dir),
            geocoder=geocoder,
            dry_run=True,
            progress=messages.append,
        )
    )
    assert (summary.planned, summary.distinct_places, summary.cold_places) == (1, 1, 1)
    assert geocoder.queries == []
    assert any("no geocoder HTTP" in line for line in messages)


def test_geocode_stats_are_cumulative(tmp_path: Path, data_dir: Path) -> None:
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    geocoder = CappableGeocoder()
    geocoder.add(PORTO, _porto_hit())
    settings = _settings(data_dir)
    run(run_geocode(job_id, settings=settings, geocoder=geocoder, geocode_uncached_max=50))
    run(run_geocode(job_id, settings=settings, geocoder=geocoder, geocode_uncached_max=50))

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, job_id) as repo:
            job = await repo.get_job()
            stats = json.loads(str(job["stats_json"]))
            assert stats["geocode"]["pinned"] == 1
            assert stats["geocode"]["runs"] == 2
            assert stats["geocode"]["last_run"]["planned"] == 0
            # The propose block's spend survives a geocode pass untouched.
            assert stats["propose"]["est_usd"] > 0

    run(inspect())


def test_bad_limit_is_a_usage_error(tmp_path: Path, data_dir: Path) -> None:
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    with pytest.raises(ProposeError, match="--limit"):
        run(run_geocode(job_id, settings=_settings(data_dir), limit=0))


def test_missing_nominatim_contact_exits_2(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("NOMINATIM_CONTACT", raising=False)
    job_id = _propose_capped(tmp_path, data_dir, _guess())
    settings = load_settings(
        _env_file=None,
        data_dir=data_dir,
        xai_api_key="k",
        nominatim_contact=None,
        geocoder_url=PUBLIC_NOMINATIM,
    )
    with pytest.raises(ProposeError, match="nominatim_contact"):
        run(run_geocode(job_id, settings=settings, dry_run=True))


def test_cli_geocode_help_and_unknown_job(tmp_path: Path) -> None:
    helped = runner.invoke(app, ["geocode", "--help"])
    assert helped.exit_code == 0

    result = runner.invoke(
        app,
        ["geocode", "00000000-0000-0000-0000-000000000000", "--dry-run"],
        env={"GPSIFY_DATA_DIR": str(tmp_path / "empty")},
    )
    assert result.exit_code == 2
