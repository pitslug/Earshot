# T12.27 dev table, `target/sum-eval/t1227-dev` (as written by `cargo xtask sum-eval`)

Shared host: seconds and tokens per second are not timing figures.

| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | cuda | dev | reference | 20/20 | 0.168 | 0.304 | 0.322 | 0.077 | 10/83 (0.12 / 0.09) | 7/41 (0.17 / 0.10) | 20/83 (0.24 / 0.18) | 7/41 (0.17 / 0.10) | 35/83 (0.42) | 10/41 (0.24) | 36.8 | 152.8 | 8811 | 1.0 | 0.0 | 0.0 |
| granite-4.2-8b-q4_k_m | cuda | dev | reference | 18/20 | 0.106 | 0.202 | 0.216 | 0.048 | 3/83 (0.04 / 0.03) | 2/41 (0.05 / 0.02) | 16/83 (0.19 / 0.16) | 6/41 (0.15 / 0.07) | 32/83 (0.39) | 9/41 (0.22) | 88.5 | 96.0 | 6147 | 12.0 | 3.0 | 0.0 |
| qwen3.5-9b-q4_k_m | cuda | dev | reference | 20/20 | 0.178 | 0.307 | 0.332 | 0.085 | 19/83 (0.23 / 0.14) | 4/41 (0.10 / 0.04) | 24/83 (0.29 / 0.17) | 3/41 (0.07 / 0.03) | 43/83 (0.52) | 6/41 (0.15) | 46.1 | 117.5 | 6445 | 3.0 | 1.0 | 3.0 |
| granite-4.2-3b-q4_k_m | cuda | dev | reference | 19/20 | 0.144 | 0.240 | 0.257 | 0.054 | 1/83 (0.01 / 0.20) | 0/41 (0.00 / 0.00) | 2/83 (0.02 / 0.40) | 0/41 (0.00 / 0.00) | 3/83 (0.04) | 0/41 (0.00) | 11.3 | 216.0 | 13211 | 2.0 | 0.0 | 3.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 600 | 71 | 462.5 | 273.2 | 383.3 | 19.2 | 13.7 | 38355 |
| granite-4.2-8b-q4_k_m | dev | reference | 600 | 71 | 1036.6 | 733.4 | 995.9 | 49.8 | 36.7 | 38355 |
| qwen3.5-9b-q4_k_m | dev | reference | 600 | 71 | 600.4 | 321.7 | 475.8 | 23.8 | 16.1 | 38355 |
| granite-4.2-3b-q4_k_m | dev | reference | 600 | 71 | 172.1 | 53.8 | 103.8 | 5.2 | 2.7 | 38355 |
