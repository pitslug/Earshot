| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m (prompt brief) | cuda | dev | reference | 20/20 | 0.191 | 0.318 | 0.340 | 0.078 | 9/83 (0.11 / 0.08) | 7/41 (0.17 / 0.11) | 21/83 (0.25 / 0.20) | 4/41 (0.10 / 0.06) | 34/83 (0.41) | 6/41 (0.15) | 58.2 | 62.9 | 3658 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m (prompt brief) | dev | reference | 20/20 | 5944 | 3027 | 66 | 184 | 107 | 65 | 12 | 1 | 1 |

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 600 | 71 | 780.7 | 382.6 | 584.6 | 29.2 | 19.1 | 38355 |
