/* Photo grid: cluster grouping, tile rendering, selection painting.

   A cluster (K9) is "photos from one folder shot within 45 minutes of each
   other". The grid says that in words — a date and a time span — instead of
   showing the term. Undated groups are called out because they are the ones
   the cluster-apply caps exist to protect. */

import { thumbUrl } from "./api.js";

const UNDATED = "—undated—";

/* EXIF stamps look like "2019:07:12 14:02:00"; cluster.py also accepts ISO with
   a Z or an offset. Parse the same shapes, and treat anything else as undated
   rather than guessing. */
export function parseCaptureTime(raw) {
  if (!raw) return null;
  const text = String(raw).trim().replace(/(Z|[+-]\d{2}:?\d{2})$/, "");
  const match = text.match(/^(\d{4})[:-](\d{2})[:-](\d{2})[T ](\d{2}):(\d{2}):(\d{2})/);
  if (!match) return null;
  const [, y, mo, d, h, mi, s] = match.map(Number);
  const when = new Date(y, mo - 1, d, h, mi, s);
  return Number.isNaN(when.getTime()) ? null : when;
}

const DAY = new Intl.DateTimeFormat(undefined, { weekday: "short", day: "numeric", month: "short" });
const CLOCK = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

export function formatWhen(raw) {
  const when = parseCaptureTime(raw);
  return when ? `${DAY.format(when)} ${CLOCK.format(when)}` : "no capture time";
}

function dirnameOf(relpath) {
  const cut = String(relpath || "").lastIndexOf("/");
  return cut < 0 ? "." : relpath.slice(0, cut);
}

export function rankClass(rank) {
  if (rank == null) return "";
  if (rank >= 0.8) return "high";
  if (rank >= 0.55) return "mid";
  return "low";
}

function statusClass(status) {
  if (status === "proposed") return "proposed";
  if (status === "refused") return "refused";
  if (status === "written") return "written";
  if (status === "error" || status === "write_failed") return "error";
  if (status.startsWith("accepted")) return "accepted";
  return "skipped";
}

/* Group by cluster, then order folder-first and oldest-first, with each
   folder's undated group last — the same shape cluster.py partitions into. */
export function groupPhotos(photos) {
  const groups = new Map();
  for (const photo of photos) {
    const key = photo.cluster_id || `${dirnameOf(photo.relpath)}::${UNDATED}`;
    let group = groups.get(key);
    if (!group) {
      group = { key, dirname: dirnameOf(photo.relpath), photos: [], first: null, last: null };
      groups.set(key, group);
    }
    group.photos.push(photo);
    const when = parseCaptureTime(photo.datetime_original);
    if (when) {
      if (!group.first || when < group.first) group.first = when;
      if (!group.last || when > group.last) group.last = when;
    }
  }

  for (const group of groups.values()) {
    group.undated = group.first === null;
    group.photos.sort((a, b) => {
      const at = parseCaptureTime(a.datetime_original);
      const bt = parseCaptureTime(b.datetime_original);
      if (at && bt && at - bt !== 0) return at - bt;
      return String(a.relpath).localeCompare(String(b.relpath));
    });
    group.label = group.undated
      ? "No capture time"
      : group.first.getTime() === group.last.getTime()
        ? `${DAY.format(group.first)} ${CLOCK.format(group.first)}`
        : `${DAY.format(group.first)} ${CLOCK.format(group.first)}–${CLOCK.format(group.last)}`;
  }

  return [...groups.values()].sort((a, b) => {
    const byDir = a.dirname.localeCompare(b.dirname);
    if (byDir !== 0) return byDir;
    if (a.undated !== b.undated) return a.undated ? 1 : -1;
    if (a.undated) return 0;
    return a.first - b.first;
  });
}

function tileFor(photo) {
  const tile = document.createElement("button");
  tile.type = "button";
  tile.className = "tile";
  tile.dataset.photoId = photo.id;

  const img = document.createElement("img");
  img.src = thumbUrl(photo.id);
  img.alt = photo.relpath || photo.id;
  /* 2,000 tiles would otherwise be 2,000 thumb renders on first paint. */
  img.loading = "lazy";
  img.decoding = "async";
  tile.appendChild(img);

  const dot = document.createElement("span");
  dot.className = `dot ${statusClass(photo.status)}`;
  dot.title = photo.status;
  tile.appendChild(dot);

  const rank = photo.proposal ? photo.proposal.ui_rank : null;
  if (rank != null) {
    const chip = document.createElement("span");
    chip.className = `rank ${rankClass(rank)}`;
    chip.textContent = Number(rank).toFixed(2);
    tile.appendChild(chip);
  }

  const caption = document.createElement("span");
  caption.className = "caption";
  caption.textContent = (photo.proposal && photo.proposal.place_name) || photo.relpath;
  tile.appendChild(caption);

  return tile;
}

/* Full re-render on data change. Selection changes only toggle classes, so
   scrolling a large grid never rebuilds the DOM. */
export function renderGrid(pane, groups, { onSelectGroup }) {
  pane.replaceChildren();
  const tiles = new Map();

  for (const group of groups) {
    const head = document.createElement("div");
    head.className = `group-head${group.undated ? " undated" : ""}`;

    const title = document.createElement("span");
    title.className = "group-title";
    title.textContent = group.undated ? `⚠ ${group.label}` : group.label;
    head.appendChild(title);

    const sub = document.createElement("span");
    sub.className = "group-sub";
    const n = group.photos.length;
    sub.textContent = `${group.dirname === "." ? "album root" : group.dirname} · ${n} photo${n === 1 ? "" : "s"}`;
    head.appendChild(sub);

    const select = document.createElement("button");
    select.type = "button";
    select.textContent = `Select ${n}`;
    select.addEventListener("click", () => onSelectGroup(group));
    head.appendChild(select);

    pane.appendChild(head);

    const grid = document.createElement("div");
    grid.className = "grid";
    for (const photo of group.photos) {
      const tile = tileFor(photo);
      tiles.set(photo.id, tile);
      grid.appendChild(tile);
    }
    pane.appendChild(grid);
  }

  return tiles;
}

export function paintSelection(tiles, selection, anchorId) {
  for (const [id, tile] of tiles) {
    tile.classList.toggle("selected", selection.has(id));
    tile.classList.toggle("anchor", id === anchorId);
  }
}

export function renderEmpty(pane, { title, body, command }) {
  pane.replaceChildren();
  const wrap = document.createElement("div");
  wrap.className = "empty";

  const heading = document.createElement("h2");
  heading.textContent = title;
  wrap.appendChild(heading);

  const text = document.createElement("p");
  text.textContent = body;
  wrap.appendChild(text);

  if (command) {
    const code = document.createElement("code");
    code.textContent = command;
    wrap.appendChild(code);
  }
  pane.appendChild(wrap);
}
