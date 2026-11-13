| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | claude-code | dev | reference | 20/20 | 0.153 | 0.274 | 0.297 | 0.087 | 17/83 (0.20 / 0.12) | 7/41 (0.17 / 0.10) | 25/83 (0.30 / 0.18) | 6/41 (0.15 / 0.09) | 40/83 (0.48) | 11/41 (0.27) | 35.1 | 0.0 | 0 | 1.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | dev | reference | 20/20 | 14235 | 11134 | 96 | 401 | 142 | 70 | 3 | 3 | 3 |
