use std::collections::BTreeSet;

use gpsify::store::{
    get_user_version, index_db_path, job_db_path, open_index_db, open_job_db, options_hash,
    resolve_job_handle, JobIndex, JobRepo, NewPhoto, NewProposal, StoreError, UpsertWriteReceipt,
    SCHEMA_VERSION,
};
use gpsify::types::{DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const JOB_ID: &str = "11111111-1111-4111-8111-111111111111";

fn create_repo(data_dir: &std::path::Path) -> JobRepo {
    JobRepo::create(
        data_dir,
        JOB_ID,
        "/photos/trip",
        "/photos/trip",
        &options_hash(false),
        Some("weekend in Porto"),
        "{}",
        None,
    )
    .unwrap()
}

fn pending_photo(repo: &JobRepo, suffix: &str) -> String {
    let mut new = NewPhoto::pending(
        format!("/photos/trip/IMG_{suffix}.jpg"),
        format!("IMG_{suffix}.jpg"),
        "a".repeat(64),
    );
    new.size = 1024;
    new.mtime_ns = 1;
    repo.insert_photo(new).unwrap()
}

fn add_test_proposal(
    repo: &JobRepo,
    photo_id: &str,
    pin_source: PinSource,
    pin_lat: Option<f64>,
    pin_lng: Option<f64>,
    place_name: Option<&str>,
) -> String {
    repo.insert_proposal(NewProposal {
        photo_id: photo_id.to_string(),
        provider: "xai".to_string(),
        model: "grok-4.6".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: "{}".to_string(),
        place_name: place_name.map(str::to_string),
        granularity: Some("exact".to_string()),
        model_confidence: Some(0.8),
        ui_rank: Some(0.83),
        refusal: Some("none".to_string()),
        pin_lat,
        pin_lng,
        pin_source: Some(pin_source),
        input_tokens: Some(2100),
        output_tokens: Some(520),
        reasoning_tokens: Some(800),
        latency_ms: Some(6120),
        current: true,
        ..Default::default()
    })
    .unwrap()
}

fn table_names(conn: &rusqlite::Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn column_names(conn: &rusqlite::Connection, table: &str) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn options_hash_is_canonical_recursive_json_only() {
    let expected_false = hex::encode(Sha256::digest(br#"{"recursive":false}"#));
    let expected_true = hex::encode(Sha256::digest(br#"{"recursive":true}"#));
    assert_eq!(options_hash(false), expected_false);
    assert_eq!(options_hash(true), expected_true);
    assert_ne!(options_hash(false), options_hash(true));
}

#[test]
fn job_schema_v1_complete() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    repo.with_conn(|db| {
        assert_eq!(get_user_version(db)?, SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 1);

        let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        let fk: i64 = db.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        assert_eq!(fk, 1);

        let tables = table_names(db);
        for expected in [
            "job",
            "cluster",
            "photo",
            "proposal",
            "decision",
            "write_receipt",
        ] {
            assert!(tables.contains(expected), "missing table {expected}");
        }

        let photo_cols = column_names(db, "photo");
        for col in [
            "id",
            "job_id",
            "path",
            "relpath",
            "sha256",
            "size",
            "mtime_ns",
            "status",
            "error",
            "datetime_original",
            "make",
            "model",
            "has_gps",
            "existing_lat",
            "existing_lng",
            "cluster_id",
            "updated_at",
        ] {
            assert!(photo_cols.contains(col), "missing photo column {col}");
        }

        let prop_cols = column_names(db, "proposal");
        for col in [
            "id",
            "photo_id",
            "created_at",
            "current",
            "provider",
            "model",
            "prompt_version",
            "raw_json",
            "place_name",
            "granularity",
            "model_confidence",
            "ui_rank",
            "refusal",
            "pin_lat",
            "pin_lng",
            "pin_source",
            "model_agree",
            "geocode_json",
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "latency_ms",
        ] {
            assert!(prop_cols.contains(col), "missing proposal column {col}");
        }

        let receipt_cols = column_names(db, "write_receipt");
        for col in [
            "id",
            "photo_id",
            "decision_id",
            "dest_path",
            "in_place",
            "status",
            "error",
            "written_at",
            "written_sha256",
            "backup_path",
        ] {
            assert!(
                receipt_cols.contains(col),
                "missing write_receipt column {col}"
            );
        }

        let mut stmt =
            db.prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name != ''")?;
        let indexes: BTreeSet<String> = stmt
            .query_map([], |r| r.get(0))?
            .map(Result::unwrap)
            .collect();
        assert!(indexes.contains("proposal_one_current"));
        assert!(indexes.contains("decision_one_current"));
        Ok(())
    })
    .unwrap();

    let job = repo.get_job().unwrap().unwrap();
    assert_eq!(job.stats_json, "{}");
}

#[test]
fn unsupported_user_version_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let path = job_db_path(tmp.path(), "schema-mismatch");
    let db = open_job_db(&path).unwrap();
    db.execute_batch("PRAGMA user_version = 99").unwrap();
    drop(db);
    let err = open_job_db(&path).unwrap_err();
    assert!(matches!(err, StoreError::Schema(_)));
    assert!(err.to_string().contains("user_version=99"));
}

#[test]
fn foreign_keys_reject_orphan_proposal() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let err = repo
        .insert_proposal(NewProposal {
            photo_id: "missing-photo".to_string(),
            provider: "xai".to_string(),
            model: "grok-4.6".to_string(),
            prompt_version: "PROMPT_V1".to_string(),
            raw_json: "{}".to_string(),
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)));
}

#[test]
fn index_register_lookup_and_resume() {
    let tmp = TempDir::new().unwrap();
    let digest = options_hash(false);
    let index = JobIndex::open(tmp.path()).unwrap();
    index
        .with_conn(|db| {
            let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            assert_eq!(get_user_version(db)?, SCHEMA_VERSION);
            Ok(())
        })
        .unwrap();

    let first = index
        .register("job-a", "/photos/trip", &digest, None)
        .unwrap();
    let resumed = index
        .register("job-other-uuid", "/photos/trip", &digest, None)
        .unwrap();
    assert_eq!(resumed.job_id, first.job_id);
    assert_eq!(resumed.job_id, "job-a");

    let found = index.lookup("/photos/trip", &digest).unwrap().unwrap();
    assert_eq!(found.job_id, "job-a");

    index
        .touch_ingest("job-a", Some("2026-08-12T00:00:00Z"))
        .unwrap();
    let touched = index.get("job-a").unwrap().unwrap();
    assert_eq!(touched.last_ingest_at, "2026-08-12T00:00:00Z");

    let listed = index.list_jobs().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].job_id, "job-a");

    let relocated = index.relocate("job-a", "/photos/trip-moved", None).unwrap();
    assert_eq!(relocated.root_canonical, "/photos/trip-moved");
    assert!(index.lookup("/photos/trip", &digest).unwrap().is_none());
    assert!(index_db_path(tmp.path()).is_file());
}

#[test]
fn index_unique_job_id_conflict_and_relocate_collision() {
    let tmp = TempDir::new().unwrap();
    let digest = options_hash(false);
    let index = JobIndex::open(tmp.path()).unwrap();
    index.register("job-a", "/a", &digest, None).unwrap();
    let err = index.register("job-a", "/b", &digest, None).unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)));
    assert!(err.to_string().contains("already exists"));

    index.register("job-b", "/b", &digest, None).unwrap();
    let rel_err = index.relocate("job-b", "/a", None).unwrap_err();
    assert!(matches!(rel_err, StoreError::Conflict(_)));
    assert!(rel_err.to_string().contains("collide"));
}

#[test]
fn add_decision_contract() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let pid = pending_photo(&repo, "a");

    // skip sets status & null coords
    let did = repo
        .add_decision(&pid, DecisionAction::Skip, None, None, None, None)
        .unwrap();
    let photo = repo.get_photo(&pid).unwrap().unwrap();
    let decision = repo.get_decision(&did).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::SkippedByUser.as_str());
    assert_eq!(decision.action, DecisionAction::Skip.as_str());
    assert!(decision.lat.is_none());
    assert!(decision.lng.is_none());
    assert_eq!(decision.current, 1);

    let err = repo
        .add_decision(
            &pid,
            DecisionAction::Skip,
            Some(41.0),
            Some(-8.0),
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Decision(_)));
    assert!(err.to_string().contains("must not include coordinates"));

    // accept without proposal rejects
    let pid_no_prop = pending_photo(&repo, "b");
    let err = repo
        .add_decision(&pid_no_prop, DecisionAction::Accept, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("current proposal"));

    // accept with pin_source=none rejects
    add_test_proposal(&repo, &pid_no_prop, PinSource::None, None, None, None);
    let err = repo
        .add_decision(&pid_no_prop, DecisionAction::Accept, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("pin_source"));

    // accept with NaN pin rejects
    add_test_proposal(
        &repo,
        &pid_no_prop,
        PinSource::Geocoder,
        Some(f64::NAN),
        Some(-8.0),
        Some("Bad"),
    );
    let err = repo
        .add_decision(&pid_no_prop, DecisionAction::Accept, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"));

    // accept copies geocoder pin
    let prop_id = add_test_proposal(
        &repo,
        &pid,
        PinSource::Geocoder,
        Some(41.1456),
        Some(-8.6143),
        Some("Clérigos Church, Porto"),
    );
    let acc_id = repo
        .add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();
    let acc = repo.get_decision(&acc_id).unwrap().unwrap();
    let photo = repo.get_photo(&pid).unwrap().unwrap();
    assert!((acc.lat.unwrap() - 41.1456).abs() < 1e-9);
    assert!((acc.lng.unwrap() - (-8.6143)).abs() < 1e-9);
    assert_eq!(acc.place_name.as_deref(), Some("Clérigos Church, Porto"));
    assert_eq!(acc.proposal_id.as_deref(), Some(prop_id.as_str()));
    assert_eq!(photo.status, PhotoStatus::Accepted.as_str());

    // accept copies user pin
    let pid_u = pending_photo(&repo, "u");
    add_test_proposal(
        &repo,
        &pid_u,
        PinSource::User,
        Some(1.0),
        Some(2.0),
        Some("User Pin"),
    );
    let u_dec = repo
        .add_decision(&pid_u, DecisionAction::Accept, None, None, None, None)
        .unwrap();
    let dec_row = repo.get_decision(&u_dec).unwrap().unwrap();
    assert_eq!(dec_row.lat, Some(1.0));
    assert_eq!(dec_row.lng, Some(2.0));

    // accept_edited and cluster_apply require finite coords & last-write-wins
    let err = repo
        .add_decision(&pid, DecisionAction::AcceptEdited, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("accept_edited requires finite"));

    let err = repo
        .add_decision(&pid, DecisionAction::ClusterApply, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("cluster_apply requires finite"));

    let edited_id = repo
        .add_decision(
            &pid,
            DecisionAction::AcceptEdited,
            Some(41.15),
            Some(-8.61),
            None,
            Some("edited"),
        )
        .unwrap();
    let edited = repo.get_decision(&edited_id).unwrap().unwrap();
    assert_eq!(edited.lat, Some(41.15));
    assert_eq!(
        repo.get_photo(&pid).unwrap().unwrap().status,
        PhotoStatus::AcceptedEdited.as_str()
    );

    let cluster_id = repo
        .add_decision(
            &pid,
            DecisionAction::ClusterApply,
            Some(41.14),
            Some(-8.61),
            None,
            None,
        )
        .unwrap();
    let current = repo.get_current_decision(&pid).unwrap().unwrap();
    assert_eq!(current.id, cluster_id);
    assert_eq!(current.current, 1);
    assert_eq!(repo.get_decision(&edited_id).unwrap().unwrap().current, 0);

    // written photo rejects further decisions
    repo.set_photo_status(&pid, PhotoStatus::Written, None, None)
        .unwrap();
    let err = repo
        .add_decision(&pid, DecisionAction::Skip, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("written"));
}

#[test]
fn proposal_one_current_unique_index() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let pid = pending_photo(&repo, "a");
    let first = add_test_proposal(&repo, &pid, PinSource::Geocoder, Some(1.0), Some(2.0), None);
    let second = add_test_proposal(&repo, &pid, PinSource::Geocoder, Some(3.0), Some(4.0), None);
    let current = repo.get_current_proposal(&pid).unwrap().unwrap();
    assert_eq!(current.id, second);
    let old = repo.get_proposal(&first).unwrap().unwrap();
    assert_eq!(old.current, 0);

    let err = repo
        .with_conn(|db| {
            db.execute(
                "UPDATE proposal SET current = 1 WHERE id = ?",
                rusqlite::params![first],
            )?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)));
}

#[test]
fn write_receipt_upsert_includes_undone() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let pid = pending_photo(&repo, "a");
    add_test_proposal(&repo, &pid, PinSource::Geocoder, Some(1.0), Some(2.0), None);
    let did = repo
        .add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();
    let dest = "/tmp/copies/IMG_a.jpg";

    let first = repo
        .upsert_write_receipt(UpsertWriteReceipt {
            photo_id: pid.clone(),
            decision_id: did.clone(),
            dest_path: dest.to_string(),
            in_place: false,
            status: WriteReceiptStatus::Pending,
            error: None,
            written_at: None,
            written_sha256: None,
            backup_path: None,
            receipt_id: None,
        })
        .unwrap();

    let second = repo
        .upsert_write_receipt(UpsertWriteReceipt {
            photo_id: pid.clone(),
            decision_id: did.clone(),
            dest_path: dest.to_string(),
            in_place: false,
            status: WriteReceiptStatus::Ok,
            error: None,
            written_at: Some("2026-08-12T00:00:00Z".to_string()),
            written_sha256: Some("b".repeat(64)),
            backup_path: None,
            receipt_id: None,
        })
        .unwrap();

    let undone = repo
        .upsert_write_receipt(UpsertWriteReceipt {
            photo_id: pid.clone(),
            decision_id: did.clone(),
            dest_path: dest.to_string(),
            in_place: false,
            status: WriteReceiptStatus::Undone,
            error: None,
            written_at: None,
            written_sha256: None,
            backup_path: None,
            receipt_id: None,
        })
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(second, undone);
    let row = repo.get_write_receipt(&pid, dest).unwrap().unwrap();
    assert_eq!(row.status, WriteReceiptStatus::Undone.as_str());
}

#[test]
fn token_columns_cluster_and_stale_reset() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let cid = repo
        .insert_cluster(None, ".", Some("2019-06-14T18:02:00"), 2, false)
        .unwrap();
    let mut np = NewPhoto::pending("/photos/trip/IMG_x.jpg", "IMG_x.jpg", "c".repeat(64));
    np.size = 2048;
    np.mtime_ns = 2;
    np.status = PhotoStatus::Proposed;
    np.cluster_id = Some(cid.clone());
    let pid = repo.insert_photo(np).unwrap();
    let prop_id = add_test_proposal(&repo, &pid, PinSource::Geocoder, Some(1.0), Some(2.0), None);
    let prop = repo.get_proposal(&prop_id).unwrap().unwrap();
    let cluster = repo.get_cluster(&cid).unwrap().unwrap();
    assert_eq!(prop.input_tokens, Some(2100));
    assert_eq!(prop.output_tokens, Some(520));
    assert_eq!(prop.reasoning_tokens, Some(800));
    assert_eq!(cluster.n, 2);
    assert_eq!(cluster.undated, 0);

    assert_eq!(repo.reset_stale_proposing().unwrap(), 0);
    repo.set_photo_status(&pid, PhotoStatus::Proposing, None, None)
        .unwrap();
    assert_eq!(repo.reset_stale_proposing().unwrap(), 1);
    assert_eq!(
        repo.get_photo(&pid).unwrap().unwrap().status,
        PhotoStatus::Pending.as_str()
    );
    repo.set_stats_json(r#"{"proposed":1}"#).unwrap();
    assert_eq!(
        repo.get_job().unwrap().unwrap().stats_json,
        r#"{"proposed":1}"#
    );
}

#[test]
fn index_schema_and_resolve_job_handle() {
    let tmp = TempDir::new().unwrap();
    let db = open_index_db(&index_db_path(tmp.path())).unwrap();
    let tables = table_names(&db);
    assert!(tables.contains("job_index"));
    assert_eq!(
        column_names(&db, "job_index"),
        [
            "job_id",
            "root_canonical",
            "options_hash",
            "created_at",
            "last_ingest_at"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    drop(db);

    // UUID on disk without index row still resolves
    let orphan_id = "22222222-2222-4222-8222-222222222222";
    JobRepo::create(
        tmp.path(),
        orphan_id,
        "/tmp/orphan",
        "/tmp/orphan",
        &options_hash(false),
        None,
        "{}",
        None,
    )
    .unwrap();
    let resolved_orphan = resolve_job_handle(tmp.path(), orphan_id).unwrap();
    assert_eq!(resolved_orphan.job_id, orphan_id);

    // Folder with both flat and recursive picks newest with a note
    let album = tmp.path().join("album");
    std::fs::create_dir_all(&album).unwrap();
    let canon = std::fs::canonicalize(&album)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let index = JobIndex::open(tmp.path()).unwrap();
    let id_flat = "33333333-3333-4333-8333-333333333333";
    let id_rec = "44444444-4444-4444-8444-444444444444";
    index
        .register(
            id_flat,
            &canon,
            &options_hash(false),
            Some("2026-08-01T00:00:00Z"),
        )
        .unwrap();
    index
        .register(
            id_rec,
            &canon,
            &options_hash(true),
            Some("2026-08-02T00:00:00Z"),
        )
        .unwrap();

    let by_folder = resolve_job_handle(tmp.path(), album.to_str().unwrap()).unwrap();
    assert_eq!(by_folder.job_id, id_rec);
    assert!(by_folder.note.unwrap().contains("recursive"));

    let by_last = resolve_job_handle(tmp.path(), "last").unwrap();
    assert_eq!(by_last.job_id, id_rec);
}
