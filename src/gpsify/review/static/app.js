/* gpsify review — entry point: state, actions, keyboard, place search.

   Place search geocodes on Enter only; never on keystroke, never as typeahead
   (K10, OSMF policy). Nothing here writes GPS — closing the tab writes nothing,
   and only `gpsify apply` touches a file.

   Two verbs that look alike and are not:
     Accept    — every selected photo keeps its OWN proposal pin. Safe in bulk.
     Apply pin — ONE photo's pin is copied onto the rest of the selection. That
                 is the "one wrong church poisons 400 photos" case, so it goes
                 through /api/pin/apply and inherits K9's caps. */

import { api, previewUrl } from "./api.js";
import { createMap } from "./map.js";
import {
  formatWhen,
  groupPhotos,
  paintSelection,
  rankClass,
  renderEmpty,
  renderGrid,
} from "./grid.js";

const CLUSTER_N_CAP = 30;
const UNSTAMPED_FRACTION = 0.25;
const TILE_KEY = "gpsify.tileSize";
const FILTER_ORDER = ["needs_review", "low", "refused", "accepted", "all"];

const state = {
  job: null,
  filter: "needs_review",
  photos: [],
  order: [],
  tiles: new Map(),
  selection: new Set(),
  anchorId: null,
  rangeFromId: null,
  editing: false,
  pendingPin: null,
  placeName: "",
  map: null,
  lightboxOpen: false,
};

const $ = (id) => document.getElementById(id);

function setStatus(message, isError = false) {
  const node = $("status-msg");
  node.textContent = message || "";
  node.classList.toggle("error", Boolean(isError && message));
}

function reportError(err) {
  setStatus(String((err && err.message) || err), true);
}

const photoById = (id) => state.photos.find((p) => p.id === id) || null;
const anchor = () => photoById(state.anchorId);

// ---------------------------------------------------------------- selection

function setSelection(ids, anchorId) {
  setStatus("");
  state.selection = new Set(ids);
  state.anchorId = anchorId ?? (ids.length ? ids[ids.length - 1] : null);
  state.rangeFromId = state.anchorId;
  state.editing = false;
  state.pendingPin = null;
  paintSelection(state.tiles, state.selection, state.anchorId);
  renderInspector();
  renderActionBar();
}

function selectRangeTo(id) {
  setStatus("");
  const from = state.order.indexOf(state.rangeFromId);
  const to = state.order.indexOf(id);
  if (from < 0 || to < 0) return setSelection([id], id);
  const [lo, hi] = from <= to ? [from, to] : [to, from];
  const ids = state.order.slice(lo, hi + 1);
  state.selection = new Set(ids);
  state.anchorId = id;
  state.editing = false;
  state.pendingPin = null;
  paintSelection(state.tiles, state.selection, state.anchorId);
  renderInspector();
  renderActionBar();
}

function toggle(id) {
  const next = new Set(state.selection);
  if (next.has(id) && next.size > 1) next.delete(id);
  else next.add(id);
  setSelection([...next], id);
}

function moveAnchor(delta) {
  if (!state.order.length) return;
  const at = state.order.indexOf(state.anchorId);
  const next = state.order[Math.min(state.order.length - 1, Math.max(0, at + delta))];
  if (!next) return;
  setSelection([next], next);
  const tile = state.tiles.get(next);
  if (tile) tile.scrollIntoView({ block: "nearest" });
  if (state.lightboxOpen) renderLightbox();
}

function columnsPerRow() {
  const grid = $("grid-pane").querySelector(".grid");
  if (!grid) return 1;
  const cols = getComputedStyle(grid).gridTemplateColumns.split(" ").length;
  return Math.max(1, cols);
}

// ---------------------------------------------------------------- rendering

function renderFilters() {
  $("job-name").textContent = state.job ? shortRoot(state.job.root_path) : "";
  $("job-name").title = (state.job && state.job.root_path) || "";
  for (const button of $("filters").querySelectorAll("button[data-filter]")) {
    const active = button.dataset.filter === state.filter;
    button.classList.toggle("active", active);
    let count = button.querySelector(".count");
    if (!count) {
      count = document.createElement("span");
      count.className = "count";
      button.appendChild(count);
    }
    // Only the active filter gets a number, and it comes from the list on
    // screen. `low` is a ui_rank threshold, not a status, so it cannot be
    // derived from the job's status histogram without lying about it.
    count.textContent = active ? String(state.photos.length) : "";
  }
}

function shortRoot(path) {
  const parts = String(path || "").split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path || "";
}

function renderPhotos() {
  const pane = $("grid-pane");
  if (!state.photos.length) {
    state.tiles = new Map();
    state.order = [];
    renderEmpty(pane, emptyCopy());
    renderInspector();
    renderActionBar();
    return;
  }
  const groups = groupPhotos(state.photos);
  state.order = groups.flatMap((g) => g.photos.map((p) => p.id));
  state.tiles = renderGrid(pane, groups, {
    onSelectGroup: (group) => setSelection(group.photos.map((p) => p.id), group.photos[0].id),
  });
  for (const [id, tile] of state.tiles) {
    tile.addEventListener("click", (event) => {
      if (event.shiftKey) selectRangeTo(id);
      else if (event.metaKey || event.ctrlKey) toggle(id);
      else setSelection([id], id);
    });
  }
  if (!state.anchorId || !state.tiles.has(state.anchorId)) {
    setSelection([state.order[0]], state.order[0]);
  } else {
    paintSelection(state.tiles, state.selection, state.anchorId);
    renderInspector();
    renderActionBar();
  }
}

function emptyCopy() {
  const root = (state.job && state.job.root_path) || "<folder>";
  if (state.filter === "needs_review") {
    return {
      title: "Nothing left to review",
      body: "Every photo in this job has a decision. Write the accepted pins to files with:",
      command: `gpsify apply ${root}`,
    };
  }
  return {
    title: "No photos in this filter",
    body: "Try another filter, or propose more photos first.",
    command: null,
  };
}

function renderInspector() {
  const photo = anchor();
  const proposal = (photo && photo.proposal) || null;

  if (!photo) {
    $("place-name").textContent = "";
    $("rank-badge").textContent = "—";
    $("rank-badge").className = "badge";
    $("meta").replaceChildren();
    $("notice").classList.add("hidden");
    $("clues-block").classList.add("hidden");
    $("candidates-block").classList.add("hidden");
    if (state.map) state.map.clear();
    return;
  }

  state.placeName = (proposal && proposal.place_name) || "";
  $("place-name").textContent = state.placeName || photo.relpath;

  const rank = proposal ? proposal.ui_rank : null;
  const badge = $("rank-badge");
  badge.textContent = rank == null ? "—" : Number(rank).toFixed(2);
  badge.className = `badge ${rankClass(rank)}`;

  const meta = $("meta");
  meta.replaceChildren();
  for (const bit of [photo.relpath, formatWhen(photo.datetime_original), (proposal && proposal.granularity) || photo.status]) {
    if (!bit) continue;
    if (meta.childNodes.length) {
      const sep = document.createElement("span");
      sep.className = "sep";
      sep.textContent = "·";
      meta.appendChild(sep);
    }
    const span = document.createElement("span");
    span.textContent = bit;
    meta.appendChild(span);
  }

  const notice = $("notice");
  notice.classList.remove("editing");
  if (state.editing) {
    notice.textContent = "Click the map or drag the pin, then Save pin.";
    notice.classList.remove("hidden");
    notice.classList.add("editing");
  } else if (proposal && (proposal.model_agree === 0 || proposal.model_agree === false)) {
    notice.textContent = "The model and the map search disagree. The dashed pin is the model's guess.";
    notice.classList.remove("hidden");
  } else if (proposal && proposal.refusal && proposal.refusal !== "none") {
    notice.textContent = `The model refused this one: ${proposal.refusal.replace(/_/g, " ")}.`;
    notice.classList.remove("hidden");
  } else {
    notice.classList.add("hidden");
  }

  const clues = (proposal && proposal.clues) || [];
  $("clues-block").classList.toggle("hidden", clues.length === 0);
  const clueList = $("clues");
  clueList.replaceChildren();
  for (const clue of clues) {
    const li = document.createElement("li");
    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = clue.kind;
    const text = document.createElement("span");
    text.textContent = clue.text;
    li.append(kind, text);
    clueList.appendChild(li);
  }

  const candidates = (proposal && proposal.candidates) || [];
  $("candidates-block").classList.toggle("hidden", candidates.length === 0);
  const candList = $("candidates");
  candList.replaceChildren();
  for (const candidate of candidates) {
    const li = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = candidate.place_name;
    button.addEventListener("click", () => runGeocode(candidate.place_name));
    li.appendChild(button);
    candList.appendChild(li);
  }

  renderPin(photo, proposal);
}

function renderPin(photo, proposal) {
  if (!state.map) return;
  state.map.clear();
  const decision = photo.decision;
  const lat = state.pendingPin ? state.pendingPin.lat
    : decision && decision.lat != null ? decision.lat
      : proposal && proposal.pin_source !== "none" ? proposal.pin_lat : null;
  const lng = state.pendingPin ? state.pendingPin.lng
    : decision && decision.lng != null ? decision.lng
      : proposal && proposal.pin_source !== "none" ? proposal.pin_lng : null;

  if (lat != null && lng != null) {
    state.map.setPin(lat, lng, {
      draggable: state.editing,
      onDrag: (dLat, dLng) => { state.pendingPin = { lat: dLat, lng: dLng }; },
    });
  } else {
    state.map.reset();
  }
  if (proposal && proposal.lat_hint != null && proposal.lng_hint != null
      && (proposal.model_agree === 0 || proposal.model_agree === false)) {
    state.map.setGhost(proposal.lat_hint, proposal.lng_hint);
  }
  $("map").classList.toggle("editing", state.editing);
}

function renderActionBar() {
  const n = state.selection.size;
  const photo = anchor();
  const proposal = (photo && photo.proposal) || null;
  const canPin = Boolean(state.pendingPin) || Boolean(
    proposal && (proposal.pin_source === "geocoder" || proposal.pin_source === "user"),
  );

  const label = $("selection-count");
  label.replaceChildren();
  const head = document.createElement("span");
  head.textContent = n > 1 ? `${n} selected` : photo ? "1 photo" : "nothing";
  label.appendChild(head);
  if (n > 1) {
    const sub = document.createElement("span");
    sub.className = "sub";
    sub.textContent = `${countProposed(state.selection)} can take a pin`;
    label.appendChild(sub);
  }

  $("btn-accept").firstChild.textContent = n > 1 ? `Accept ${n}` : "Accept";
  $("btn-accept").disabled = !photo || (n === 1 && !canPin);
  $("btn-skip").firstChild.textContent = n > 1 ? `Skip ${n}` : "Skip";
  $("btn-skip").disabled = !photo;

  const edit = $("btn-edit");
  edit.firstChild.textContent = state.editing ? "Save pin" : "Edit pin";
  edit.classList.toggle("primary", state.editing);
  edit.disabled = !photo || n > 1;

  const applyPin = $("btn-apply-pin");
  applyPin.firstChild.textContent = n > 1 ? `Apply pin to ${n - 1}` : "Apply pin";
  applyPin.disabled = !photo || n < 2 || !canPin;
  applyPin.title = n < 2
    ? "Select more photos to copy this pin onto them"
    : `Copy ${state.placeName || "this pin"} onto the rest of the selection`;

  const model = $("btn-model");
  const showModel = Boolean(
    proposal && proposal.lat_hint != null && proposal.lng_hint != null
    && n === 1 && (proposal.model_agree === 0 || proposal.model_agree === false || !canPin),
  );
  model.classList.toggle("hidden", !showModel);
}

function countProposed(ids) {
  let n = 0;
  for (const id of ids) {
    const photo = photoById(id);
    if (photo && photo.status === "proposed") n += 1;
  }
  return n;
}

// ---------------------------------------------------------------- lightbox

function renderLightbox() {
  const photo = anchor();
  if (!photo) return;
  $("lightbox-img").src = previewUrl(photo.id);
  $("lightbox-img").alt = photo.relpath;
  $("lightbox-path").textContent = photo.relpath;
  $("lightbox-when").textContent = formatWhen(photo.datetime_original);
  $("lightbox-place").textContent = (photo.proposal && photo.proposal.place_name) || "";
}

function toggleLightbox(open) {
  state.lightboxOpen = open ?? !state.lightboxOpen;
  $("lightbox").classList.toggle("hidden", !state.lightboxOpen);
  if (state.lightboxOpen) renderLightbox();
}

// ---------------------------------------------------------------- actions

async function loadPhotos({ keepAnchor = true } = {}) {
  const previousAnchor = keepAnchor ? state.anchorId : null;
  const previousIndex = state.order.indexOf(previousAnchor);
  const data = await api.photos(state.filter);
  state.photos = data.photos || [];
  state.job = await api.job();
  renderFilters();

  const stillHere = state.photos.some((p) => p.id === previousAnchor);
  if (!stillHere) {
    // The anchor left the filter (a decision moved it). Land where it was, the
    // way a filmstrip advances, instead of snapping back to the top.
    state.anchorId = null;
    state.selection = new Set();
    renderPhotos();
    if (previousIndex >= 0 && state.order.length) {
      const next = state.order[Math.min(previousIndex, state.order.length - 1)];
      setSelection([next], next);
    }
    return;
  }
  renderPhotos();
}

async function decideMany(action, extra) {
  const ids = [...state.selection];
  if (!ids.length) return;
  let ok = 0;
  const failures = [];
  for (const id of ids) {
    try {
      await api.decide(id, { action, ...(extra || {}) });
      ok += 1;
    } catch (err) {
      failures.push(String((err && err.message) || err));
    }
  }
  const noun = ok === 1 ? "photo" : "photos";
  setStatus(
    failures.length
      ? `${action}: ${ok} ${noun}, ${failures.length} failed — ${failures[0]}`
      : `${action}: ${ok} ${noun}`,
    failures.length > 0,
  );
  await loadPhotos();
}

/* Client-side mirror of app.batch_needs_confirm, for the dialog copy only. The
   server re-checks and 400s regardless of what the UI decided. */
function needsTypedConfirm(targetIds) {
  const targets = targetIds.map(photoById).filter(Boolean);
  const candidates = targets.filter((p) => p.status === "proposed");
  if (targetIds.length > CLUSTER_N_CAP) return true;
  if (targets.length && targets.every((p) => !p.datetime_original)) return true;
  if (!candidates.length) return false;
  const missing = candidates.filter((p) => !p.datetime_original).length;
  return missing / candidates.length > UNSTAMPED_FRACTION;
}

function askApplyConfirm(sourcePlace, targetIds) {
  const dialog = $("apply-dialog");
  const n = targetIds.length;
  const needed = needsTypedConfirm(targetIds);
  const undated = targetIds.map(photoById).filter((p) => p && !p.datetime_original).length;

  $("apply-title").textContent = `Copy this pin onto ${n} photo${n === 1 ? "" : "s"}?`;
  $("apply-body").textContent = needed
    ? `${sourcePlace || "This pin"} will be written to ${n} photos`
      + (undated ? `, ${undated} of which have no capture time` : "")
      + ". A wrong pin copied this widely is hard to notice later, so type the count to confirm."
    : `${sourcePlace || "This pin"} will be written to ${n} photos. They keep their own files until you run gpsify apply.`;
  $("apply-confirm-wrap").classList.toggle("hidden", !needed);
  $("apply-confirm-label").textContent = `Type ${n} to confirm`;
  $("apply-confirm").value = "";

  return new Promise((resolve) => {
    const form = $("apply-form");
    const onSubmit = (event) => {
      event.preventDefault();
      form.removeEventListener("submit", onSubmit);
      const applied = event.submitter && event.submitter.value === "apply";
      dialog.close();
      if (!applied) return resolve(null);
      resolve({ confirmN: needed ? Number($("apply-confirm").value) : undefined });
    };
    form.addEventListener("submit", onSubmit);
    dialog.showModal();
  });
}

async function applyPinToSelection() {
  const source = anchor();
  if (!source) return;
  const targets = [...state.selection].filter((id) => id !== source.id);
  if (!targets.length) {
    setStatus("Select the photos to copy this pin onto.");
    return;
  }

  // /api/pin/apply copies the source's *decision*, so the source has to have
  // one. Accepting it first is what the user meant by pressing this.
  if (!source.decision || source.decision.lat == null) {
    try {
      const payload = state.pendingPin
        ? { action: "accept_edited", lat: state.pendingPin.lat, lng: state.pendingPin.lng, place_name: state.placeName || undefined }
        : { action: "accept" };
      await api.decide(source.id, payload);
    } catch (err) {
      setStatus(`This photo has no pin yet — set one with Edit pin first. (${(err && err.message) || err})`, true);
      return;
    }
  }

  const answer = await askApplyConfirm(state.placeName, targets);
  if (!answer) return;
  try {
    const result = await api.applyPin({
      source_photo_id: source.id,
      photo_ids: targets,
      ...(answer.confirmN === undefined ? {} : { confirm_n: answer.confirmN }),
    });
    setStatus(`Pin applied to ${result.count} photos. Nothing written to disk yet.`);
  } catch (err) {
    reportError(err);
  }
  await loadPhotos();
}

async function runGeocode(query) {
  const q = (query || "").trim();
  if (q.length < 3) {
    setStatus("Type at least 3 characters, then press Enter.");
    return;
  }
  try {
    const data = await api.geocode(q);
    if (!data.hit) {
      setStatus("No match. Drop a pin on the map instead.");
      return;
    }
    state.editing = true;
    state.pendingPin = { lat: data.hit.lat, lng: data.hit.lng };
    state.placeName = data.hit.display_name || q;
    $("place-name").textContent = state.placeName;
    renderPin(anchor(), (anchor() || {}).proposal);
    renderInspector();
    renderActionBar();
    setStatus("Pin from search. Press Save pin to keep it.");
  } catch (err) {
    reportError(err);
  }
}

async function toggleEditPin() {
  const photo = anchor();
  if (!photo) return;
  if (!state.editing) {
    state.editing = true;
    if (state.map) state.map.setDraggable(true);
    renderInspector();
    renderActionBar();
    setStatus("Click the map or drag the pin, then Save pin.");
    return;
  }
  if (!state.pendingPin) {
    setStatus("Click the map to place a pin first.");
    return;
  }
  const { lat, lng } = state.pendingPin;
  state.editing = false;
  try {
    await api.decide(photo.id, {
      action: "accept_edited",
      lat,
      lng,
      place_name: state.placeName || undefined,
    });
    setStatus("Pin saved. Nothing written to disk yet.");
  } catch (err) {
    reportError(err);
  }
  state.pendingPin = null;
  await loadPhotos();
}

function cancelEdit() {
  if (state.lightboxOpen) return toggleLightbox(false);
  if (state.editing) {
    state.editing = false;
    state.pendingPin = null;
    renderInspector();
    renderActionBar();
    setStatus("");
    return;
  }
  if (state.anchorId) setSelection([state.anchorId], state.anchorId);
}

async function setFilter(filter) {
  setStatus("");
  state.filter = filter;
  state.anchorId = null;
  state.selection = new Set();
  renderFilters();
  await loadPhotos({ keepAnchor: false });
}

// ---------------------------------------------------------------- wiring

$("filters").addEventListener("click", (event) => {
  const button = event.target.closest("button[data-filter]");
  if (!button) return;
  setFilter(button.dataset.filter).catch(reportError);
});

$("btn-accept").addEventListener("click", () => decideMany("accept").catch(reportError));
$("btn-skip").addEventListener("click", () => decideMany("skip").catch(reportError));
$("btn-edit").addEventListener("click", () => toggleEditPin().catch(reportError));
$("btn-apply-pin").addEventListener("click", () => applyPinToSelection().catch(reportError));
$("btn-model").addEventListener("click", () => {
  const photo = anchor();
  const proposal = photo && photo.proposal;
  if (!proposal || proposal.lat_hint == null) return;
  api.decide(photo.id, {
    action: "accept_edited",
    lat: proposal.lat_hint,
    lng: proposal.lng_hint,
    place_name: proposal.place_name,
  }).then(() => loadPhotos()).catch(reportError);
});
$("btn-help").addEventListener("click", () => $("help-dialog").showModal());
$("lightbox").addEventListener("click", () => toggleLightbox(false));

/* "change", not "input": a range slider fires input on every pixel of the drag,
   and this file is held to a no-input-listener rule so no one can ever wire
   keystroke geocoding onto the search box. The grid resizes on release. */
$("tile-size").addEventListener("change", (event) => {
  const size = Number(event.target.value);
  document.documentElement.style.setProperty("--tile", `${size}px`);
  try {
    window.localStorage.setItem(TILE_KEY, String(size));
  } catch {
    /* private windows and blocked site data: the size just does not persist. */
  }
});

const search = $("place-search");
search.addEventListener("keydown", (event) => {
  if (event.key !== "Enter") return;
  event.preventDefault();
  runGeocode(search.value);
});

document.addEventListener("keydown", (event) => {
  const tag = event.target && event.target.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
  if (document.querySelector("dialog[open]")) return;

  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "a") {
    event.preventDefault();
    setSelection(state.order, state.anchorId || state.order[0]);
    return;
  }
  if (event.metaKey || event.ctrlKey || event.altKey) return;

  switch (event.key) {
    case " ":
      event.preventDefault();
      toggleLightbox();
      return;
    case "Escape":
      cancelEdit();
      return;
    case "ArrowRight":
    case "j":
      event.preventDefault();
      moveAnchor(1);
      return;
    case "ArrowLeft":
    case "k":
      event.preventDefault();
      moveAnchor(-1);
      return;
    case "ArrowDown":
      event.preventDefault();
      moveAnchor(columnsPerRow());
      return;
    case "ArrowUp":
      event.preventDefault();
      moveAnchor(-columnsPerRow());
      return;
    case "a":
      $("btn-accept").click();
      return;
    case "s":
      $("btn-skip").click();
      return;
    case "e":
      $("btn-edit").click();
      return;
    case "g":
      $("btn-apply-pin").click();
      return;
    case "?":
      $("help-dialog").showModal();
      return;
    default:
      break;
  }
  const slot = FILTER_ORDER[Number(event.key) - 1];
  if (slot) setFilter(slot).catch(reportError);
});

// ---------------------------------------------------------------- boot

(async () => {
  try {
    let size = 160;
    try {
      size = Number(window.localStorage.getItem(TILE_KEY)) || 160;
    } catch {
      /* storage unavailable; the default size is fine. */
    }
    document.documentElement.style.setProperty("--tile", `${size}px`);
    $("tile-size").value = String(size);

    state.job = await api.job();
    $("osm-attribution").textContent = state.job.attribution || "© OpenStreetMap contributors";
    state.map = createMap("map", {
      tileUrl: state.job.map_tile_url,
      attribution: state.job.attribution,
      onPick: (lat, lng) => {
        if (!state.editing) return;
        state.pendingPin = { lat, lng };
        renderPin(anchor(), (anchor() || {}).proposal);
        renderActionBar();
      },
    });
    renderFilters();
    await loadPhotos({ keepAnchor: false });
    state.map.refresh();
  } catch (err) {
    reportError(err);
  }
})();
