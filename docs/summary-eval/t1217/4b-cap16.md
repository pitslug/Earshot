| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | cuda | dev | reference | 20/20 | 0.166 | 0.301 | 0.319 | 0.077 | 10/83 (0.12 / 0.08) | 7/41 (0.17 / 0.10) | 20/83 (0.24 / 0.17) | 7/41 (0.17 / 0.10) | 35/83 (0.42) | 10/41 (0.24) | 29.8 | 191.4 | 10213 | 1.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 20/20 | 12117 | 8647 | 152 | 473 | 118 | 70 | 8 | 1 | 1 |

| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |
|---|---|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b-q4_k_m | dev | reference | 600 | 71 | 373.5 | 223.2 | 313.8 | 15.7 | 11.2 | 38355 |
