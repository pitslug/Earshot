| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | claude-code | dev | reference | 5/5 | 0.181 | 0.285 | 0.316 | 0.071 | 8/22 (0.36 / 0.17) | 1/11 (0.09 / 0.07) | 8/22 (0.36 / 0.17) | 0/11 (0.00 / 0.00) | 12/22 (0.55) | 1/11 (0.09) | 21.2 | 0.0 | 0 | 0.0 | 0.0 | 0.0 |

* Reference recall: a reference item counts as caught when some summary item holds at least half of its content tokens (defined after seeing dev data; `earshot_core::metrics::items`).
** Coverage: a reference item counts as caught when all of the summary's items of that list together hold at least half of its content tokens (many-to-one; defined after seeing data; `items::covered_items`).

| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | dev | reference | 5/5 | 1137 | 592 | 12 | 15 | 48 | 15 | 1 | 1 | 1 |
