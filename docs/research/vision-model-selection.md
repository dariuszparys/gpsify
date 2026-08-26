# Vision model selection for gpsify

| Field | Value |
| --- | --- |
| **Date** | 2026-08-25 |
| **Status** | Partial — no single public board ranks every candidate on the same images |
| **Question** | What is the best and cheapest vision model for gpsify? Would self-hosted work? |
| **Workload** | Resized orientation-corrected JPEG preview + structured `VisionGuess` JSON; geocode the place name; refuse private/indoor/generic scenes |

v1 ships SpaceXAI `grok-4.6` only (design K2). This note compares that default to public geolocation benches and list prices. It is not a measured Grok bake-off: `grok-4.6` is absent from IMAGEO-Bench, EarthWhere, GeoArena, and GeoRC.

---

## Recommendation

**Gemini 2.5 Flash** is the best cheap hosted pick for this app. **Gemini 2.5 Pro** is the most accurate on public photo-geolocation benches. Self-hosting can name city and country on typical travel photos; it is not a street-level substitute for the current Grok path.

1. Keep Grok if one provider is enough.
2. Add Gemini 2.5 Flash next if the goal is cheaper with similar-or-better geo quality (same schema, roughly 3–8× lower list cost, second on the benches). Use Pro only for the accuracy ceiling at Grok-like spend.
3. Do not switch the default to Flash-Lite, GPT-5.6 luna, or a 4B local model without a bake-off on known photos. The documented gap is geo reasoning, not JSON emission.
4. Self-host Qwen3-VL-30B only if photos must stay on-machine and city-level is enough. Pair it with Nominatim (already in the pipeline) rather than trusting local lat/lng.

---

## Headline

Gemini 2.5 Pro is the strongest vision model for gpsify-style photo geolocation, ranking first on EarthWhere, GeoArena, and GeoRC, with Gemini 2.5 Flash a close second. The cheapest hosted VLMs that can return structured JSON from a resized JPEG are Gemini 2.5 Flash-Lite, GPT-5.6 luna, and Qwen3-VL-30B-A3B-Instruct on OpenRouter — all well under a tenth of a cent per photo, versus gpsify’s grok-4.6 prior of about $0.006–$0.015 plus billed reasoning tokens. Self-hosting can work for city-and-country guesses on typical travel photos, but smaller open weights are not shown to match closed frontier models on street-level sign and architecture localization.

---

## Best accuracy

On IMAGEO-Bench street photos (Dataset-GSS, 6,152 images), closed Gemini 2.5 Pro leads administrative accuracy; cheaper open VLMs on the same task trail by a wide margin, which is the quality prior gpsify cites.

| Model | GSS city | GSS country | GSS median | UPC city / state | IMAGEO $ |
| --- | --- | --- | --- | --- | --- |
| gemini-2.5-pro | 44.1% | 92.4% | 4.2 km | 26.4% / 49.5% | $0.00556 |
| gemini-2.5-flash | 41.7% | 91.2% | 6.0 km | — | — |
| o3 | 42.0% | 88.7% | 8.2 km | 23.9% / 45.8% | — |
| llama-4-17b | 25.7% | 70.0% | 127.0 km | — | $0.00093 |
| llama-3.2-11b | 24.0% | 66.7% | 66.6 km | — | $0.00015 |

Qwen3-VL is absent from that 10-model table. Success there tracks urban, outdoor, street, and landmark scenes.

EarthWhere (13 VLMs; country panoramas plus street images, optional web search) has Gemini 2.5 Pro at 56.32% overall and 68.4% on WhereCountry without web; with web it leads WhereStreet Acc@1km at 6.38% (Bilibili) and 65.63% (YouTube). The strongest open-weight model, GLM-4.5V, reaches 34.71%; remaining open-weight WhereCountry baselines average 19.57%, with most others near chance (18.50%). Valid clues in that benchmark are visual features such as road markings and signage language.

GeoArena’s September 2025 human-preference leaderboard ranks Gemini 2.5 Pro first (Elo 1319.7) and Gemini 2.5 Flash second (1206.5). Qwen 2.5 VL 72B is the highest open-source model (1094.5), ahead of Gemma 3 12B (1086.5) and comparable to the GPT 4.1 series; GPT 4.1, Llama 4 Maverick, and Claude Opus 4 cluster around Elo 1040–1050 with overlapping confidence intervals. Hard-case ranking after the two Geminis is Qwen 2.5 VL 72B, Gemma 3 12B, then Gemma 3 27B.

On GeoRC GeoGuessr scenes, closed VLMs rival human experts on country prediction but lag on auditable reasoning chains; open-weight models sit far lower on both country accuracy and reasoning-chain F1 (Qwen3-VL-8B F1 22.41 vs a human average of 56.69). Compute limits excluded Qwen3-VL-235B.

| Model | Country accuracy |
| --- | --- |
| Human Expert #3 (RC) | 97.33% |
| Other human experts | 96.67% / 90.00% |
| Gemini-2.5-Pro | 91.20% |
| Gemini-3-Pro | 90.40% |
| GPT-5 | 88.40% |
| Gemini-2.5-Flash | 88.00% |
| GPT-4.1 | 77.80% |
| Qwen2.5-VL-7B | 69.40% |
| Qwen3-VL-8B-Instruct | 64.40% |
| Gemma-3-12b-it | 64.00% |
| Llama-3.2-11B | 58.40% |

A 25-VLM study that did **not** include Gemini 2.5 found GPT-4.1 strongest on GPTGeoChat social-media-like photos (Recall@1km 61%, vs Claude-3.7-Sonnet 60% and Gemini-1.5-Pro 50%), with the highest recall at 1/25/200/750 km; mean city/country accuracy across that study’s datasets was led by GPT-o3 (41.3%/77.5%). OpenStreetView-5 was much harder (GPT-4.1 Recall@100km 11.6%).

Street-level pins stay rare even for the leaders. That matches gpsify’s design: name a place, geocode it, never trust model lat/lng alone.

---

## Cheapest hosted APIs

For a 1280×853-class JPEG plus ~600 JSON output tokens (Gemini figures include thinking), per-image cost and list rates are:

| Model | $ / 1M in / out | ~Image-only | ~Image + ~600 JSON | Batch |
| --- | --- | --- | --- | --- |
| Gemini 2.5 Flash-Lite | $0.10 / $0.40 | — | ~$0.00034 | 50% off |
| Qwen3-VL-30B-A3B-Instruct (OpenRouter) | $0.13 / $0.52 | ~$0.00016 | ~$0.00047 | — |
| gpt-5.6-luna | $0.20 / $1.20 | — | ~$0.0010 | 50% off |
| Gemini 2.5 Flash | $0.30 / $2.50 | — | ~$0.0018 | 50% off |
| Gemini 3.5 Flash-Lite | $0.30 / $2.50 | — | ~$0.0018 | 50% off |
| Gemini 3.7 Flash (promo) | $0.75 / $3.75 → **$1.50 / $7.50 on 1 Jan 2027** | — | ~$0.0030 → ~$0.0060 | 50% off |
| Gemini 3.6 Flash (promo) | $0.75 / $3.75 → **$1.50 / $7.50 on 1 Jan 2027** | — | ~$0.0030 → ~$0.0060 | 50% off |
| Gemini 3.5 Flash | $1.50 / $9.00 | — | ~$0.0069 | 50% off |
| Claude Haiku 4.5 | $1 / $5 | ~$0.00143 | ~$0.0044 | 50% off |
| grok-4.6 | $2 / $6 (cached in $0.50; rates double at ≥200k) | — | ~$0.006–$0.015 plus reasoning; ~$1.20–$3.00+ / 200 photos; ~$12–$30+ / 2,000 | Not supported |
| Gemini 2.5 Pro (≤200k) | $1.25 / $10 | — | ~$0.0073 | 50% off |
| GPT-4o high-detail | $2.50 / $10 | — | ~$0.0088 | 50% off |
| gpt-5.6-terra | $2 / $12 | — | ~$0.010 | 50% off |
| gpt-5.6-sol (promo through 21 Nov 2026) | $4 / $20 | — | ~$0.017 | 50% off |
| Claude Opus 5 | $5 / $25 | ~$0.00713 | ~$0.022 | 50% off |
| Claude Sonnet 5 | $2 / $10 | — | — | 50% off |

Gemini 2.5 Flash is the highest-ranked model among those sub-cent hosted options. Gemini 2.5 Pro is in the same price band as grok-4.6 on this workload while topping the geolocation leaderboards above.

**Gemini rates re-verified 2026-08-25** against [ai.google.dev/gemini-api/docs/pricing](https://ai.google.dev/gemini-api/docs/pricing) (paid tier). Two things the earlier snapshot missed:

- **3.6 and 3.7 Flash are on promotional launch pricing that doubles on 1 January 2027** ($0.75/$3.75 → $1.50/$7.50). After the rollover, 3.7 Flash costs *more* per photo than `grok-4.6` on this workload once thinking tokens are counted, which removes the cost argument for switching to it. `estimate_usd` encodes the rollover date.
- **Gemini 3.5 Flash-Lite lists at $0.30 / $2.50 — identical to Gemini 2.5 Flash.** That makes it a more interesting generation-3 candidate than 3.7 Flash: newer weights at the current default's exact price. Neither has a published geolocation score, so it is a bake-off question, not a reading question.

### Image-token math

- **Claude** counts ⌈width/28⌉×⌈height/28⌉ visual tokens (1,426 for 1280×853, under the 1568 px standard / 2576 px Claude 4.7+ long-edge caps).
- **GPT-5.6** original/auto uses 1,080 32px patches × 1.2 = 1,296 image tokens; GPT-4o high-detail is 1,105 tokens.
- **Gemini** bills images at the text input rate: ≤384 px on both sides = 258 tokens, otherwise 258 per 768×768 tile (1280×853 → 1,032).
- **Fireworks** documents 1,196 image tokens for a 1280×720 Qwen2.5-VL frame.
- **xAI** image-tile math is unpublished. gpsify’s $0.006–$0.015 band is an explicit guess from published grok-4.6 text rates plus typical image+JSON+reasoning size.
- OpenRouter Qwen3-VL-30B-A3B-Instruct supports JSON-schema structured outputs.

Frontier models bill reasoning/thinking (grok-4.6 defaults to high; Gemini output price includes thinking). Those tokens can exceed the 400–800 JSON schema. Aspect ratio at a 1280 px long edge changes tile/patch counts, so per-image input tokens move with orientation.

---

## Self-hosted

Local Qwen3-VL 30B named city and country correctly on 91% of 21,236 public travel photos (Gemma3 27B: 87%), with human review of labels and edge cases; country was almost always right when city was missed, and landmarks/tourist scenes were easier than generic beaches, rural roads, or hotel rooms. Those runs were on local machines, not public APIs.

Apple-Silicon-ready Qwen3-VL weights exist from edge to cloud (dense 2B/4B/8B plus 32B, and MoE 30B-A3B):

| Build | Size |
| --- | --- |
| Qwen3-VL-4B Instruct 8-bit MLX | 5.1 GB |
| Qwen3-VL-30B-A3B Q3_K_M GGUF | 14.7 GB |
| Qwen3-VL-30B-A3B Q4_K_M GGUF | 18.6 GB |
| Qwen3-VL-30B-A3B Q8_0 GGUF | 32.5 GB |
| Qwen3-VL-30B-A3B 8-bit MLX | 33.5 GB |

Zero-shot unconstrained country-name prediction (not city or landmark names, no GPS matching) for Mac-feasible dense Qwen3-VL-2B/4B/8B peaks at Qwen3-VL-4B: 74.79% Top-1 on GeoGuessr-50k, 65.78% on CityGuessr, 45.45% on OSV5M (2B: 73.86% / 64.57% / 46.50%; 8B: 67.30% / 65.31% / 41.78%, inverted scaling). On GeoGuessr-50k that 4B model is 83.8% urban vs 68.5% rural Top-1.

On MAPBench GPS-distance thresholds, unaugmented Qwen3-VL-30B-A3B reaches Acc@25km 71.95% easy / 14.20% hard and Acc@750km 83.31% easy, but street-scale Acc@500m is 4.01% easy / 0.21% hard — below Gemini-3-Pro with Google Search/Map (easy 80.69% / 20.86% / 93.79%; hard Acc@25km 29.64%, Acc@500m 4.02%). Qwen3-VL-235B-A22B easy Acc@25km is 88.48%. Adding map tools then RL raises Acc@500m far above the unaugmented 30B baseline (easy 4.01% → 41.51% after RL; 44.98% with parallel×4).

30B Q4 generally needs 24 GB+ unified memory; 8-bit MLX needs ~36 GB+. The 4B 8-bit MLX (5.1 GB) fits an 8–16 GB Mac. Quantized 4B/8B Mac inference quality versus full-precision 30B geolocation accuracy is not reported in the inspected sources.

---

## Fit for gpsify

v1 propose must call a vision model (not a text-only LLM) with a resized, orientation-corrected JPEG preview plus optional album/user hints, then geocode the place. Each call requests strict JSON Schema for the `VisionGuess` contract (refusal, place, clues, candidates, confidence). The prompt requires multilingual visual reasoning from signs, language/script, architecture, vegetation, road markings, vehicles, and skyline, and requires refusal on private homes, hotel rooms, generic indoor scenes, and close-ups with no geographic signal. A `privacy_sensitive` or `indoor_generic` (or `policy_refusal` / `insufficient_evidence` / `not_a_photograph`) refusal is a hard stop: those strings are never geocoded.

That clue set is exactly where closed–open gaps are largest. Self-hosted Qwen3-VL is a workable cheap path for coarse city/country on tourist-like photos; it is not shown to deliver gpsify-class street-level sign/architecture pins without extra map tools, and the smallest local dense sizes are country-name predictors with a large urban–rural gap.

gpsify now ships that bake-off as `scripts/bakeoff.py`: it scores any model against photos whose GPS you already have, so the answer is measured on your own album rather than inferred from the tables above. What follows is the prior it replaces. Local VLM and cheap indoor-triage models are deferred for setup and second-provider cost, not a failed capability test. A small bake-off on a known album (Flash vs Grok, record real `usage`) would beat any of these papers for this repo.

---

## Coverage and uncertainty

- No single public leaderboard jointly ranks Gemini 2.5/3, GPT-5, Qwen3-VL 30B, Grok, and GPT-4.1 on the same images, so “highest” is study-specific.
- IMAGEO-Bench gemini-1.5-pro had a lower Dataset-GSS median distance (3.5 km) than gemini-2.5-pro (4.2 km) despite worse country/city accuracy.
- Grainge et al. (GPTGeoChat/OSV5) omitted Gemini 2.5; McAfee tested only two local open models on travel-biased photos.
- GeoArena ranks human preference, not GPS error, and style (longer answers) influences votes.
- Street-level precision stays low even for top models (e.g. EarthWhere Bilibili Acc@1km 6.38% for Gemini-2.5-pro with web).
- gpsify’s default Grok provider is not scored on IMAGEO-Bench, EarthWhere, GeoArena, or GeoRC in the sources inspected.
- **No Gemini 3.x Flash model has a published geolocation score in any source here.** The only generation-3 model that is scored is Gemini-3-Pro, and on GeoRC it lands *below* Gemini-2.5-Pro (90.40% vs 91.20%) — one benchmark, Pro-vs-Pro, but the only within-family generational comparison available, and it does not favour the newer generation. Do not assume a 3.x Flash beats 2.5 Flash at this task without measuring it.
- xAI’s official Image Understanding page documents `detail=high` and jpeg/png input but does not publish a tile/token formula, so grok-4.6 dollars per 1280px JPEG cannot be computed from primary docs; gpsify’s $0.006–$0.015 band is an explicit guess.
- Frontier models bill reasoning/thinking (grok-4.6 defaults to high; Gemini output price includes thinking). Those tokens can exceed the 400–800 JSON schema and are not included in the Luna/Flash-Lite/Haiku arithmetic above.
- Aspect ratio at a 1280px long edge changes tile/patch counts (e.g. 16:9 vs square), so per-image input tokens move with orientation.
- GPT-5.6 auto/original preserves native dimensions and can cost more than high on larger photos; the 1,296-token figure is for original/auto on a 1280×853 image, not a resized high-detail crop.
- Gemini 3.x might use a different image tokenizer than the 258-token/768px rule still stated on the 2026-08-17 tokens page; if so, Flash-Lite/Flash dollar estimates would change.
- A 200–2,000 photo job is linear per request: OpenAI/Gemini/Anthropic Batch is 50% off where used; grok-4.6 has no Batch API. Prompt-cache hits need a shared prefix and do not discount the unique JPEG.
- McAfee did not report Mac vs other hardware, RAM, quantization, landmark-vs-city name split, or a cloud-model bake-off on the same 21,236 travel images; 91% is travel-biased and not comparable 1:1 to IMAGEO’s 44% city accuracy on Mapillary-style street scenes.
- No inspected source measured Qwen3-VL city/landmark name accuracy on a typical 8–16 GB personal Mac; 4B 8-bit MLX (5.1 GB) fits that class, while 30B Q4_K_M (18.6 GB) and 8-bit MLX (33.5 GB) generally need 24 GB+ / ~36 GB+ unified memory.
- Most academic metrics are country labels or Acc@km, not “usable city/landmark place names”; MAPBench Acc@25km is a GPS radius, not a named-place score.
- Qwen’s official card claims upgraded landmark recognition qualitatively, without a place-name accuracy number.
- EarthWhere reports Gemini-2.5-Pro 56.32% overall vs strongest open-weight GLM-4.5V 34.71% on a harder country/street mix; Qwen3-VL-235B is listed as a baseline but full Qwen3-VL numbers were not extracted from truncated tables.
- No inspected source shows that cheapest models specifically cannot emit gpsify’s VisionGuess schema or set `privacy_sensitive` / `indoor_generic`; the documented quality gap is geo-reasoning accuracy, not schema or refusal compliance.

---

## Sources

- IMAGEO-Bench — [arXiv:2508.01608v2](https://arxiv.org/html/2508.01608v2)
- EarthWhere — [arXiv:2510.10880v1](https://arxiv.org/html/2510.10880v1)
- GeoArena — [arXiv:2509.04334v3](https://arxiv.org/html/2509.04334v3)
- GeoRC — [arXiv:2601.21278v2](https://arxiv.org/html/2601.21278v2)
- Grainge et al., geolocation capabilities of generative VLMs — [arXiv:2508.19967](https://arxiv.org/html/2508.19967)
- McAfee Labs Safer Summer Travel Report (local Qwen3-VL 30B 91%) — [McAfee blog](https://www.mcafee.com/blogs/mcafee-news/ai-travel-photo-location-scams-geolocation-research/)
- Grok 4.6 pricing — [docs.x.ai](https://docs.x.ai/developers/models/grok-4.6)
- gpsify decisions and schema — [`docs/decisions.md`](../decisions.md)
- Claude vision token math — [Claude docs](https://platform.claude.com/docs/en/build-with-claude/vision)
- OpenAI images and vision — [OpenAI API](https://developers.openai.com/api/docs/guides/images-vision)
- Gemini Developer API pricing — [ai.google.dev](https://ai.google.dev/gemini-api/docs/pricing)
- Qwen3 VL 30B A3B Instruct — [OpenRouter](https://openrouter.ai/qwen/qwen3-vl-30b-a3b-instruct)
- mlx-community/Qwen3-VL-4B-Instruct-8bit and unsloth GGUF — [Hugging Face](https://huggingface.co/mlx-community/Qwen3-VL-4B-Instruct-8bit)
- Where Do Vision-Language Models Fail? — [arXiv:2604.16248v1](https://arxiv.org/html/2604.16248v1)
- Thinking with Map — [arXiv:2601.05432](https://arxiv.org/html/2601.05432)
- gpsify xAI wrapper and prompt — `src/gpsify/providers/xai_grok.py`
- gpsify refusal gate — `src/gpsify/reconcile.py`
