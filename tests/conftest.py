"""Unit tests never talk to the network or write live EXIF unless marked."""

from __future__ import annotations

import os

os.environ.setdefault("GPSIFY_DRY_RUN", "1")
