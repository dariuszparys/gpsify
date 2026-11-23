# Vendored third-party assets

`leaflet.js` / `leaflet.css` — Leaflet 1.9.4, BSD-2-Clause,
(c) 2010-2023 Volodymyr Agafonkin, (c) 2010-2011 CloudMade.
<https://leafletjs.com>

The full license text is in [`LICENSE-leaflet.txt`](LICENSE-leaflet.txt) next to
this file. BSD-2-Clause requires the notice and disclaimer to travel *with* the
redistribution, so the text is vendored here rather than linked upstream.

Vendored so the review UI does not fetch a CDN script on every start. Leaflet's
default marker PNGs are deliberately absent: gpsify draws its pins with
`L.divIcon`, so nothing here requests `images/`. Map tiles still come from
`settings.map_tile_url`.
