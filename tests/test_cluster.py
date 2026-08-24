from __future__ import annotations

from datetime import datetime, timedelta

from gpsify.cluster import (
    CLUSTER_GAP,
    ClusterMember,
    cluster_id_for,
    parent_of,
    parse_capture_time,
    plan_clusters,
)


def _member(name: str, captured: str | None, photo_id: str | None = None) -> ClusterMember:
    return ClusterMember(
        photo_id=photo_id or name,
        relpath=name,
        datetime_original=captured,
    )


def test_parent_of_root_and_nested() -> None:
    assert parent_of("IMG_1.jpg") == "."
    assert parent_of("day1/IMG_1.jpg") == "day1"
    assert parent_of("a/b/c.jpg") == "a/b"


def test_parse_capture_time_exif_and_iso() -> None:
    assert parse_capture_time("2019:06:14 18:02:00") == datetime(2019, 6, 14, 18, 2, 0)
    assert parse_capture_time("2019-06-14T18:02:00") == datetime(2019, 6, 14, 18, 2, 0)
    assert parse_capture_time("2019-06-14T18:02:00Z") == datetime(2019, 6, 14, 18, 2, 0)
    assert parse_capture_time(None) is None
    assert parse_capture_time("not-a-date") is None


def test_45_minute_gap_splits_dated_clusters() -> None:
    t0 = "2019:06:14 18:00:00"
    t_equal_gap = (
        datetime(2019, 6, 14, 18, 0, 0) + CLUSTER_GAP
    ).strftime("%Y:%m:%d %H:%M:%S")
    t_over = (
        datetime(2019, 6, 14, 18, 0, 0) + CLUSTER_GAP + timedelta(seconds=1)
    ).strftime("%Y:%m:%d %H:%M:%S")
    # Exactly 45 minutes from the previous dated photo stays in the cluster.
    equal = plan_clusters([_member("a.jpg", t0), _member("b.jpg", t_equal_gap)])
    assert len(equal) == 1
    assert [m.relpath for m in equal[0].members] == ["a.jpg", "b.jpg"]
    # One second past 45 minutes starts a new cluster.
    split = plan_clusters([_member("a.jpg", t0), _member("c.jpg", t_over)])
    assert len(split) == 2
    assert [m.relpath for m in split[0].members] == ["a.jpg"]
    assert [m.relpath for m in split[1].members] == ["c.jpg"]


def test_undated_bucket_per_directory_not_mixed_with_dated() -> None:
    members = [
        _member("day1/dated.jpg", "2019:06:14 18:00:00"),
        _member("day1/undated_b.jpg", None),
        _member("day1/undated_a.jpg", None),
        _member("day2/undated.jpg", None),
    ]
    plans = plan_clusters(members)
    by_key = {(plan.parent, plan.undated): plan for plan in plans}
    assert set(by_key) == {("day1", False), ("day1", True), ("day2", True)}
    undated_day1 = by_key[("day1", True)]
    assert [m.relpath for m in undated_day1.members] == [
        "day1/undated_a.jpg",
        "day1/undated_b.jpg",
    ]
    assert undated_day1.first_ts is None
    assert by_key[("day1", False)].members[0].relpath == "day1/dated.jpg"


def test_mtime_is_not_used_as_capture_time() -> None:
    """Callers must not pass mtime as datetime_original; None stays one undated cluster."""
    members = [
        ClusterMember(photo_id="early", relpath="early.jpg", datetime_original=None),
        ClusterMember(photo_id="late", relpath="late.jpg", datetime_original=None),
    ]
    plans = plan_clusters(members)
    assert len(plans) == 1
    assert plans[0].undated is True
    assert plans[0].n == 2


def test_cluster_id_is_blake2s_of_parent_first_path_and_ts() -> None:
    members = [
        _member("porto/IMG_1041.jpg", "2019:06:14 18:02:00"),
        _member("porto/IMG_1042.jpg", "2019:06:14 18:04:00"),
    ]
    plans = plan_clusters(members)
    assert len(plans) == 1
    expected = cluster_id_for("porto", "porto/IMG_1041.jpg", "2019:06:14 18:02:00")
    assert plans[0].cluster_id == expected
    undated = plan_clusters([_member("x.jpg", None)])
    assert undated[0].cluster_id == cluster_id_for(".", "x.jpg", None)
