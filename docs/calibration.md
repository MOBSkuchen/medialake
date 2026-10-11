# Score calibration

Raw cosine similarities of EmbeddingGemma 2 are compressed. Measured on a 24-document text corpus
(12 queries with a known best document, 2 queries without a match; `normal` quality, CPU; on macOS, Metal and CPU agree to the third decimal on the bench media, see `plan2.md` §10):

| | raw cosine |
|---|---|
| top-1 hit, relevant (12/12 correct) | 0.65 – 0.83, median ~0.70 |
| best irrelevant hit | 0.52 – 0.64, usually ≤ 0.60 |
| queries without any match | best hit 0.60 / 0.61 |
| audio matches (dog bark, rain) | 0.76 / 0.66 |
| image / video matches | 0.74 – 0.75 / 0.72 |

Mapping used (`config.toml`: `score_floor`, `score_ceiling`, `threshold`, `relative_cut`):

* confidence = clamp((raw − 0.58) / (0.74 − 0.58), 0, 1)
* results below `threshold` (default 0.4, user-adjustable) are hidden
* results more than `relative_cut` (0.06 raw) below the best result are hidden, which removes the
  "least bad" filler hits when there is one clear match

Re-run the evaluation after changing model, quantisation or the prompt prefixes.

## Qwen3-VL-Embedding

The mapping is a per-model default (`Model::calibration` in `medialake-proto`); switching the model switches it, unless
the values in `config.toml` were tuned by hand.

Raw cosines of Qwen3-VL-Embedding are spread much wider than EmbeddingGemma's. Measured with `qwen3-vl-embedding-2b`,
`normal` quality, Metal, on a 24-document text corpus plus 2 photos (16 text queries and 2 image queries with a known
best file, 4 queries without a match):

| | raw cosine |
|---|---|
| top-1 hit, relevant text (16/16 correct) | 0.59 – 0.78, median ~0.69 |
| best irrelevant hit | 0.32 – 0.52, median ~0.43 |
| queries without any match | best hit 0.34 – 0.43 |
| image matches ("red sports car", "a photo of a cat") | 0.50 / 0.43 |

Mapping: confidence = clamp((raw − 0.38) / (0.60 − 0.38), 0, 1), `relative_cut` 0.12. With the default `threshold`
of 0.4 a hit needs a raw cosine of 0.47.

Text-to-image similarities sit well below text-to-text ones, so a weak image match (the cat photo, 0.43) is in the
same range as the best hit of a query without any match and is hidden at the default threshold.

`qwen3-vl-embedding-8b` uses the same values but has not been measured.
