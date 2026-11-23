/* Leaflet wrapper. Two markers at most: the pin the write would use, and a
   dashed ghost for the model's own guess when it disagrees with the geocoder. */

const PIN_ICON = () => L.divIcon({ className: "", html: '<div class="pin"></div>', iconSize: [18, 18], iconAnchor: [9, 9] });
const GHOST_ICON = () => L.divIcon({ className: "", html: '<div class="pin ghost"></div>', iconSize: [18, 18], iconAnchor: [9, 9] });

export function createMap(elementId, { tileUrl, attribution, onPick }) {
  const map = L.map(elementId, { zoomControl: true, attributionControl: false });
  L.tileLayer(tileUrl || "https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
    attribution: attribution || "© OpenStreetMap contributors",
    maxZoom: 19,
  }).addTo(map);
  map.setView([20, 0], 2);

  let pin = null;
  let ghost = null;

  map.on("click", (event) => {
    if (onPick) onPick(event.latlng.lat, event.latlng.lng);
  });

  return {
    raw: map,
    /* Leaflet measures the container on init; the inspector is sized by grid
       fractions, so a resize after first paint is required or tiles tile wrong. */
    refresh() {
      map.invalidateSize();
    },
    setPin(lat, lng, { draggable = false, recenter = true, onDrag = null } = {}) {
      if (pin) map.removeLayer(pin);
      pin = L.marker([lat, lng], { icon: PIN_ICON(), draggable, keyboard: false }).addTo(map);
      if (onDrag) {
        pin.on("dragend", () => {
          const at = pin.getLatLng();
          onDrag(at.lat, at.lng);
        });
      }
      if (recenter) map.setView([lat, lng], Math.max(map.getZoom(), 13));
    },
    setGhost(lat, lng) {
      if (ghost) map.removeLayer(ghost);
      ghost = L.marker([lat, lng], { icon: GHOST_ICON(), interactive: false, keyboard: false }).addTo(map);
    },
    setDraggable(draggable) {
      if (!pin) return;
      if (draggable) pin.dragging.enable();
      else pin.dragging.disable();
    },
    clear() {
      if (pin) map.removeLayer(pin);
      if (ghost) map.removeLayer(ghost);
      pin = null;
      ghost = null;
    },
    reset() {
      map.setView([20, 0], 2);
    },
  };
}
