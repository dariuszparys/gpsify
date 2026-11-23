use gpsify::status::{
    format_job_list, format_status, job_status, list_job_entries, run_clean, StatusError,
    PNG_XMP_ONLY_WARNING,
};
use gpsify::store::{copies_root, job_dir, options_hash, JobIndex, JobRepo, NewPhoto};
use gpsify::types::PhotoStatus;
use tempfile::TempDir;

const JOB_ID: &str = "99999999-9999-4999-8999-999999999999";
const JOB_B: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn create_job(data_dir: &std::path::Path, job_id: &str, root: &str) -> JobRepo {
    let index = JobIndex::open(data_dir).unwrap();
    index
        .register(job_id, root, &options_hash(false), None)
        .unwrap();
    JobRepo::create(
        data_dir,
        job_id,
        root,
        root,
        &options_hash(false),
        Some("weekend in Porto"),
        "{}",
        None,
    )
    .unwrap()
}

fn add_photo(repo: &JobRepo, relpath: &str, status: PhotoStatus) {
    let mut p = NewPhoto::pending(format!("/photos/trip/{relpath}"), relpath, "a".repeat(64));
    p.size = 1024;
    p.mtime_ns = 1;
    p.status = status;
    repo.insert_photo(p).unwrap();
}

fn seed_job(data_dir: &std::path::Path) {
    let repo = create_job(data_dir, JOB_ID, "/photos/trip");
    add_photo(&repo, "a.jpg", PhotoStatus::Proposed);
    add_photo(&repo, "b.jpg", PhotoStatus::Accepted);
    add_photo(&repo, "c.png", PhotoStatus::Pending);
    repo.set_stats_json(
        r#"{
            "propose": {
                "counts": {"proposed": 1, "refused": 0, "error": 0},
                "est_usd": 0.009,
                "input_tokens": 2100,
                "output_tokens": 520,
                "reasoning_tokens": 800
            }
        }"#,
    )
    .unwrap();
}

#[test]
fn status_rollup_reads_stats_and_live_counts() {
    let tmp = TempDir::new().unwrap();
    seed_job(tmp.path());

    let rollup = job_status(tmp.path(), JOB_ID).unwrap();
    assert_eq!(rollup.job_id, JOB_ID);
    assert_eq!(
        rollup.dest_root,
        copies_root(tmp.path(), JOB_ID).to_string_lossy()
    );
    assert_eq!(rollup.counts[PhotoStatus::Proposed.as_str()], 1);
    assert_eq!(rollup.counts[PhotoStatus::Accepted.as_str()], 1);
    assert_eq!(rollup.counts[PhotoStatus::Pending.as_str()], 1);
    assert_eq!(rollup.counts[PhotoStatus::Written.as_str()], 0);
    assert_eq!(rollup.stats["propose"]["est_usd"], 0.009);
    assert_eq!(rollup.stats["propose"]["reasoning_tokens"], 800);
    assert!(rollup.warnings.contains(&PNG_XMP_ONLY_WARNING.to_string()));

    let human = format_status(&rollup);
    assert!(human.contains(JOB_ID));
    assert!(human.contains("est_usd: 0.009"));
    assert!(human.contains("reasoning_tokens: 800"));
    assert!(human.contains("proposed=1"));
}

#[test]
fn list_jobs_and_empty_index() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        format_job_list(&list_job_entries(tmp.path()).unwrap()),
        "no jobs"
    );

    seed_job(tmp.path());
    create_job(tmp.path(), JOB_B, "/photos/other");
    let entries = list_job_entries(tmp.path()).unwrap();
    assert_eq!(entries.len(), 2);
    let ids: std::collections::BTreeSet<_> = entries.iter().map(|e| e.job_id.as_str()).collect();
    assert!(ids.contains(JOB_ID));
    assert!(ids.contains(JOB_B));
}

#[test]
fn status_unknown_job_errors() {
    let tmp = TempDir::new().unwrap();
    let err = job_status(tmp.path(), "missing-job").unwrap_err();
    assert!(matches!(err, StatusError(_)));
}

#[test]
fn clean_deletes_previews_and_thumbs_keeps_sqlite_and_backups() {
    let tmp = TempDir::new().unwrap();
    seed_job(tmp.path());
    let root = job_dir(tmp.path(), JOB_ID);
    let previews = root.join("previews");
    let thumbs = root.join("thumbs");
    let backups = root.join("backups");
    let copies = root.join("copies");
    let logs = root.join("logs");
    for d in [&previews, &thumbs, &backups, &copies, &logs] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(previews.join("photo.jpg"), b"preview").unwrap();
    std::fs::write(thumbs.join("photo.jpg"), b"thumb").unwrap();
    std::fs::write(backups.join("photo.bin"), b"backup").unwrap();
    std::fs::write(copies.join("photo.jpg"), b"copy").unwrap();
    std::fs::write(logs.join("propose.log"), b"log\n").unwrap();

    let sqlite = root.join("job.sqlite");
    let sqlite_bytes = std::fs::read(&sqlite).unwrap();

    let summary = run_clean(tmp.path(), JOB_ID).unwrap();
    assert!(summary.deleted_previews);
    assert!(summary.deleted_thumbs);
    assert!(summary.kept_sqlite);
    assert!(summary.kept_backups);
    assert!(!previews.exists());
    assert!(!thumbs.exists());
    assert_eq!(std::fs::read(&sqlite).unwrap(), sqlite_bytes);
    assert_eq!(std::fs::read(backups.join("photo.bin")).unwrap(), b"backup");
    assert_eq!(std::fs::read(copies.join("photo.jpg")).unwrap(), b"copy");

    let second = run_clean(tmp.path(), JOB_ID).unwrap();
    assert!(!second.deleted_previews);
    assert!(!second.deleted_thumbs);
    assert!(second.kept_sqlite);
}
