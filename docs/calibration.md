# Score calibration

Raw cosine similarities of EmbeddingGemma 2 are compressed. Measured on a 24-document text corpus
(12 queries with a known best document, 2 queries without a match; `normal` quality, CPU):

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
