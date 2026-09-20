"""Real Chromium + local Starlette integration; no external network or paid calls."""
from __future__ import annotations

import asyncio
import socket
import threading
import time

import pytest
import uvicorn
from PIL import Image
from playwright.sync_api import expect, sync_playwright

from gpsify.config import Settings
from gpsify.ingest import run_ingest
from gpsify.review import create_app
from gpsify.store import JobRepo
from gpsify.types import PhotoStatus


@pytest.fixture
def review(tmp_path):
    folder = tmp_path / "album"
    folder.mkdir()
    for i in range(3):
        Image.new("RGB", (80, 80), (i * 70, 90, 150)).save(folder / f"{i}.jpg")
    data = tmp_path / "data"

    async def seed():
        job = await run_ingest(folder, data_dir=data)
        repo = await JobRepo.open(data, job.job_id)
        try:
            for i, photo in enumerate(await repo.list_photos()):
                await repo.insert_proposal(
                    photo_id=photo["id"], provider="fake", model="fake", prompt_version="test",
                    raw_json="{}", place_name=f"Place {i}", pin_lat=40 + i, pin_lng=10 + i,
                    pin_source="geocoder", ui_rank=0.9,
                )
                await repo.set_photo_status(photo["id"], PhotoStatus.proposed)
        finally:
            await repo.close()
        return job.job_id

    job_id = asyncio.run(seed())
    settings = Settings(_env_file=None, data_dir=data, geocoder_url="http://127.0.0.1:1/search",
                        map_tile_url="/tiles/{z}/{x}/{y}.png")
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    server = uvicorn.Server(uvicorn.Config(create_app(settings=settings, job_id=job_id), log_level="error"))
    thread = threading.Thread(target=lambda: server.run(sockets=[sock]), daemon=True)
    thread.start()
    try:
        deadline = time.monotonic() + 10
        while not server.started:
            if time.monotonic() > deadline:
                raise RuntimeError("review server did not start")
            time.sleep(0.01)
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 1000})
            page.route("**/*", lambda route: route.continue_() if route.request.url.startswith(f"http://127.0.0.1:{port}/") else route.abort())
            page.route("**/tiles/**", lambda route: route.fulfill(status=204))
            page.goto(f"http://127.0.0.1:{port}/")
            expect(page.locator(".tile")).to_have_count(3)
            yield page
            browser.close()
    finally:
        server.should_exit = True
        thread.join(timeout=10)
        sock.close()


def test_selection_bulk_accept_preserves_each_pin(review):
    page = review
    page.locator(".tile").first.click()
    page.locator(".tile").last.click(modifiers=["Shift"])
    expect(page.locator(".tile.selected")).to_have_count(3)
    page.locator("#btn-accept").click()
    expect(page.locator(".tile")).to_have_count(0)
    page.locator('[data-filter="accepted"]').click()
    expect(page.locator(".tile")).to_have_count(3)
    records = page.request.get(page.url + "api/photos?status=accepted").json()["photos"]
    assert [p["decision"]["lat"] for p in records] == [40, 41, 42]


def test_edit_pin_persists_decision(review):
    page = review
    page.locator(".tile").first.click()
    page.locator("#btn-edit").click()
    page.locator("#map").click(position={"x": 160, "y": 140})
    page.locator("#btn-edit").click()
    expect(page.locator(".tile")).to_have_count(2)
    records = page.request.get(page.url + "api/photos?status=accepted").json()["photos"]
    assert records[0]["decision"]["action"] == "accept_edited"
    assert records[0]["decision"]["lat"] != 40


def test_undated_pin_copy_requires_correct_count(review):
    page = review
    page.locator(".tile").first.click()
    page.locator(".tile").last.click(modifiers=["Shift"])
    page.locator("#btn-apply-pin").click()
    expect(page.locator("#apply-confirm")).to_be_visible()
    page.locator("#apply-confirm").fill("1")
    page.locator("#apply-ok").click()
    expect(page.locator("#status-msg")).to_contain_text("requires confirm_n")
    records = page.request.get(page.url + "api/photos?status=accepted").json()["photos"]
    assert len(records) == 1  # Source accepted, targets unchanged.
    page.locator('[data-filter="all"]').click()
    expect(page.locator(".tile")).to_have_count(3)
    page.locator(".tile").first.click()
    page.locator(".tile").last.click(modifiers=["Shift"])
    page.locator("#btn-apply-pin").click()
    page.locator("#apply-confirm").fill("2")
    page.locator("#apply-ok").click()
    expect(page.locator("#status-msg")).to_contain_text("Pin applied to 2 photos")
    records = page.request.get(page.url + "api/photos?status=accepted").json()["photos"]
    assert len(records) == 3
    assert {p["decision"]["lat"] for p in records} == {42}
