# Vendored third-party assets

`leaflet.js` / `leaflet.css` — Leaflet 1.9.4, BSD-2-Clause,
(c) 2010-2023 Volodymyr Agafonkin, (c) 2010-2011 CloudMade.
<https://leafletjs.com> · <https://github.com/Leaflet/Leaflet/blob/main/LICENSE>

Vendored so the review UI does not fetch a CDN script on every start. Leaflet's
default marker PNGs are deliberately absent: gpsify draws its pins with
`L.divIcon`, so nothing here requests `images/`. Map tiles still come from
`settings.map_tile_url`.
