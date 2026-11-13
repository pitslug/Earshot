| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code (prompt short) | claude-code | dev | reference | 20/20 | 0.192 | 0.336 | 0.370 | 0.086 | 19/83 (0.23 / 0.12) | 6/41 (0.15 / 0.08) | 28/83 (0.34 / 0.18) | 4/41 (0.10 / 0.05) | 42/83 (0.51) | 9/41 (0.22) | 24.8 | 0.0 | 0 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code (prompt short) | dev | reference | 20/20 | 7420 | 4278 | 61 | 192 | 153 | 75 | 4 | 4 | 4 |
