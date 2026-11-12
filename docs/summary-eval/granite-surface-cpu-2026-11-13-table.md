| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | cpu | dev | reference | 1/1 | 0.162 | 0.237 | 0.248 | 0.060 | 0/3 (0.00 / -) | 0/1 (0.00 / 0.00) | 0/3 (0.00 / -) | 0/1 (0.00 / 0.00) | 0/3 (0.00) | 0/1 (0.00) | 220.3 | 16.9 | 106 | 0.0 | 0.0 | 0.0 |
| granite-4.2-3b-q4_k_m | cpu | dev | reference | 1/1 | 0.147 | 0.209 | 0.225 | 0.031 | 0/3 (0.00 / -) | 0/1 (0.00 / -) | 0/3 (0.00 / -) | 0/1 (0.00 / -) | 0/3 (0.00) | 0/1 (0.00) | 199.8 | 15.3 | 98 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 600 | 2 | 143.1 | 77.2 | 151.3 | 151.3 | 77.2 | 1124 |
| granite-4.2-3b-q4_k_m | dev | reference | 600 | 2 | 144.5 | 55.4 | 127.5 | 127.5 | 55.4 | 1124 |
