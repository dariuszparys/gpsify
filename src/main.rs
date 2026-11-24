//! gpsify CLI: thin layer over each stage's `run_*` entrypoint.
//!
//! Exit codes:
//! - 0: success / `--version`
//! - 1: runtime / dependency / partial-photo failure
//! - 2: CLI usage, configuration, store, or lock error
//! - 3: `apply` found zero accepted photos (refused)

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use gpsify::apply::{run_apply, ApplyOptions};
use gpsify::config::load_settings;
use gpsify::exif::exiftool_available;
use gpsify::geocode::{require_geocoder_ready, DEFAULT_UNCACHED_MAX};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::media::heic_available;
use gpsify::propose::{
    count_unpinned_proposals, run_geocode, run_propose, GeocodeOptions, ProposeOptions,
    DEFAULT_CONCURRENCY,
};
use gpsify::review::{
    bind_review_listener, build_review_state_for_job, create_router, list_low_rank_photos,
    REVIEW_HOST,
};
use gpsify::status::{format_job_list, format_status, job_status, list_job_entries, run_clean};
use gpsify::store::{resolve_job_handle, JobRepo};
use gpsify::types::DecisionAction;
use gpsify::undo::run_undo;
use gpsify::VERSION;

#[derive(Parser, Debug)]
#[command(
    name = "gpsify",
    version = VERSION,
    about = "Propose GPS from photo pixels; write tags only after a human accepts. See docs/decisions.md.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check local photo dependencies and configuration without network calls.
    Doctor,

    /// Ingest, propose, and open review in one command.
    Run {
        /// Photo folder to ingest, propose, and review.
        folder: PathBuf,
        /// Album / trip hint sent with vision calls.
        #[arg(long)]
        hint: Option<String>,
        /// Walk subdirectories.
        #[arg(long)]
        recursive: bool,
        /// Max photos to propose this run.
        #[arg(long)]
        limit: Option<usize>,
        /// Parallel vision calls.
        #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
        /// Estimated USD stopping threshold; serializes calls, may overshoot by one call.
        #[arg(long)]
        max_cost: Option<f64>,
        /// Vision provider: xai | gemini. Defaults to GPSIFY_VISION_PROVIDER.
        #[arg(long)]
        provider: Option<String>,
        /// Public Nominatim uncached lookup cap for this run.
        #[arg(long, default_value_t = DEFAULT_UNCACHED_MAX)]
        geocode_uncached_max: usize,
        /// Loopback port (binds 127.0.0.1 only).
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Plan only: no vision spend, no geocoder HTTP, no review.
        #[arg(long)]
        dry_run: bool,
        /// Stop after propose instead of serving the review UI.
        #[arg(long)]
        no_review: bool,
    },

    /// Ingest a folder; print job_id and counts (total / tagged / untagged / skipped).
    Ingest {
        /// Photo folder to ingest.
        folder: PathBuf,
        /// Album / trip hint sent with vision calls.
        #[arg(long)]
        hint: Option<String>,
        /// Walk subdirectories.
        #[arg(long)]
        recursive: bool,
        /// Rebind an existing job_id after a folder move.
        #[arg(long, value_name = "JOB_ID")]
        relocate: Option<String>,
    },

    /// Call vision + geocode; write proposals. Does not write GPS.
    Propose {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Max photos to propose this run.
        #[arg(long)]
        limit: Option<usize>,
        /// Parallel vision calls.
        #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
        /// Estimated USD stopping threshold; serializes calls, may overshoot by one call.
        #[arg(long)]
        max_cost: Option<f64>,
        /// No network; print planned calls and cache hits.
        #[arg(long)]
        dry_run: bool,
        /// Re-send photos that already have a current proposal.
        #[arg(long)]
        force: bool,
        /// Pixels only: strip hint, folder, filename, and siblings.
        #[arg(long)]
        no_context: bool,
        /// Vision provider: xai | gemini. Defaults to GPSIFY_VISION_PROVIDER.
        #[arg(long)]
        provider: Option<String>,
        /// Public Nominatim uncached lookup cap for this run.
        #[arg(long, default_value_t = DEFAULT_UNCACHED_MAX)]
        geocode_uncached_max: usize,
    },

    /// Geocode proposals left at pin_source=none. No vision call, no vision spend.
    Geocode {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Max unpinned proposals to geocode this run.
        #[arg(long)]
        limit: Option<usize>,
        /// No network; count the place names that still need an uncached lookup.
        #[arg(long)]
        dry_run: bool,
        /// Public Nominatim uncached lookup cap for this run.
        #[arg(long, default_value_t = DEFAULT_UNCACHED_MAX)]
        geocode_uncached_max: usize,
    },

    /// Open the localhost review UI (loopback only).
    Review {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Loopback port (binds 127.0.0.1 only).
        #[arg(long, default_value_t = 8765)]
        port: u16,
    },

    /// Test/script seam over add_decision. Not the review UX.
    Decide {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Photo id to decide.
        #[arg(long)]
        photo: String,
        /// accept | skip | accept_edited
        #[arg(long)]
        action: String,
        /// Required for accept_edited.
        #[arg(long)]
        lat: Option<f64>,
        /// Required for accept_edited.
        #[arg(long)]
        lng: Option<f64>,
    },

    /// Write GPS from the current decision only. Default dest is the job copies/ tree.
    Apply {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Write copies under DIR/<relpath>.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Mutate sources (or RAW sidecars).
        #[arg(long)]
        in_place: bool,
        /// Print would-be writes; mutate nothing.
        #[arg(long)]
        dry_run: bool,
        /// Confirm --in-place without a TTY prompt.
        #[arg(long)]
        i_mean_it: bool,
    },

    /// Restore the backup taken before exiftool (or delete/restore a RAW sidecar).
    Undo {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Limit undo to one source path.
        #[arg(long)]
        photo: Option<PathBuf>,
    },

    /// Preview (and with --yes, write) skip decisions for low UI-rank photos.
    Skip {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
        /// Preview low UI-rank photos.
        #[arg(long)]
        all_low: bool,
        /// Write skip decisions without a TTY confirm.
        #[arg(long)]
        yes: bool,
    },

    /// Print job counts, dest root, and cost counters.
    Status {
        /// Job UUID, folder path, or 'last'. Omit to list jobs.
        job_id: Option<String>,
        /// Machine-readable rollup.
        #[arg(long = "json")]
        json_output: bool,
    },

    /// Delete previews and thumbs; keep sqlite and backups.
    Clean {
        /// Job UUID, folder path, or 'last'.
        job_id: String,
    },
}

fn exit_err(code: u8, msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(code)
}

fn resolve_job(data_dir: &Path, handle: &str) -> Result<String, ExitCode> {
    match resolve_job_handle(data_dir, handle) {
        Ok(resolved) => {
            if let Some(note) = resolved.note {
                eprintln!("{note}");
            }
            Ok(resolved.job_id)
        }
        Err(err) => Err(exit_err(2, &err.to_string())),
    }
}

fn confirm_tty(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

async fn serve_review_ui(settings: gpsify::config::Settings, job_id: &str, port: u16) -> ExitCode {
    let state = match build_review_state_for_job(settings, job_id) {
        Ok(s) => s,
        Err(err) => return exit_err(2, &err.to_string()),
    };
    let listener = match bind_review_listener(port).await {
        Ok(l) => l,
        Err(err) => return exit_err(1, &format!("failed to bind {REVIEW_HOST}:{port}: {err}")),
    };
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let url = format!(
        "http://{REVIEW_HOST}:{actual_port}/?token={}",
        state.auth_token
    );
    println!("{url}  (loopback only; no LAN bind)");
    let _ = io::stdout().flush();
    if std::env::var_os("GPSIFY_NO_BROWSER").is_none() {
        let _ = open::that(&url);
    }
    let router = create_router(state);
    if let Err(err) = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    {
        return exit_err(1, &err.to_string());
    }
    ExitCode::from(0)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let settings = match load_settings() {
        Ok(s) => s,
        Err(err) => return exit_err(2, &err.to_string()),
    };

    match cli.command {
        Command::Doctor => {
            let tool = exiftool_available();
            let heic = heic_available();
            println!(
                "exiftool: {}",
                if tool {
                    "available"
                } else {
                    "missing — install exiftool (macOS: brew install exiftool)"
                }
            );
            println!(
                "HEIC previews: {}",
                if heic {
                    "available"
                } else {
                    "missing — build with --features heic"
                }
            );
            let key = if settings.vision_provider == "gemini" {
                settings.gemini_api_key.as_deref()
            } else {
                settings.xai_api_key.as_deref()
            };
            let has_key = key.is_some_and(|k| !k.trim().is_empty());
            println!(
                "{} API key: {}",
                settings.vision_provider,
                if has_key { "configured" } else { "missing" }
            );
            match require_geocoder_ready(&settings) {
                Ok(()) => println!("geocoder: configured"),
                Err(err) => {
                    println!("geocoder: {err}");
                    return ExitCode::from(1);
                }
            }
            if !tool || !heic || !has_key {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }

        Command::Run {
            folder,
            hint,
            recursive,
            limit,
            concurrency,
            max_cost,
            provider,
            geocode_uncached_max,
            port,
            dry_run,
            no_review,
        } => {
            let mut ingest_opts = IngestOptions::new(&settings.data_dir);
            ingest_opts.hint = hint.as_deref();
            ingest_opts.recursive = recursive;
            let ingested = match run_ingest(&folder, ingest_opts) {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            for warning in &ingested.warnings {
                eprintln!("warning: {warning}");
            }
            let clusters_word = if ingested.clusters == 1 {
                "cluster"
            } else {
                "clusters"
            };
            println!(
                "scanning    {} photos  ({} already tagged, {} to place, {} {clusters_word})",
                ingested.total, ingested.tagged, ingested.untagged, ingested.clusters
            );

            let job_id = ingested.job_id.clone();
            let mut prop_opts = ProposeOptions::new(settings.clone());
            prop_opts.limit = limit;
            prop_opts.concurrency = concurrency;
            prop_opts.max_cost = max_cost;
            prop_opts.dry_run = dry_run;
            prop_opts.provider_name = provider;
            prop_opts.geocode_uncached_max = geocode_uncached_max;
            prop_opts.progress = Some(Arc::new(|msg| println!("proposing   {msg}")));

            let proposed = match run_propose(&job_id, prop_opts).await {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            for warning in &proposed.warnings {
                eprintln!("warning: {warning}");
            }

            if dry_run {
                println!("dry-run: nothing spent, nothing written. job {job_id}");
                return ExitCode::from(u8::from(proposed.error > 0));
            }

            if let Ok(repo) = JobRepo::open(&settings.data_dir, &job_id) {
                if let Ok(unpinned) = count_unpinned_proposals(&repo) {
                    if unpinned > 0 {
                        println!(
                            "note        {unpinned} proposals still have no pin. Fill them without vision spend:  gpsify geocode {}",
                            folder.display()
                        );
                    }
                }
            }

            if no_review {
                println!(
                    "job {job_id}. Review with:  gpsify review {}",
                    folder.display()
                );
                return ExitCode::from(u8::from(proposed.error > 0));
            }

            println!(
                "when you're done reviewing:  gpsify apply {}",
                folder.display()
            );
            print!("review      ");
            let _ = io::stdout().flush();
            serve_review_ui(settings, &job_id, port).await
        }

        Command::Ingest {
            folder,
            hint,
            recursive,
            relocate,
        } => {
            let mut opts = IngestOptions::new(&settings.data_dir);
            opts.hint = hint.as_deref();
            opts.recursive = recursive;
            opts.relocate = relocate.as_deref();
            let summary = match run_ingest(&folder, opts) {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            println!("{}", summary.job_id);
            println!(
                "total: {}  tagged: {}  untagged: {}  skipped: {}  missing: {}  clusters: {}",
                summary.total,
                summary.tagged,
                summary.untagged,
                summary.skipped,
                summary.missing,
                summary.clusters
            );
            if !summary.other.is_empty() {
                let parts: Vec<String> = summary
                    .other
                    .iter()
                    .map(|(name, n)| format!("{name}={n}"))
                    .collect();
                println!("other: {}", parts.join("  "));
            }
            for warning in &summary.warnings {
                eprintln!("warning: {warning}");
            }
            ExitCode::from(0)
        }

        Command::Propose {
            job_id,
            limit,
            concurrency,
            max_cost,
            dry_run,
            force,
            no_context,
            provider,
            geocode_uncached_max,
        } => {
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let mut opts = ProposeOptions::new(settings);
            opts.limit = limit;
            opts.concurrency = concurrency;
            opts.max_cost = max_cost;
            opts.dry_run = dry_run;
            opts.force = force;
            opts.no_context = no_context;
            opts.provider_name = provider;
            opts.geocode_uncached_max = geocode_uncached_max;
            opts.progress = Some(Arc::new(|msg| println!("{msg}")));
            let summary = match run_propose(&resolved_id, opts).await {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            for warning in &summary.warnings {
                eprintln!("warning: {warning}");
            }
            ExitCode::from(u8::from(summary.error > 0))
        }

        Command::Geocode {
            job_id,
            limit,
            dry_run,
            geocode_uncached_max,
        } => {
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let mut opts = GeocodeOptions::new(settings);
            opts.limit = limit;
            opts.dry_run = dry_run;
            opts.geocode_uncached_max = geocode_uncached_max;
            opts.progress = Some(Arc::new(|msg| println!("{msg}")));
            let summary = match run_geocode(&resolved_id, opts).await {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            for warning in &summary.warnings {
                eprintln!("warning: {warning}");
            }
            ExitCode::from(0)
        }

        Command::Review { job_id, port } => {
            if let Err(err) = require_geocoder_ready(&settings) {
                return exit_err(2, &err.to_string());
            }
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            serve_review_ui(settings, &resolved_id, port).await
        }

        Command::Decide {
            job_id,
            photo,
            action,
            lat,
            lng,
        } => {
            let action_enum = match DecisionAction::from_str(&action) {
                Ok(
                    a @ (DecisionAction::Accept
                    | DecisionAction::Skip
                    | DecisionAction::AcceptEdited),
                ) => a,
                Ok(DecisionAction::ClusterApply) => {
                    return exit_err(
                        2,
                        "cluster_apply is the review API; use accept/skip/accept_edited",
                    );
                }
                Err(_) => return exit_err(2, "--action must be accept, skip, or accept_edited"),
            };
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let repo = match JobRepo::open(&settings.data_dir, &resolved_id) {
                Ok(r) => r,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            match repo.add_decision(&photo, action_enum, lat, lng, None, None) {
                Ok(decision_id) => {
                    println!("{decision_id}");
                    println!("{}", action_enum.as_str());
                    ExitCode::from(0)
                }
                Err(err) => exit_err(2, &err.to_string()),
            }
        }

        Command::Apply {
            job_id,
            out,
            in_place,
            dry_run,
            mut i_mean_it,
        } => {
            if out.is_some() && in_place {
                return exit_err(2, "--out and --in-place are mutually exclusive");
            }
            if in_place && !dry_run && !i_mean_it {
                if !io::stdin().is_terminal() {
                    return exit_err(2, "pass --i-mean-it to apply --in-place (not a TTY)");
                }
                if !confirm_tty("Mutate originals in place?") {
                    return ExitCode::from(1);
                }
                i_mean_it = true;
            }
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let opts = ApplyOptions {
                out,
                in_place,
                dry_run,
                i_mean_it: i_mean_it || dry_run,
                allow_writes_override: None,
            };
            let summary = match run_apply(
                &resolved_id,
                &settings.data_dir,
                opts,
                Some(|m: &str| println!("{m}")),
            ) {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            if summary.dry_run {
                return ExitCode::from(0);
            }
            if summary.eligible == 0 {
                eprintln!("nothing accepted to apply");
                return ExitCode::from(3);
            }
            ExitCode::from(u8::from(summary.failed > 0))
        }

        Command::Undo { job_id, photo } => {
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let summary = match run_undo(
                &resolved_id,
                &settings.data_dir,
                photo.as_deref(),
                Some(|m: &str| println!("{m}")),
            ) {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            ExitCode::from(u8::from(summary.failed > 0))
        }

        Command::Skip {
            job_id,
            all_low,
            yes,
        } => {
            if !all_low {
                return exit_err(2, "--all-low is required");
            }
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let repo = match JobRepo::open(&settings.data_dir, &resolved_id) {
                Ok(r) => r,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            let rows = match list_low_rank_photos(&repo) {
                Ok(r) => r,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            if rows.is_empty() {
                println!("no low-rank proposed photos");
                return ExitCode::from(0);
            }
            println!("low-rank photos (ui_rank < 0.55): {}", rows.len());
            for row in &rows {
                let place = row.place_name.as_deref().unwrap_or("");
                println!(
                    "  {}  {}  {:.2}  {}",
                    row.id, row.relpath, row.ui_rank, place
                );
            }
            if !yes {
                if !io::stdin().is_terminal() {
                    return exit_err(2, "pass --yes to write skip decisions (not a TTY)");
                }
                if !confirm_tty(&format!("Write skip decisions for {} photos?", rows.len())) {
                    return ExitCode::from(1);
                }
            }
            for row in &rows {
                if let Err(err) =
                    repo.add_decision(&row.id, DecisionAction::Skip, None, None, None, None)
                {
                    return exit_err(2, &err.to_string());
                }
            }
            println!("skipped {} photos", rows.len());
            ExitCode::from(0)
        }

        Command::Status {
            job_id,
            json_output,
        } => {
            if let Some(handle) = job_id {
                let resolved_id = match resolve_job(&settings.data_dir, &handle) {
                    Ok(id) => id,
                    Err(code) => return code,
                };
                let rollup = match job_status(&settings.data_dir, &resolved_id) {
                    Ok(r) => r,
                    Err(err) => return exit_err(2, &err.to_string()),
                };
                if json_output {
                    match serde_json::to_string(&rollup) {
                        Ok(s) => println!("{s}"),
                        Err(err) => return exit_err(2, &err.to_string()),
                    }
                } else {
                    println!("{}", format_status(&rollup));
                }
                for warning in &rollup.warnings {
                    eprintln!("warning: {warning}");
                }
                ExitCode::from(0)
            } else {
                let entries = match list_job_entries(&settings.data_dir) {
                    Ok(e) => e,
                    Err(err) => return exit_err(2, &err.to_string()),
                };
                if json_output {
                    let payload = serde_json::json!({ "jobs": entries });
                    match serde_json::to_string(&payload) {
                        Ok(s) => println!("{s}"),
                        Err(err) => return exit_err(2, &err.to_string()),
                    }
                } else {
                    println!("{}", format_job_list(&entries));
                }
                ExitCode::from(0)
            }
        }

        Command::Clean { job_id } => {
            let resolved_id = match resolve_job(&settings.data_dir, &job_id) {
                Ok(id) => id,
                Err(code) => return code,
            };
            let summary = match run_clean(&settings.data_dir, &resolved_id) {
                Ok(s) => s,
                Err(err) => return exit_err(2, &err.to_string()),
            };
            let mut deleted = Vec::new();
            if summary.deleted_previews {
                deleted.push("previews");
            }
            if summary.deleted_thumbs {
                deleted.push("thumbs");
            }
            if deleted.is_empty() {
                println!("nothing to clean");
            } else {
                println!("deleted {}", deleted.join(" and "));
            }
            println!("kept sqlite and backups");
            ExitCode::from(0)
        }
    }
}
