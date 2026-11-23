/* Review HTTP client. Loopback JSON only; no auth header, no CORS. */

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
  job: () => request("/api/job"),
  photos: (filter) => request(`/api/photos?status=${encodeURIComponent(filter)}`),
  cluster: (id) => request(`/api/clusters/${id}`),
  decideMany: (payload) => post("/api/decisions", payload),
  decide: (photoId, payload) => post(`/api/photos/${photoId}/decision`, payload),
  /* One accepted pin onto a hand-drawn selection. The server re-checks the K9
     caps; confirm_n is what the human typed when they trip. */
  applyPin: (payload) => post("/api/pin/apply", payload),
  geocode: (q) => post("/api/geocode", { q }),
};

export const thumbUrl = (id) => `/api/thumb/${id}`;
export const previewUrl = (id) => `/api/preview/${id}`;
