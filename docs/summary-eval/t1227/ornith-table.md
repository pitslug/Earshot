# T12.27 Ornith dev table, `target/sum-eval/t1227-ornith` (as written by `cargo xtask sum-eval`)

Shared host: seconds and tokens per second are not timing figures.

| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ornith-1.5-9b-q4_k_m | cuda | dev | reference | 20/20 | 0.153 | 0.260 | 0.280 | 0.094 | 6/83 (0.07 / 0.04) | 6/41 (0.15 / 0.06) | 15/83 (0.18 / 0.11) | 5/41 (0.12 / 0.05) | 28/83 (0.34) | 11/41 (0.27) | 40.6 | 119.9 | 6556 | 1.0 | 1.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| ornith-1.5-9b-q4_k_m | dev | reference | 600 | 71 | 470.1 | 341.1 | 438.2 | 21.9 | 17.1 | 38355 |
