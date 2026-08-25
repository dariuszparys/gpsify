from __future__ import annotations

import asyncio
import json
import os
import re
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import pytest
from typer.testing import CliRunner

from gpsify.cli import app
from gpsify.config import load_settings
from gpsify.geocode import FakeGeocoder, GeocodeCache, GeocodeService
from gpsify.ingest import run_ingest
from gpsify.propose import (
    JobExclusiveLock,
    ProposeError,
    clue_score,
    cluster_score,
    estimate_usd,
    geocode_score,
    granularity_score,
    hint_names_city,
    refuse_guess_reason,
    run_propose,
    ui_rank,
)
from gpsify.providers import FakeVisionProvider, format_user_message
from gpsify.reconcile import ReconcileResult
from gpsify.store import JobRepo, job_dir
from gpsify.types import (
    GeoHit,
    Granularity,
    PhotoStatus,
    PinSource,
    RefusalReason,
    Usage,
    VisionGuess,
)
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
PORTO = "Clérigos Church, Porto, Portugal"
_ANSI_SGR = re.compile(r"\x1b\[[0-9;]*m")


def _plain(text: str) -> str:
    return _ANSI_SGR.sub("", text)


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
        "xai_api_key": "test-key",
        "nominatim_contact": "dev@example.com",
        "geocoder_url": "http://127.0.0.1:8080/search",
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
        provider="fake",
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


def test_ui_rank_formula_weights() -> None:
    guess = _guess(model_confidence=1.0, clues=[])
    result = ReconcileResult(
        pin_source=PinSource.geocoder,
        pin_lat=PORTO_LAT,
        pin_lng=PORTO_LNG,
        model_agree=True,
        geocode=None,
        photo_status=PhotoStatus.proposed,
        skipped_geocode=None,
    )
    rank = ui_rank(guess, result, [], [])
    expected = 0.45 * 1.0 + 0.20 * 1.0 + 0.15 * 0.0 + 0.10 * 1.0 + 0.10 * 0.0
    assert rank == pytest.approx(expected)
    assert granularity_score(Granularity.street) == 0.85
    assert granularity_score(Granularity.unknown) == 0.0
    assert clue_score(_guess(clues=[{"kind": "sign", "text": "x", "weight": 1.0}])) == pytest.approx(
        0.40
    )
    assert geocode_score(
        ReconcileResult(
            pin_source=PinSource.geocoder,
            pin_lat=0.0,
            pin_lng=0.0,
            model_agree=False,
            geocode=None,
            photo_status=PhotoStatus.proposed,
            skipped_geocode=None,
        )
    ) == pytest.approx(0.6)
    assert cluster_score(_guess(), [], []) == 0.0


def test_refuse_low_rank_and_weak_granularity() -> None:
    low = _guess(model_confidence=0.0, granularity=Granularity.unknown, clues=[])
    result = ReconcileResult(
        pin_source=PinSource.none,
        pin_lat=None,
        pin_lng=None,
        model_agree=None,
        geocode=None,
        photo_status=PhotoStatus.proposed,
        skipped_geocode="miss",
    )
    rank = ui_rank(low, result, [], [])
    assert rank < 0.25
    assert refuse_guess_reason(low, album_hint=None, rank=rank) == "weak_granularity"
    country = _guess(
        granularity=Granularity.country,
        clues=[{"kind": "sign", "text": "welcome", "weight": 1.0}],
        model_confidence=0.9,
    )
    assert refuse_guess_reason(country, album_hint=None, rank=0.5) is None
    assert hint_names_city("weekend in Porto, 2019") is True
    assert hint_names_city("weekend trip") is False


def test_estimate_usd_includes_reasoning_tokens() -> None:
    usage = Usage(input_tokens=1_000_000, output_tokens=0, reasoning_tokens=1_000_000)
    assert estimate_usd(usage) == pytest.approx(2.0 + 6.0)


def test_dry_run_plans_calls_without_vision_or_geocode(
    tmp_path: Path, data_dir: Path
) -> None:
    folder = tmp_path / "album"
    summary_ingest = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    summary = run(
        run_propose(
            summary_ingest.job_id,
            settings=_settings(data_dir),
            dry_run=True,
            vision=vision,
            geocoder=geocoder,
        )
    )
    assert summary.planned == 1
    assert summary.planned_relpaths == ["plain.jpg"]
    assert vision.calls == []
    assert geocoder.queries == []
    async def inspect() -> None:
        async with await JobRepo.open(data_dir, summary_ingest.job_id) as repo:
            photos = await repo.list_photos()
            assert photos[0]["status"] == PhotoStatus.pending.value
            assert await repo.get_current_proposal(photos[0]["id"]) is None

    run(inspect())
    assert not (job_dir(data_dir, summary_ingest.job_id) / "previews").exists()


def test_propose_writes_proposal_not_gps(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir, hint="weekend in Porto")
    source_bytes = (folder / "plain.jpg").read_bytes()
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.proposed == 1
    assert summary.refused == 0
    assert summary.error == 0
    assert summary.est_usd > 0
    assert len(vision.calls) == 1
    assert geocoder.queries == [PORTO]
    assert (folder / "plain.jpg").read_bytes() == source_bytes

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photos = await repo.list_photos()
            photo = photos[0]
            assert photo["status"] == PhotoStatus.proposed.value
            proposal = await repo.get_current_proposal(photo["id"])
            assert proposal is not None
            assert proposal["place_name"] == PORTO
            assert proposal["pin_source"] == PinSource.geocoder.value
            assert proposal["pin_lat"] == pytest.approx(PORTO_LAT)
            assert proposal["pin_lng"] == pytest.approx(PORTO_LNG)
            assert proposal["reasoning_tokens"] == 10
            assert proposal["prompt_version"] == "PROMPT_V1"
            assert proposal["ui_rank"] is not None
            assert float(proposal["ui_rank"]) >= 0.25
            receipts = await repo._db.execute("SELECT COUNT(*) AS n FROM write_receipt")
            row = await receipts.fetchone()
            assert row is not None
            assert int(row["n"]) == 0
            job = await repo.get_job()
            assert job is not None
            stats = json.loads(job["stats_json"])
            assert stats["propose"]["reasoning_tokens"] == 10
            assert stats["propose"]["counts"]["proposed"] == 1
            log_path = job_dir(data_dir, ingested.job_id) / "logs" / "propose.log"
            log_text = log_path.read_text(encoding="utf-8")
            assert "vision_ok" in log_text
            assert "data:image" not in log_text
            assert str(folder) not in log_text

    run(inspect())
    preview = job_dir(data_dir, ingested.job_id) / "previews"
    assert any(preview.glob("*.jpg"))


def test_propose_stats_accumulate_and_survive_a_noop_run(
    tmp_path: Path, data_dir: Path
) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())

    def propose(**overrides: Any) -> Any:
        return run(
            run_propose(
                ingested.job_id,
                settings=_settings(data_dir),
                vision=vision,
                geocoder=geocoder,
                concurrency=1,
                sleep=_noop_sleep,
                **overrides,
            )
        )

    def stats() -> dict[str, Any]:
        async def read() -> dict[str, Any]:
            async with await JobRepo.open(data_dir, ingested.job_id) as repo:
                job = await repo.get_job()
                assert job is not None
                block = json.loads(job["stats_json"])["propose"]
                assert isinstance(block, dict)
                return block

        return run(read())

    first = propose()
    assert first.proposed == 1
    spent = stats()
    assert spent["reasoning_tokens"] == 10
    assert spent["est_usd"] > 0
    assert spent["runs"] == 1

    # Nothing left to propose: the run must not zero what the first one spent.
    noop = propose()
    assert noop.planned == 0
    after_noop = stats()
    assert after_noop["input_tokens"] == spent["input_tokens"]
    assert after_noop["reasoning_tokens"] == spent["reasoning_tokens"]
    assert after_noop["est_usd"] == spent["est_usd"]
    assert after_noop["latency_ms_p50"] == spent["latency_ms_p50"]
    assert after_noop["counts"]["proposed"] == 1
    assert after_noop["runs"] == 2
    assert after_noop["last_run"]["counts"]["planned"] == 0
    assert after_noop["last_run"]["reasoning_tokens"] == 0
    assert after_noop["last_run"]["latency_ms_p50"] is None

    # A second run that really spends adds to the total instead of replacing it.
    forced = propose(force=True)
    assert forced.proposed == 1
    after_force = stats()
    assert after_force["reasoning_tokens"] == 2 * spent["reasoning_tokens"]
    assert after_force["est_usd"] == pytest.approx(2 * spent["est_usd"])
    assert after_force["counts"]["proposed"] == 2
    assert after_force["last_run"]["counts"]["proposed"] == 1
    assert after_force["last_run"]["est_usd"] == pytest.approx(spent["est_usd"])
    assert after_force["runs"] == 3


def test_privacy_sensitive_never_geocodes(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    guess = _guess(
        refusal=RefusalReason.privacy_sensitive,
        primary_place_name="12 Oak Street",
        granularity=Granularity.street,
    )
    vision = FakeVisionProvider(guess_result=guess)
    geocoder = FakeGeocoder()
    geocoder.add("12 Oak Street", _porto_hit())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.refused == 1
    assert geocoder.queries == []

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            assert photo["status"] == PhotoStatus.refused.value
            proposal = await repo.get_current_proposal(photo["id"])
            assert proposal is not None
            assert proposal["pin_source"] == PinSource.none.value
            assert proposal["pin_lat"] is None

    run(inspect())


def test_skips_current_proposal_unless_force(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert len(vision.calls) == 1
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert len(vision.calls) == 1
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            force=True,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert len(vision.calls) == 2


def test_resets_stale_proposing(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)

    async def mark() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            await repo.set_photo_status(photo["id"], PhotoStatus.proposing)

    run(mark())
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.proposed == 1


def test_job_lock_blocks_second_propose(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    lock_path = job_dir(data_dir, ingested.job_id) / "propose.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path.write_text(str(os.getpid()), encoding="utf-8")
    vision = FakeVisionProvider(guess_result=_guess())
    with pytest.raises(ProposeError, match="lock"):
        run(
            run_propose(
                ingested.job_id,
                settings=_settings(data_dir),
                vision=vision,
                geocoder=FakeGeocoder(),
            )
        )
    assert vision.calls == []
    lock_path.unlink()


def test_job_exclusive_lock_roundtrip(tmp_path: Path) -> None:
    path = tmp_path / "propose.lock"
    first = JobExclusiveLock(path)
    first.acquire()
    second = JobExclusiveLock(path)
    with pytest.raises(ProposeError, match="lock"):
        second.acquire()
    first.release()
    second.acquire()
    second.release()
    assert not path.exists()


def test_sha256_change_is_error_not_sent(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    write_test_jpeg(
        folder / "plain.jpg",
        datetime_original="2019:06:14 18:04:00",
        color=(9, 9, 9),
    )
    vision = FakeVisionProvider(guess_result=_guess())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=FakeGeocoder(),
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.error == 1
    assert vision.calls == []


def test_no_context_strips_hint_and_siblings(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir, hint="weekend in Porto")
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            no_context=True,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert len(vision.calls) == 1
    _jpeg, context, _options = vision.calls[0]
    assert context.no_context is True
    assert context.hint is None
    assert context.folder_basename == ""
    assert context.proposed_siblings == []
    text = format_user_message(context)
    assert "Porto" not in text
    assert "weekend" not in text
    assert "Task: infer where this photo was taken." in text


def test_limit_caps_photos(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    folder.mkdir()
    write_test_jpeg(folder / "a.jpg", datetime_original="2019:06:14 18:00:00")
    write_test_jpeg(folder / "b.jpg", datetime_original="2019:06:14 18:01:00")
    ingested = run(run_ingest(folder, data_dir=data_dir))
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            limit=1,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.planned == 1
    assert len(vision.calls) == 1


def test_retryable_then_success(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(
        guess_result=_guess(), retryable_failures_remaining=2
    )
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    sleeps: list[float] = []

    async def record_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=record_sleep,
        )
    )
    assert summary.proposed == 1
    assert len(vision.calls) == 3
    assert sleeps == [2.0, 8.0]


def test_retryable_exhausted_marks_error(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(
        guess_result=_guess(), retryable_failures_remaining=8
    )
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=FakeGeocoder(),
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.error == 1
    assert len(vision.calls) == 4

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            assert photo["status"] == PhotoStatus.error.value
            assert await repo.get_current_proposal(photo["id"]) is None

    run(inspect())


def test_content_block_is_refused_not_error(tmp_path: Path, data_dir: Path) -> None:
    """A safety block is terminal, so it lands in ``refused``/``policy_refusal``."""
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess(), blocked_on={"plain.jpg"})
    geocoder = FakeGeocoder()
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert (summary.refused, summary.error) == (1, 0)
    assert len(vision.calls) == 1
    assert geocoder.queries == []

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            assert photo["status"] == PhotoStatus.refused.value
            assert "FAKE_BLOCK" in str(photo["error"])
            proposal = await repo.get_current_proposal(photo["id"])
            assert proposal is not None
            assert proposal["refusal"] == RefusalReason.policy_refusal.value
            assert proposal["pin_source"] == PinSource.none.value
            assert (proposal["pin_lat"], proposal["pin_lng"]) == (None, None)
            assert proposal["input_tokens"] == 0
            guess = VisionGuess.model_validate_json(str(proposal["raw_json"]))
            assert guess.refusal is RefusalReason.policy_refusal
            assert "FAKE_BLOCK" in str(guess.refusal_detail)
            job = await repo.get_job()
            stats = json.loads(str(job["stats_json"]))["propose"]
            assert stats["refusal_histogram"]["policy_refusal"] == 1

    run(inspect())


def test_blocked_photo_is_not_re_sent_without_force(
    tmp_path: Path, data_dir: Path
) -> None:
    """The whole point: a second propose must not re-bill a call that cannot succeed."""
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess(), blocked_on={"plain.jpg"})
    kwargs: dict[str, Any] = {
        "settings": _settings(data_dir),
        "vision": vision,
        "geocoder": FakeGeocoder(),
        "concurrency": 1,
        "sleep": _noop_sleep,
    }
    run(run_propose(ingested.job_id, **kwargs))
    assert len(vision.calls) == 1

    again = run(run_propose(ingested.job_id, **kwargs))
    assert again.planned == 0
    assert len(vision.calls) == 1

    forced = run(run_propose(ingested.job_id, force=True, **kwargs))
    assert forced.planned == 1
    assert len(vision.calls) == 2


def test_empty_parse_stays_error(tmp_path: Path, data_dir: Path) -> None:
    """Only safety blocks are terminal; a plain vision error still retries."""
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess(), error_on={"plain.jpg"})
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=FakeGeocoder(),
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert (summary.error, summary.refused) == (1, 0)

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            assert photo["status"] == PhotoStatus.error.value

    run(inspect())


def test_missing_api_key_exits_2(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    settings = _settings(data_dir, xai_api_key=None)
    with pytest.raises(ProposeError, match="XAI_API_KEY"):
        run(run_propose(ingested.job_id, settings=settings, dry_run=False))


def test_missing_nominatim_contact_exits_2(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("NOMINATIM_CONTACT", raising=False)
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    settings = load_settings(
        _env_file=None,
        data_dir=data_dir,
        xai_api_key="k",
        nominatim_contact=None,
        geocoder_url="https://nominatim.openstreetmap.org/search",
    )
    with pytest.raises(ProposeError, match="nominatim_contact"):
        run(run_propose(ingested.job_id, settings=settings, dry_run=True))


def test_gemini_provider_needs_its_own_key(tmp_path: Path, data_dir: Path) -> None:
    """--provider gemini is accepted (PR 10) but never borrows XAI_API_KEY."""
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    with pytest.raises(ProposeError, match="GEMINI_API_KEY is required"):
        run(
            run_propose(
                ingested.job_id,
                settings=_settings(data_dir, gemini_api_key=None),
                provider_name="gemini",
            )
        )


def test_unknown_provider_rejected(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    with pytest.raises(ProposeError, match="--provider must be one of"):
        run(
            run_propose(
                ingested.job_id,
                settings=_settings(data_dir),
                provider_name="openai",
            )
        )


def test_cli_propose_help_and_dry_run(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    help_result = runner.invoke(app, ["propose", "--help"])
    assert help_result.exit_code == 0
    help_out = _plain(help_result.stdout)
    assert "--dry-run" in help_out
    assert "--no-context" in help_out
    assert "--force" in help_out
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    monkeypatch.setenv("NOMINATIM_CONTACT", "dev@example.com")
    monkeypatch.setenv("GEOCODER_URL", "http://127.0.0.1:8080/search")
    monkeypatch.setenv("XAI_API_KEY", "test-key")
    result = runner.invoke(app, ["propose", ingested.job_id, "--dry-run"])
    assert result.exit_code == 0
    combined = _plain(f"{result.stdout}{result.stderr}")
    assert "vision calls planned" in combined


def test_cli_gemini_requires_a_gemini_key(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    monkeypatch.setenv("NOMINATIM_CONTACT", "dev@example.com")
    monkeypatch.setenv("GEOCODER_URL", "http://127.0.0.1:8080/search")
    monkeypatch.setenv("XAI_API_KEY", "test-key")
    monkeypatch.delenv("GEMINI_API_KEY", raising=False)
    result = runner.invoke(app, ["propose", ingested.job_id, "--provider", "gemini"])
    assert result.exit_code == 2
    assert "gemini_api_key is required" in _plain(f"{result.stdout}{result.stderr}").lower()


def test_house_address_not_geocoded(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    guess = _guess(
        primary_place_name="12 Oak Street, Portland",
        granularity=Granularity.street,
        model_confidence=0.9,
        clues=[{"kind": "sign", "text": "oak", "weight": 1.0}],
    )
    vision = FakeVisionProvider(guess_result=guess)
    geocoder = FakeGeocoder()
    geocoder.add("12 Oak Street, Portland", _porto_hit())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert geocoder.queries == []
    assert summary.proposed + summary.refused == 1

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, ingested.job_id) as repo:
            photo = (await repo.list_photos())[0]
            proposal = await repo.get_current_proposal(photo["id"])
            assert proposal is not None
            assert proposal["pin_source"] == PinSource.none.value

    run(inspect())


def test_candidates_are_not_geocoded(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    vision = FakeVisionProvider(guess_result=_guess())
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())
    run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert geocoder.queries == [PORTO]


def test_geocode_cache_hit_skips_provider(
    tmp_path: Path, data_dir: Path
) -> None:
    folder = tmp_path / "album"
    ingested = _ingest_plain(folder, data_dir)
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _porto_hit())

    async def seed_cache() -> None:
        async with GeocodeCache(data_dir / "cache" / "geocode.sqlite") as cache:
            service = GeocodeService(geocoder, cache, cap_uncached=False)
            await service.lookup_primary(PORTO, "PT", Granularity.exact)

    run(seed_cache())
    assert geocoder.queries == [PORTO]
    vision = FakeVisionProvider(guess_result=_guess())
    summary = run(
        run_propose(
            ingested.job_id,
            settings=_settings(data_dir),
            vision=vision,
            geocoder=geocoder,
            concurrency=1,
            sleep=_noop_sleep,
        )
    )
    assert summary.proposed == 1
    assert geocoder.queries == [PORTO]
    assert summary.cache_hits == 1
