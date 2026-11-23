/* Desktop UI orchestration: album switcher, ingest, propose, geocode, apply, undo, doctor.
   Must remain free of any keystroke-based search listeners or autofill hints (K10). */

import { desktop, setMediaContext } from "./api.js";

const $ = (id) => document.getElementById(id);

const desk = {
  doctor: null,
  jobs: [],
  activeJobId: null,
  statusRollup: null,
  unpinned: 0,
  lowRankCount: 0,
  mediaBase: "gpsify://localhost",
  busy: false,
  hooks: null,
};

function installClosedByFallback() {
  if ("closedBy" in HTMLDialogElement.prototype) return;
  const outside = (e, dialog) => {
    const r = dialog.getBoundingClientRect();
    return e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom;
  };
  for (const dialog of document.querySelectorAll('dialog[closedby="any"]')) {
    let pressedOutside = false;
    dialog.addEventListener("pointerdown", (e) => {
      pressedOutside = outside(e, dialog);
    });
    dialog.addEventListener("pointerup", (e) => {
      if (pressedOutside && outside(e, dialog)) dialog.close();
    });
  }
}

function shortRoot(path) {
  const parts = String(path || "").split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path || "";
}

function setProgress(visible, stage, message) {
  const banner = $("desktop-progress");
  banner.classList.toggle("hidden", !visible);
  if (visible) {
    $("desktop-progress-stage").textContent = stage || "Working";
    $("desktop-progress-msg").textContent = message || "";
  }
}

function setBusy(isBusy, stage = "", message = "") {
  desk.busy = isBusy;
  setProgress(isBusy, stage, message);
  renderToolbar();
}

function renderToolbar() {
  const hasJob = Boolean(desk.activeJobId);
  const label = hasJob && desk.statusRollup ? shortRoot(desk.statusRollup.root_path) : "No album open";
  $("job-name-desktop").textContent = label;
  $("btn-jobs").title = hasJob && desk.statusRollup ? desk.statusRollup.root_path : "Open or switch album";

  $("btn-open-folder").disabled = desk.busy;
  $("btn-jobs").disabled = desk.busy;
  $("btn-doctor").disabled = desk.busy;

  $("btn-propose").disabled = desk.busy || !hasJob;
  const geocodeBtn = $("btn-geocode-pass");
  geocodeBtn.classList.toggle("hidden", !hasJob || desk.unpinned === 0);
  geocodeBtn.disabled = desk.busy || !hasJob;
  $("unpinned-count").textContent = String(desk.unpinned);

  const counts = (desk.statusRollup && desk.statusRollup.counts) || {};
  const accepted = (counts.accepted || 0) + (counts.accepted_edited || 0);
  const written = counts.written || 0;

  $("btn-apply-gps").disabled = desk.busy || !hasJob;
  $("btn-apply-gps").textContent = accepted > 0 ? `Apply (${accepted})` : "Apply";

  $("btn-undo-gps").disabled = desk.busy || !hasJob || written === 0;
  $("btn-undo-gps").classList.toggle("hidden", !hasJob || written === 0);
}

function renderWelcome() {
  const pane = $("grid-pane");
  pane.replaceChildren();
  const wrap = document.createElement("div");
  wrap.className = "desktop-welcome";

  const head = document.createElement("div");
  head.className = "welcome-head";
  const h1 = document.createElement("h1");
  h1.textContent = "Visual geolocation for your photo albums";
  const lead = document.createElement("p");
  lead.textContent =
    "Choose a photo folder to inspect EXIF & time clusters, propose locations with AI, review pins on the map, and write GPS tags only after you accept.";
  head.append(h1, lead);

  const actions = document.createElement("div");
  actions.className = "welcome-actions";
  const scanBtn = document.createElement("button");
  scanBtn.type = "button";
  scanBtn.className = "primary";
  scanBtn.textContent = "Choose Photo Folder…";
  scanBtn.addEventListener("click", () => openIngestDialogWithPicker());
  const statusBtn = document.createElement("button");
  statusBtn.type = "button";
  statusBtn.textContent = "Check System Status";
  statusBtn.addEventListener("click", () => openDoctorDialog());
  actions.append(scanBtn, statusBtn);

  wrap.append(head, actions);

  if (desk.doctor) {
    const readiness = document.createElement("div");
    readiness.className = "welcome-checks";
    const chips = [
      ["ExifTool", desk.doctor.exiftool],
      ["HEIC (ImageIO)", desk.doctor.heic],
      [`${desk.doctor.vision_provider.toUpperCase()} key`, desk.doctor.api_key_configured],
      ["Geocoder", desk.doctor.geocoder_ok],
    ];
    for (const [name, ok] of chips) {
      const chip = document.createElement("span");
      chip.className = `check-chip ${ok ? "ok" : "warn"}`;
      chip.textContent = `${ok ? "✓" : "!"} ${name}`;
      readiness.appendChild(chip);
    }
    wrap.appendChild(readiness);
  }

  if (desk.jobs.length) {
    const recents = document.createElement("div");
    recents.className = "welcome-recents";
    const sub = document.createElement("div");
    sub.className = "section-label";
    sub.textContent = "Recent albums";
    const list = document.createElement("ul");
    list.className = "jobs-list";
    for (const entry of desk.jobs.slice(0, 8)) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      const title = document.createElement("strong");
      title.textContent = shortRoot(entry.root_canonical);
      const subPath = document.createElement("span");
      subPath.textContent = entry.root_canonical;
      btn.append(title, subPath);
      btn.addEventListener("click", () => switchToJob(entry.job_id));
      li.appendChild(btn);
      list.appendChild(li);
    }
    recents.append(sub, list);
    wrap.appendChild(recents);
  }

  pane.appendChild(wrap);
}

async function applyOpenedState(opened) {
  if (!opened || !opened.job) return;
  desk.activeJobId = opened.job.id;
  desk.statusRollup = opened.status;
  desk.unpinned = opened.unpinned || 0;
  desk.lowRankCount = opened.low_rank_count || 0;
  desk.mediaBase = opened.media_base || desk.mediaBase;
  setMediaContext(desk.activeJobId, desk.mediaBase);
  renderToolbar();
  await desk.hooks.reloadJob(opened.job);
}

async function refreshActiveStatus() {
  if (!desk.activeJobId) return;
  const info = await desktop.status(desk.activeJobId);
  desk.statusRollup = info.status;
  desk.unpinned = info.unpinned || 0;
  desk.lowRankCount = info.low_rank_count || 0;
  renderToolbar();
}

async function switchToJob(handle) {
  try {
    const opened = await desktop.openJob(handle);
    await applyOpenedState(opened);
    const list = await desktop.listJobs();
    desk.jobs = list.jobs || [];
  } catch (err) {
    desk.hooks.reportError(err);
  }
}

async function openIngestDialogWithPicker(prefillFolder) {
  let folder = prefillFolder || "";
  if (!folder) {
    const picked = await desktop.pickFolder("Select a photo album folder");
    if (picked) folder = picked;
  }
  const dialog = $("ingest-dialog");
  $("ingest-folder").value = folder;
  $("ingest-hint").value = "";
  $("ingest-recursive").checked = false;
  $("ingest-error").classList.add("hidden");
  $("ingest-error").textContent = "";
  dialog.showModal();
}

async function openJobsDialog() {
  try {
    const list = await desktop.listJobs();
    desk.jobs = list.jobs || [];
  } catch (err) {
    desk.hooks.reportError(err);
    return;
  }
  const ul = $("jobs-list");
  ul.replaceChildren();
  if (!desk.jobs.length) {
    const li = document.createElement("li");
    li.className = "muted";
    li.textContent = "No scanned albums yet.";
    ul.appendChild(li);
  } else {
    for (const entry of desk.jobs) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      if (entry.job_id === desk.activeJobId) btn.classList.add("active");
      const title = document.createElement("strong");
      title.textContent = shortRoot(entry.root_canonical);
      const sub = document.createElement("span");
      sub.textContent = `${entry.root_canonical} · ${entry.last_ingest_at.slice(0, 10)}`;
      btn.append(title, sub);
      btn.addEventListener("click", () => {
        $("jobs-dialog").close();
        switchToJob(entry.job_id);
      });
      li.appendChild(btn);
      ul.appendChild(li);
    }
  }
  $("jobs-dialog").showModal();
}

function updateGeminiWarn() {
  const isGemini = $("prov-gemini").checked;
  $("propose-gemini-warn").classList.toggle("hidden", !isGemini);
}

function openProposeDialog() {
  if (!desk.activeJobId) return;
  const pref = (desk.doctor && desk.doctor.vision_provider) || "xai";
  $("prov-xai").checked = pref !== "gemini";
  $("prov-gemini").checked = pref === "gemini";
  updateGeminiWarn();
  $("propose-limit").value = "";
  $("propose-max-cost").value = "";
  $("propose-dry-run").checked = false;
  $("propose-force").checked = false;
  $("propose-no-context").checked = false;
  $("propose-error").classList.add("hidden");
  $("propose-error").textContent = "";
  $("propose-dialog").showModal();
}

function openApplyGpsDialog() {
  if (!desk.activeJobId || !desk.statusRollup) return;
  const counts = desk.statusRollup.counts || {};
  const accepted = (counts.accepted || 0) + (counts.accepted_edited || 0);
  const dest = desk.statusRollup.dest_root || "job copies/";
  $("apply-gps-summary").textContent =
    accepted > 0
      ? `${accepted} accepted photo${accepted === 1 ? "" : "s"} ready to write. Default destination: ${dest}`
      : "No accepted photos yet. Accept or edit pins before applying (Dry Run is still available).";
  $("apply-gps-out").value = "";
  $("apply-gps-in-place").checked = false;
  $("apply-gps-inplace-warn").classList.add("hidden");
  $("apply-gps-result").classList.add("hidden");
  $("apply-gps-error").classList.add("hidden");
  $("btn-apply-gps-submit").disabled = accepted === 0;
  $("apply-dialog-gps").showModal();
}

function openUndoDialog() {
  if (!desk.activeJobId) return;
  const counts = (desk.statusRollup && desk.statusRollup.counts) || {};
  const written = counts.written || 0;
  $("undo-body").textContent =
    `Restore backups for ${written} written photo${written === 1 ? "" : "s"} (or remove/restore RAW .xmp sidecars). Files edited after write are preserved.`;
  $("undo-error").classList.add("hidden");
  $("undo-dialog").showModal();
}

async function openDoctorDialog() {
  try {
    desk.doctor = await desktop.doctor();
    if (desk.activeJobId) await refreshActiveStatus();
  } catch (err) {
    desk.hooks.reportError(err);
    return;
  }
  const dl = $("doctor-dl");
  dl.replaceChildren();
  const rows = [
    ["ExifTool", desk.doctor.exiftool ? "Available" : "Missing — install with: brew install exiftool"],
    ["HEIC previews", desk.doctor.heic ? "Available (Apple ImageIO)" : "Missing"],
    ["Default provider", `${desk.doctor.vision_provider} (${desk.doctor.vision_provider === "gemini" ? desk.doctor.gemini_model : desk.doctor.model})`],
    ["xAI API key", desk.doctor.xai_key_configured ? "Configured" : "Not set (XAI_API_KEY)"],
    ["Gemini API key", desk.doctor.gemini_key_configured ? "Configured" : "Not set (GEMINI_API_KEY)"],
    ["Geocoder", desk.doctor.geocoder_ok ? `Configured (${desk.doctor.geocoder_url})` : desk.doctor.geocoder_error || "Not configured"],
    ["Data directory", desk.doctor.data_dir],
    ["Dry-run env", desk.doctor.dry_run ? "GPSIFY_DRY_RUN=1 active" : "Off"],
  ];
  for (const [k, v] of rows) {
    const dt = document.createElement("dt");
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = v;
    dl.append(dt, dd);
  }

  const jdl = $("job-stats-dl");
  jdl.replaceChildren();
  if (desk.activeJobId && desk.statusRollup) {
    const propose = (desk.statusRollup.stats && desk.statusRollup.stats.propose) || {};
    const jobRows = [
      ["Active album", desk.statusRollup.root_path],
      ["Copies folder", desk.statusRollup.dest_root],
      ["Est. vision spend", propose.est_usd != null ? `$${Number(propose.est_usd).toFixed(4)}` : "$0.0000"],
      ["Tokens (in/out/reason)", `${propose.input_tokens || 0} / ${propose.output_tokens || 0} / ${propose.reasoning_tokens || 0}`],
      ["Low-rank proposed", String(desk.lowRankCount)],
    ];
    for (const [k, v] of jobRows) {
      const dt = document.createElement("dt");
      dt.textContent = k;
      const dd = document.createElement("dd");
      dd.textContent = v;
      jdl.append(dt, dd);
    }
  }
  $("btn-skip-low").disabled = !desk.activeJobId || desk.lowRankCount === 0;
  $("btn-clean-cache").disabled = !desk.activeJobId;
  $("doctor-msg").classList.add("hidden");
  $("doctor-dialog").showModal();
}

function wireDesktopHandlers() {
  installClosedByFallback();

  desktop.onProgress((payload) => {
    if (!payload) return;
    if (payload.done) setProgress(false);
    else setProgress(true, payload.stage, payload.message);
  });

  $("btn-jobs").addEventListener("click", () => openJobsDialog());
  $("btn-open-folder").addEventListener("click", () => openIngestDialogWithPicker());
  $("btn-propose").addEventListener("click", () => openProposeDialog());
  $("btn-apply-gps").addEventListener("click", () => openApplyGpsDialog());
  $("btn-undo-gps").addEventListener("click", () => openUndoDialog());
  $("btn-doctor").addEventListener("click", () => openDoctorDialog());

  $("btn-jobs-new").addEventListener("click", () => {
    $("jobs-dialog").close();
    openIngestDialogWithPicker();
  });

  $("btn-ingest-browse").addEventListener("click", async () => {
    const picked = await desktop.pickFolder("Select a photo album folder");
    if (picked) $("ingest-folder").value = picked;
  });
  $("btn-ingest-cancel").addEventListener("click", () => $("ingest-dialog").close());
  $("ingest-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const errEl = $("ingest-error");
    errEl.classList.add("hidden");
    const submitBtn = $("btn-ingest-submit");
    submitBtn.disabled = true;
    setBusy(true, "ingest", "Scanning folder and reading EXIF…");
    try {
      const res = await desktop.ingest({
        folder: $("ingest-folder").value.trim(),
        hint: $("ingest-hint").value.trim(),
        recursive: $("ingest-recursive").checked,
      });
      $("ingest-dialog").close();
      await applyOpenedState(res.opened);
      const list = await desktop.listJobs();
      desk.jobs = list.jobs || [];
      const s = res.summary;
      desk.hooks.setStatus(`Scanned ${s.total} photos (${s.tagged} tagged, ${s.untagged} untagged, ${s.clusters} clusters)`);
    } catch (err) {
      errEl.textContent = String((err && err.message) || err);
      errEl.classList.remove("hidden");
    } finally {
      submitBtn.disabled = false;
      setBusy(false);
    }
  });

  $("prov-xai").addEventListener("change", updateGeminiWarn);
  $("prov-gemini").addEventListener("change", updateGeminiWarn);
  $("btn-propose-cancel").addEventListener("click", () => $("propose-dialog").close());
  $("propose-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!desk.activeJobId) return;
    const errEl = $("propose-error");
    errEl.classList.add("hidden");
    const submitBtn = $("btn-propose-submit");
    submitBtn.disabled = true;
    const limitVal = $("propose-limit").value.trim();
    const costVal = $("propose-max-cost").value.trim();
    setBusy(true, "propose", "Running vision & geocoding…");
    try {
      const res = await desktop.propose({
        job_id: desk.activeJobId,
        provider: $("prov-gemini").checked ? "gemini" : "xai",
        limit: limitVal ? Number(limitVal) : null,
        max_cost: costVal ? Number(costVal) : null,
        dry_run: $("propose-dry-run").checked,
        force: $("propose-force").checked,
        no_context: $("propose-no-context").checked,
      });
      $("propose-dialog").close();
      desk.statusRollup = res.status;
      desk.unpinned = res.unpinned || 0;
      desk.lowRankCount = res.low_rank_count || 0;
      renderToolbar();
      await desk.hooks.reloadPhotos();
      const s = res.summary;
      const note = s.dry_run
        ? `Dry run: ${s.planned} photos queued`
        : `Proposed ${s.proposed}/${s.planned}, refused ${s.refused}, errors ${s.error}, ~$${Number(s.est_usd || 0).toFixed(4)}`;
      desk.hooks.setStatus(note, s.error > 0);
    } catch (err) {
      errEl.textContent = String((err && err.message) || err);
      errEl.classList.remove("hidden");
    } finally {
      submitBtn.disabled = false;
      setBusy(false);
    }
  });

  $("btn-geocode-pass").addEventListener("click", async () => {
    if (!desk.activeJobId || desk.busy) return;
    setBusy(true, "geocode", "Geocoding remaining place names (no vision spend)…");
    try {
      const res = await desktop.geocodePass({ jobId: desk.activeJobId });
      desk.statusRollup = res.status;
      desk.unpinned = res.unpinned || 0;
      desk.lowRankCount = res.low_rank_count || 0;
      renderToolbar();
      await desk.hooks.reloadPhotos();
      const s = res.summary;
      desk.hooks.setStatus(`Geocode pass: ${s.pinned}/${s.planned} pinned, ${s.unrefused} un-refused`);
    } catch (err) {
      desk.hooks.reportError(err);
    } finally {
      setBusy(false);
    }
  });

  $("btn-apply-gps-browse").addEventListener("click", async () => {
    const picked = await desktop.pickFolder("Select output folder for tagged copies");
    if (picked) $("apply-gps-out").value = picked;
  });
  $("apply-gps-in-place").addEventListener("change", (e) => {
    $("apply-gps-inplace-warn").classList.toggle("hidden", !e.target.checked);
  });
  $("btn-apply-gps-cancel").addEventListener("click", () => $("apply-dialog-gps").close());

  async function runApplyFlow(dryRun) {
    if (!desk.activeJobId) return;
    const errEl = $("apply-gps-error");
    const resEl = $("apply-gps-result");
    errEl.classList.add("hidden");
    resEl.classList.add("hidden");
    const submitBtn = $("btn-apply-gps-submit");
    const dryBtn = $("btn-apply-gps-dry");
    submitBtn.disabled = true;
    dryBtn.disabled = true;
    const inPlace = $("apply-gps-in-place").checked;
    setBusy(true, "apply", dryRun ? "Planning EXIF writes…" : "Writing GPS via exiftool…");
    try {
      const res = await desktop.apply({
        job_id: desk.activeJobId,
        out: $("apply-gps-out").value.trim() || null,
        in_place: inPlace,
        dry_run: dryRun,
        i_mean_it: inPlace && !dryRun,
      });
      desk.statusRollup = res.status;
      renderToolbar();
      const s = res.summary;
      if (dryRun) {
        resEl.textContent = `Dry run: ${s.eligible} photos ready -> ${s.dest_root}`;
        resEl.classList.remove("hidden");
      } else {
        $("apply-dialog-gps").close();
        await desk.hooks.reloadPhotos();
        desk.hooks.setStatus(`Wrote GPS to ${s.wrote} files (${s.failed} failed) -> ${s.dest_root}`, s.failed > 0);
      }
    } catch (err) {
      errEl.textContent = String((err && err.message) || err);
      errEl.classList.remove("hidden");
    } finally {
      submitBtn.disabled = false;
      dryBtn.disabled = false;
      setBusy(false);
    }
  }

  $("btn-apply-gps-dry").addEventListener("click", () => runApplyFlow(true));
  $("apply-gps-form").addEventListener("submit", (e) => {
    e.preventDefault();
    runApplyFlow(false);
  });

  $("btn-undo-cancel").addEventListener("click", () => $("undo-dialog").close());
  $("undo-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!desk.activeJobId) return;
    const errEl = $("undo-error");
    errEl.classList.add("hidden");
    const submitBtn = $("btn-undo-submit");
    submitBtn.disabled = true;
    setBusy(true, "undo", "Restoring backups…");
    try {
      const res = await desktop.undo(desk.activeJobId);
      $("undo-dialog").close();
      desk.statusRollup = res.status;
      renderToolbar();
      await desk.hooks.reloadPhotos();
      const s = res.summary;
      desk.hooks.setStatus(`Undo complete: ${s.undone} restored, ${s.skipped} skipped, ${s.failed} failed`, s.failed > 0);
    } catch (err) {
      errEl.textContent = String((err && err.message) || err);
      errEl.classList.remove("hidden");
    } finally {
      submitBtn.disabled = false;
      setBusy(false);
    }
  });

  $("btn-clean-cache").addEventListener("click", async () => {
    if (!desk.activeJobId) return;
    const res = await desktop.clean(desk.activeJobId);
    const msg = $("doctor-msg");
    msg.textContent = `Cleaned cached previews/thumbs (SQLite and backups preserved).`;
    msg.classList.remove("hidden");
    if (res) renderToolbar();
  });

  $("btn-skip-low").addEventListener("click", async () => {
    if (!desk.activeJobId) return;
    const res = await desktop.skipLow(desk.activeJobId);
    desk.statusRollup = res.status;
    desk.lowRankCount = res.low_rank_count || 0;
    renderToolbar();
    await desk.hooks.reloadPhotos();
    const msg = $("doctor-msg");
    msg.textContent = `Marked ${res.skipped} low-confidence photos as skipped.`;
    msg.classList.remove("hidden");
    $("btn-skip-low").disabled = true;
  });
}

export async function bootDesktop(hooks) {
  desk.hooks = hooks;
  document.body.classList.add("is-desktop");
  wireDesktopHandlers();

  try {
    const data = await desktop.bootstrap();
    desk.doctor = data.doctor;
    desk.jobs = data.jobs || [];
    desk.mediaBase = data.media_base || desk.mediaBase;

    if (data.active && data.active.job) {
      await applyOpenedState(data.active);
    } else {
      renderToolbar();
      renderWelcome();
      if (data.initial_folder) {
        await openIngestDialogWithPicker(data.initial_folder);
      }
    }
  } catch (err) {
    renderToolbar();
    renderWelcome();
    hooks.reportError(err);
  }
}

export function onPhotosUpdated() {
  refreshActiveStatus().catch(() => {});
}
