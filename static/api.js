/* Dual-transport review + desktop IPC client.
   Browser mode talks strictly to loopback HTTP `/api/...`; desktop mode uses
   Tauri v2 IPC (`window.__TAURI__.core.invoke`) and custom `gpsify://` media URLs. */

export const isDesktop = Boolean(
  typeof window !== "undefined" &&
    window.__TAURI__ &&
    window.__TAURI__.core &&
    typeof window.__TAURI__.core.invoke === "function",
);

let mediaContext = {
  jobId: null,
  mediaBase: "gpsify://localhost",
};

export function setMediaContext(jobId, mediaBase) {
  mediaContext = {
    jobId: jobId || null,
    mediaBase: mediaBase || mediaContext.mediaBase,
  };
}

async function tauriInvoke(cmd, args) {
  try {
    return await window.__TAURI__.core.invoke(cmd, args || {});
  } catch (err) {
    throw new Error(typeof err === "string" ? err : (err && err.message) || String(err));
  }
}

async function request(path, options) {
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

function post(path, payload) {
  return request(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

export const api = {
  job: () => (isDesktop ? tauriInvoke("review_job") : request("/api/job")),
  photos: (filter) =>
    isDesktop
      ? tauriInvoke("review_photos", { status: filter })
      : request(`/api/photos?status=${encodeURIComponent(filter)}`),
  cluster: (id) =>
    isDesktop ? tauriInvoke("review_cluster", { clusterId: id }) : request(`/api/clusters/${id}`),
  decideMany: (payload) =>
    isDesktop ? tauriInvoke("review_decide_many", { payload }) : post("/api/decisions", payload),
  decide: (photoId, payload) =>
    isDesktop
      ? tauriInvoke("review_decide", { photoId, payload })
      : post(`/api/photos/${photoId}/decision`, payload),
  /* One accepted pin onto a hand-drawn selection. The server re-checks the K9
     caps; confirm_n is what the human typed when they trip. */
  applyPin: (payload) =>
    isDesktop ? tauriInvoke("review_apply_pin", { payload }) : post("/api/pin/apply", payload),
  geocode: (q) =>
    isDesktop ? tauriInvoke("review_geocode", { payload: { q } }) : post("/api/geocode", { q }),
};

export const desktop = {
  bootstrap: () => tauriInvoke("desktop_bootstrap"),
  doctor: () => tauriInvoke("desktop_doctor"),
  listJobs: () => tauriInvoke("desktop_list_jobs"),
  openJob: (handle) => tauriInvoke("desktop_open_job", { handle }),
  pickFolder: (title) => tauriInvoke("desktop_pick_folder", { title }),
  ingest: ({ folder, hint, recursive }) =>
    tauriInvoke("desktop_ingest", { folder, hint: hint || null, recursive: Boolean(recursive) }),
  propose: (args) => tauriInvoke("desktop_propose", { args }),
  geocodePass: (args) => tauriInvoke("desktop_geocode_pass", args),
  apply: (args) => tauriInvoke("desktop_apply", { args }),
  undo: (jobId) => tauriInvoke("desktop_undo", { jobId }),
  status: (jobId) => tauriInvoke("desktop_status", { jobId }),
  clean: (jobId) => tauriInvoke("desktop_clean", { jobId }),
  skipLow: (jobId) => tauriInvoke("desktop_skip_low", { jobId }),
  onProgress: (handler) => {
    const ev = window.__TAURI__ && window.__TAURI__.event;
    if (!ev || typeof ev.listen !== "function") return () => {};
    return ev.listen("gpsify://progress", (event) => handler(event.payload));
  },
};

export const thumbUrl = (id) =>
  isDesktop && mediaContext.jobId
    ? `${mediaContext.mediaBase}/thumb/${mediaContext.jobId}/${id}`
    : `/api/thumb/${id}`;

export const previewUrl = (id) =>
  isDesktop && mediaContext.jobId
    ? `${mediaContext.mediaBase}/preview/${mediaContext.jobId}/${id}`
    : `/api/preview/${id}`;
