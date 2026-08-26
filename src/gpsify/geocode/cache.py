"""SQLite geocode cache keyed by normalize(place)|country|lang."""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Self

import aiosqlite

from gpsify.store.db import utc_now_iso
from gpsify.types import GeoHit

CACHE_DB_NAME = "geocode.sqlite"

_WHITESPACE = re.compile(r"\s+")


def geocode_cache_path(data_dir: Path) -> Path:
    return data_dir / "cache" / CACHE_DB_NAME


def normalize_place(place: str) -> str:
    """NFKC, strip, lower, punctuation→space except comma, collapse whitespace."""
    text = unicodedata.normalize("NFKC", place).strip().lower()
    chars: list[str] = []
    for char in text:
        if char == ",":
            chars.append(char)
        elif unicodedata.category(char).startswith("P"):
            chars.append(" ")
        else:
            chars.append(char)
    return _WHITESPACE.sub(" ", "".join(chars)).strip()


def cache_key(place: str, country_code: str | None, lang: str) -> str:
    cc = (country_code or "").strip().lower()
    return f"{normalize_place(place)}|{cc}|{lang.strip().lower()}"


@dataclass(frozen=True)
class CacheLookup:
    """``found`` distinguishes a cached miss (hit is None) from a cold key."""

    found: bool
    hit: GeoHit | None


CACHE_SCHEMA = """
CREATE TABLE IF NOT EXISTS geocode_cache (
  cache_key TEXT PRIMARY KEY,
  query TEXT NOT NULL,
  hit_json TEXT,
  created_at TEXT NOT NULL
);
"""


class GeocodeCache:
    """Unique-string cache at ``~/.gpsify/cache/geocode.sqlite``."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._db: aiosqlite.Connection | None = None

    async def open(self) -> Self:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        db = await aiosqlite.connect(self.path, isolation_level=None)
        db.row_factory = aiosqlite.Row
        await db.execute("PRAGMA journal_mode = WAL")
        await db.execute("PRAGMA busy_timeout = 5000")
        await db.executescript(CACHE_SCHEMA)
        self._db = db
        return self

    async def close(self) -> None:
        if self._db is not None:
            await self._db.close()
            self._db = None

    async def __aenter__(self) -> Self:
        return await self.open()

    async def __aexit__(
        self,
        exc_type: object,
        exc: object,
        tb: object,
    ) -> None:
        del exc_type, exc, tb
        await self.close()

    def _require_db(self) -> aiosqlite.Connection:
        if self._db is None:
            raise RuntimeError("GeocodeCache is closed")
        return self._db

    async def get(self, key: str) -> CacheLookup:
        db = self._require_db()
        cursor = await db.execute(
            "SELECT hit_json FROM geocode_cache WHERE cache_key = ?",
            (key,),
        )
        row = await cursor.fetchone()
        if row is None:
            return CacheLookup(found=False, hit=None)
        raw = row["hit_json"]
        if raw is None:
            return CacheLookup(found=True, hit=None)
        return CacheLookup(found=True, hit=GeoHit.model_validate_json(raw))

    async def put(self, key: str, hit: GeoHit | None, *, query: str) -> None:
        db = self._require_db()
        payload = None if hit is None else hit.model_dump_json()
        await db.execute(
            """
            INSERT OR REPLACE INTO geocode_cache (cache_key, query, hit_json, created_at)
            VALUES (?, ?, ?, ?)
            """,
            (key, query, payload, utc_now_iso()),
        )
