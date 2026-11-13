| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | claude-code | dev | reference | 20/20 | 0.156 | 0.277 | 0.298 | 0.089 | 15/83 (0.18 / 0.11) | 7/41 (0.17 / 0.10) | 28/83 (0.34 / 0.21) | 8/41 (0.20 / 0.12) | 42/83 (0.51) | 16/41 (0.39) | 32.0 | 0.0 | 0 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | dev | reference | 20/20 | 14298 | 11164 | 99 | 397 | 136 | 69 | 5 | 5 | 5 |
