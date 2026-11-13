| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code (prompt brief) | claude-code | dev | reference | 20/20 | 0.185 | 0.311 | 0.336 | 0.073 | 13/83 (0.16 / 0.09) | 7/41 (0.17 / 0.09) | 24/83 (0.29 / 0.17) | 4/41 (0.10 / 0.05) | 44/83 (0.53) | 8/41 (0.20) | 22.0 | 0.0 | 0 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code (prompt brief) | dev | reference | 20/20 | 5520 | 2846 | 55 | 151 | 141 | 74 | 4 | 4 | 4 |
