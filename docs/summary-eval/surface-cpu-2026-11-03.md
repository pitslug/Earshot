| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | cpu | dev | reference | 3/3 | 0.142 | 0.231 | 0.246 | 0.054 | 0/16 (0.00 / 0.00) | 0/3 (0.00 / 0.00) | 4/16 (0.25 / 0.31) | 0/3 (0.00 / 0.00) | 7/16 (0.44) | 0/3 (0.00) | 553.3 | 13.8 | 93 | 0.0 | 0.0 | 0.0 |
| qwen3.5-9b-q4_k_m | cpu | dev | reference | 3/3 | 0.151 | 0.249 | 0.267 | 0.069 | 0/16 (0.00 / 0.00) | 0/3 (0.00 / 0.00) | 4/16 (0.25 / 0.40) | 0/3 (0.00 / 0.00) | 9/16 (0.56) | 0/3 (0.00) | 712.8 | 9.5 | 53 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 600 | 10 | 1091.0 | 568.8 | 871.2 | 290.4 | 189.6 | 5496 |
| qwen3.5-9b-q4_k_m | dev | reference | 600 | 10 | 1587.3 | 551.0 | 958.7 | 319.6 | 183.7 | 5496 |
