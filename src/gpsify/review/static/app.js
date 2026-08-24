/* Review UI. Place search geocodes on Enter only; never on keystroke / typeahead. */
(() => {
  const state = {
    photos: [],
    index: 0,
    filter: "needs_review",
    job: null,
    map: null,
    pin: null,
    ghost: null,
    editMode: false,
    cluster: null,
  };

  const $ = (id) => document.getElementById(id);

  async function api(path, options) {
    const response = await fetch(path, options);
    const text = await response.text();
    let body = null;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      body = { error: text };
    }
    if (!response.ok) {
      throw new Error((body && body.error) || response.statusText);
    }
    return body;
  }

  function rankClass(rank) {
    if (rank == null) return "";
    if (rank >= 0.8) return "high";
    if (rank >= 0.55) return "mid";
    return "low";
  }

  function currentPhoto() {
    return state.photos[state.index] || null;
  }

  function setStatus(message) {
    $("status-msg").textContent = message || "";
  }

  function renderFilmstrip() {
    const strip = $("filmstrip");
    strip.replaceChildren();
    state.photos.forEach((photo, idx) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = idx === state.index ? "active" : "";
      const img = document.createElement("img");
      img.alt = photo.relpath || photo.id;
      img.src = `/api/thumb/${photo.id}`;
      btn.appendChild(img);
      btn.addEventListener("click", () => {
        state.index = idx;
        renderPhoto();
      });
      strip.appendChild(btn);
    });
  }

  function ensureMap(tileUrl, attribution) {
    if (state.map) return;
    state.map = L.map("map");
    L.tileLayer(tileUrl || "https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
      attribution: attribution || "© OpenStreetMap contributors",
    }).addTo(state.map);
    state.map.setView([20, 0], 2);
    state.map.on("click", (event) => {
      if (!state.editMode) return;
      placeUserPin(event.latlng.lat, event.latlng.lng);
    });
  }

  function clearPins() {
    if (state.pin) {
      state.map.removeLayer(state.pin);
      state.pin = null;
    }
    if (state.ghost) {
      state.map.removeLayer(state.ghost);
      state.ghost = null;
    }
  }

  function placeUserPin(lat, lng) {
    if (!state.map) return;
    if (state.pin) state.map.removeLayer(state.pin);
    state.pin = L.marker([lat, lng], { draggable: state.editMode }).addTo(state.map);
    state.pin.on("dragend", () => {
      const pos = state.pin.getLatLng();
      state.pendingLat = pos.lat;
      state.pendingLng = pos.lng;
    });
    state.pendingLat = lat;
    state.pendingLng = lng;
    state.map.setView([lat, lng], Math.max(state.map.getZoom(), 12));
  }

  function renderPhoto() {
    const photo = currentPhoto();
    const empty = $("preview-empty");
    const img = $("preview");
    renderFilmstrip();
    if (!photo) {
      img.classList.add("hidden");
      empty.classList.remove("hidden");
      $("place-name").textContent = "";
      $("granularity").textContent = "";
      $("rank-badge").textContent = "—";
      $("clues").replaceChildren();
      $("candidates").replaceChildren();
      $("btn-accept").disabled = true;
      return;
    }
    empty.classList.add("hidden");
    img.classList.remove("hidden");
    img.src = `/api/preview/${photo.id}`;
    img.alt = photo.relpath || photo.id;
    const proposal = photo.proposal || {};
    const rank = proposal.ui_rank;
    const badge = $("rank-badge");
    badge.textContent = rank == null ? "—" : Number(rank).toFixed(2);
    badge.className = `badge ${rankClass(rank)}`;
    $("place-name").textContent = proposal.place_name || photo.relpath || "";
    $("granularity").textContent = proposal.granularity || photo.status;
    const disagree = $("disagree");
    if (proposal.model_agree === 0 || proposal.model_agree === false) {
      disagree.classList.remove("hidden");
      disagree.textContent = "Model and map search disagree.";
    } else {
      disagree.classList.add("hidden");
      disagree.textContent = "";
    }
    const clues = $("clues");
    clues.replaceChildren();
    (proposal.clues || []).forEach((clue) => {
      const li = document.createElement("li");
      li.textContent = `${clue.kind}: ${clue.text}`;
      clues.appendChild(li);
    });
    const candidates = $("candidates");
    candidates.replaceChildren();
    (proposal.candidates || []).forEach((candidate) => {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = candidate.place_name;
      btn.addEventListener("click", () => geocodeQuery(candidate.place_name));
      li.appendChild(btn);
      candidates.appendChild(li);
    });
    const canAccept = proposal.pin_source === "geocoder" || proposal.pin_source === "user";
    $("btn-accept").disabled = !canAccept;
    const modelBtn = $("btn-model");
    const showModel = (
      proposal.lat_hint != null
      && proposal.lng_hint != null
      && (proposal.model_agree === 0 || proposal.model_agree === false || !canAccept)
    );
    modelBtn.classList.toggle("hidden", !showModel);
    clearPins();
    if (proposal.pin_lat != null && proposal.pin_lng != null && proposal.pin_source !== "none") {
      placeUserPin(proposal.pin_lat, proposal.pin_lng);
    } else if (state.map) {
      state.map.setView([20, 0], 2);
    }
    if (proposal.lat_hint != null && proposal.lng_hint != null && (proposal.model_agree === 0 || proposal.model_agree === false)) {
      state.ghost = L.circleMarker([proposal.lat_hint, proposal.lng_hint], {
        color: "#e0a100",
        radius: 8,
      }).addTo(state.map);
    }
    state.editMode = false;
  }

  async function loadPhotos() {
    const data = await api(`/api/photos?status=${encodeURIComponent(state.filter)}`);
    state.photos = data.photos || [];
    state.index = 0;
    renderPhoto();
  }

  async function postDecision(action, extra) {
    const photo = currentPhoto();
    if (!photo) return;
    const body = { action, ...(extra || {}) };
    await api(`/api/photos/${photo.id}/decision`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    setStatus(`${action} saved`);
    await loadPhotos();
  }

  async function geocodeQuery(query) {
    const q = (query || "").trim();
    if (q.length < 3) {
      setStatus("Type at least 3 characters, then press Enter.");
      return;
    }
    try {
      const data = await api("/api/geocode", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ q }),
      });
      if (!data.hit) {
        setStatus("No geocode hit. Drop a pin.");
        return;
      }
      state.editMode = true;
      placeUserPin(data.hit.lat, data.hit.lng);
      $("place-name").textContent = data.hit.display_name || q;
      setStatus("Pin from search. Save with Edit pin / Accept edited.");
    } catch (err) {
      setStatus(String(err.message || err));
    }
  }

  async function openCluster() {
    const photo = currentPhoto();
    if (!photo || !photo.cluster_id) {
      setStatus("No cluster for this photo.");
      return;
    }
    const cluster = await api(`/api/clusters/${photo.cluster_id}`);
    state.cluster = cluster;
    const list = $("cluster-members");
    list.replaceChildren();
    const allowed = new Set(cluster.candidate_ids || []);
    (cluster.members || []).forEach((member) => {
      const li = document.createElement("li");
      const label = document.createElement("label");
      const box = document.createElement("input");
      box.type = "checkbox";
      box.value = member.photo_id;
      box.checked = allowed.has(member.photo_id);
      box.disabled = !allowed.has(member.photo_id);
      label.appendChild(box);
      label.append(` ${member.relpath} (${member.status})`);
      li.appendChild(label);
      list.appendChild(li);
    });
    const wrap = $("cluster-confirm-wrap");
    if (cluster.needs_confirm) {
      wrap.classList.remove("hidden");
      $("cluster-confirm").value = "";
    } else {
      wrap.classList.add("hidden");
    }
    $("cluster-dialog").showModal();
  }

  async function applyCluster(event) {
    event.preventDefault();
    const dialog = $("cluster-dialog");
    const submitter = event.submitter;
    if (!submitter || submitter.value !== "apply") {
      dialog.close();
      return;
    }
    const photo = currentPhoto();
    const ids = [...$("cluster-members").querySelectorAll("input:checked")].map(
      (el) => el.value
    );
    const payload = { source_photo_id: photo.id, photo_ids: ids };
    if (state.cluster && state.cluster.needs_confirm) {
      payload.confirm_n = Number($("cluster-confirm").value);
    }
    try {
      await api(`/api/clusters/${photo.cluster_id}/apply`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      dialog.close();
      setStatus("cluster apply saved");
      await loadPhotos();
    } catch (err) {
      setStatus(String(err.message || err));
    }
  }

  function nextPhoto() {
    if (!state.photos.length) return;
    state.index = (state.index + 1) % state.photos.length;
    renderPhoto();
  }

  function prevPhoto() {
    if (!state.photos.length) return;
    state.index = (state.index - 1 + state.photos.length) % state.photos.length;
    renderPhoto();
  }

  $("filters").addEventListener("click", (event) => {
    const btn = event.target.closest("button[data-filter]");
    if (!btn) return;
    state.filter = btn.getAttribute("data-filter");
    [...$("filters").querySelectorAll("button")].forEach((el) => {
      el.classList.toggle("active", el === btn);
    });
    loadPhotos().catch((err) => setStatus(String(err.message || err)));
  });

  $("btn-accept").addEventListener("click", () => postDecision("accept").catch((err) => setStatus(String(err.message || err))));
  $("btn-skip").addEventListener("click", () => postDecision("skip").catch((err) => setStatus(String(err.message || err))));
  $("btn-edit").addEventListener("click", async () => {
    const photo = currentPhoto();
    if (!photo) return;
    if (!state.editMode) {
      state.editMode = true;
      if (state.pin) state.pin.dragging.enable();
      setStatus("Drag the pin or search, then press Edit pin again to save.");
      return;
    }
    if (state.pendingLat == null || state.pendingLng == null) {
      setStatus("Drop or drag a pin first.");
      return;
    }
    await postDecision("accept_edited", {
      lat: state.pendingLat,
      lng: state.pendingLng,
      place_name: $("place-name").textContent || undefined,
    }).catch((err) => setStatus(String(err.message || err)));
  });
  $("btn-cluster").addEventListener("click", () => openCluster().catch((err) => setStatus(String(err.message || err))));
  $("btn-model").addEventListener("click", async () => {
    const photo = currentPhoto();
    const proposal = photo && photo.proposal;
    if (!proposal || proposal.lat_hint == null || proposal.lng_hint == null) return;
    await postDecision("accept_edited", {
      lat: proposal.lat_hint,
      lng: proposal.lng_hint,
      place_name: proposal.place_name,
    }).catch((err) => setStatus(String(err.message || err)));
  });
  $("btn-next").addEventListener("click", nextPhoto);
  $("cluster-form").addEventListener("submit", (event) => {
    applyCluster(event).catch((err) => setStatus(String(err.message || err)));
  });

  const search = $("place-search");
  search.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    geocodeQuery(search.value);
  });

  document.addEventListener("keydown", (event) => {
    if (event.target && (event.target.tagName === "INPUT" || event.target.tagName === "TEXTAREA")) {
      return;
    }
    if (event.key === "j") nextPhoto();
    if (event.key === "k") prevPhoto();
    if (event.key === "a") $("btn-accept").click();
    if (event.key === "s") $("btn-skip").click();
    if (event.key === "e") $("btn-edit").click();
    if (event.key === "c") $("btn-cluster").click();
  });

  (async () => {
    try {
      state.job = await api("/api/job");
      $("job-meta").textContent = state.job.id;
      $("osm-attribution").textContent = state.job.attribution || "© OpenStreetMap contributors";
      ensureMap(state.job.map_tile_url, state.job.attribution);
      await loadPhotos();
    } catch (err) {
      setStatus(String(err.message || err));
    }
  })();
})();
