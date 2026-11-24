use chrono::NaiveDateTime;
use gpsify::cluster::{
    blake2s_256, cluster_id_for, parent_of, parse_capture_time, plan_clusters, ClusterMember,
};

fn member(name: &str, captured: Option<&str>) -> ClusterMember {
    ClusterMember {
        photo_id: name.to_string(),
        relpath: name.to_string(),
        datetime_original: captured.map(str::to_string),
    }
}

#[test]
fn blake2s_matches_rfc7693_vectors() {
    assert_eq!(
        hex::encode(blake2s_256(b"")),
        "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"
    );
    assert_eq!(
        hex::encode(blake2s_256(b"abc")),
        "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
    );
}

#[test]
fn parent_of_root_and_nested() {
    assert_eq!(parent_of("IMG_1.jpg"), ".");
    assert_eq!(parent_of("day1/IMG_1.jpg"), "day1");
    assert_eq!(parent_of("a/b/c.jpg"), "a/b");
}

#[test]
fn parse_capture_time_exif_and_iso() {
    let expected =
        NaiveDateTime::parse_from_str("2019-06-14 18:02:00", "%Y-%m-%d %H:%M:%S").unwrap();
    assert_eq!(
        parse_capture_time(Some("2019:06:14 18:02:00")),
        Some(expected)
    );
    assert_eq!(
        parse_capture_time(Some("2019-06-14T18:02:00")),
        Some(expected)
    );
    assert_eq!(
        parse_capture_time(Some("2019-06-14T18:02:00Z")),
        Some(expected)
    );
    assert_eq!(
        parse_capture_time(Some("2019-06-14T18:02:00+02:00")),
        Some(expected)
    );
    assert_eq!(parse_capture_time(None), None);
    assert_eq!(parse_capture_time(Some("not-a-date")), None);
}

#[test]
fn forty_five_minute_gap_splits_dated_clusters() {
    let t0 = "2019:06:14 18:00:00";
    let t_eq = "2019:06:14 18:45:00";
    let t_over = "2019:06:14 18:45:01";

    let equal = plan_clusters(&[member("a.jpg", Some(t0)), member("b.jpg", Some(t_eq))]);
    assert_eq!(equal.len(), 1);
    assert_eq!(
        equal[0]
            .members
            .iter()
            .map(|m| m.relpath.as_str())
            .collect::<Vec<_>>(),
        vec!["a.jpg", "b.jpg"]
    );

    let split = plan_clusters(&[member("a.jpg", Some(t0)), member("c.jpg", Some(t_over))]);
    assert_eq!(split.len(), 2);
    assert_eq!(split[0].members[0].relpath, "a.jpg");
    assert_eq!(split[1].members[0].relpath, "c.jpg");
}

#[test]
fn undated_bucket_per_directory_not_mixed_with_dated() {
    let members = vec![
        member("day1/dated.jpg", Some("2019:06:14 18:00:00")),
        member("day1/undated_b.jpg", None),
        member("day1/undated_a.jpg", None),
        member("day2/undated.jpg", None),
    ];
    let plans = plan_clusters(&members);
    assert_eq!(plans.len(), 3);
    let undated_day1 = plans
        .iter()
        .find(|p| p.parent == "day1" && p.undated)
        .unwrap();
    assert_eq!(
        undated_day1
            .members
            .iter()
            .map(|m| m.relpath.as_str())
            .collect::<Vec<_>>(),
        vec!["day1/undated_a.jpg", "day1/undated_b.jpg"]
    );
    assert_eq!(undated_day1.first_ts, None);
}

#[test]
fn cluster_id_is_blake2s_of_parent_first_path_and_ts() {
    let plans = plan_clusters(&[
        member("porto/IMG_1041.jpg", Some("2019:06:14 18:02:00")),
        member("porto/IMG_1042.jpg", Some("2019:06:14 18:04:00")),
    ]);
    assert_eq!(plans.len(), 1);
    let expected = cluster_id_for("porto", "porto/IMG_1041.jpg", Some("2019:06:14 18:02:00"));
    assert_eq!(plans[0].cluster_id, expected);

    let undated = plan_clusters(&[member("x.jpg", None)]);
    assert_eq!(undated[0].cluster_id, cluster_id_for(".", "x.jpg", None));
}
