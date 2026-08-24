"""Time + folder clustering (K9). Invoked from ingest.

1. Partition by parent directory (``relpath`` dirname).
2. Inside a directory:
   - **dated:** rows with a parseable capture time, sorted by that timestamp.
     Start a new cluster when the gap to the previous dated photo is **> 45
     minutes**.
   - **undated:** rows with neither ``DateTimeOriginal`` nor ``CreateDate``.
     They form **one undated cluster per directory** and are not mixed into
     dated clusters. File mtime is never used as a capture time.
3. Cluster id = ``blake2s(parent + first_path + first_ts_or_"undated")``.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path

CLUSTER_GAP = timedelta(minutes=45)

_DT_FORMATS = (
    "%Y:%m:%d %H:%M:%S",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
)


@dataclass(frozen=True)
class ClusterMember:
    photo_id: str
    relpath: str
    datetime_original: str | None


@dataclass(frozen=True)
class ClusterPlan:
    cluster_id: str
    parent: str
    first_ts: str | None
    undated: bool
    members: tuple[ClusterMember, ...]

    @property
    def n(self) -> int:
        return len(self.members)


def parent_of(relpath: str) -> str:
    """Directory of ``relpath`` in posix form. Album root is ``.``."""
    posix = relpath.replace("\\", "/")
    parent = Path(posix).parent.as_posix()
    return parent if parent != "" else "."


def parse_capture_time(value: str | None) -> datetime | None:
    """Parse EXIF capture time. Does not accept file mtime."""
    if value is None:
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z") or text.endswith("z"):
        text = text[:-1]
    if len(text) >= 6 and text[-6] in "+-" and text[-3] == ":":
        text = text[:-6]
    elif len(text) >= 5 and text[-5] in "+-" and text[-4:].isdigit():
        text = text[:-5]
    core = text[:19]
    for fmt in _DT_FORMATS:
        try:
            return datetime.strptime(core, fmt)
        except ValueError:
            continue
    return None


def cluster_id_for(parent: str, first_path: str, first_ts: str | None) -> str:
    payload = f"{parent}{first_path}{first_ts if first_ts is not None else 'undated'}"
    return hashlib.blake2s(payload.encode("utf-8")).hexdigest()


def plan_clusters(members: list[ClusterMember]) -> list[ClusterPlan]:
    """Group photos into dated (45-minute gap) and undated per-directory clusters."""
    by_parent: dict[str, list[ClusterMember]] = {}
    for member in members:
        by_parent.setdefault(parent_of(member.relpath), []).append(member)

    plans: list[ClusterPlan] = []
    for parent in sorted(by_parent):
        dated: list[ClusterMember] = []
        undated: list[ClusterMember] = []
        for member in by_parent[parent]:
            if parse_capture_time(member.datetime_original) is None:
                undated.append(member)
            else:
                dated.append(member)
        dated.sort(
            key=lambda item: (
                parse_capture_time(item.datetime_original) or datetime.min,
                item.relpath,
            )
        )
        undated.sort(key=lambda item: item.relpath)
        plans.extend(_dated_clusters(parent, dated))
        undated_plan = _undated_cluster(parent, undated)
        if undated_plan is not None:
            plans.append(undated_plan)
    return plans


def _dated_clusters(parent: str, dated: list[ClusterMember]) -> list[ClusterPlan]:
    if not dated:
        return []
    groups: list[list[ClusterMember]] = []
    current: list[ClusterMember] = [dated[0]]
    for member in dated[1:]:
        previous_time = parse_capture_time(current[-1].datetime_original)
        this_time = parse_capture_time(member.datetime_original)
        if previous_time is None or this_time is None:
            groups.append(current)
            current = [member]
            continue
        if this_time - previous_time > CLUSTER_GAP:
            groups.append(current)
            current = [member]
        else:
            current.append(member)
    groups.append(current)
    return [_plan(parent, group, undated=False) for group in groups]


def _undated_cluster(parent: str, undated: list[ClusterMember]) -> ClusterPlan | None:
    if not undated:
        return None
    return _plan(parent, undated, undated=True)


def _plan(parent: str, members: list[ClusterMember], *, undated: bool) -> ClusterPlan:
    first = members[0]
    first_ts = None if undated else first.datetime_original
    return ClusterPlan(
        cluster_id=cluster_id_for(parent, first.relpath, first_ts),
        parent=parent,
        first_ts=first_ts,
        undated=undated,
        members=tuple(members),
    )
