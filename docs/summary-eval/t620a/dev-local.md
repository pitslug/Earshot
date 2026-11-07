**Not validated:** the local judge failed its agreement rule (docs/summary-eval.md, T6.20a). These rates never choose between models.

| model | decisions covered | contradicted | missing | actions covered | contradicted | missing |
|---|---|---|---|---|---|---|
| qwen3.5-4b | 85 | 14 | 24 | 29 | 2 | 22 |
| qwen3.5-9b | 82 | 11 | 30 | 29 | 4 | 20 |

| model | list | facts | covered | contradicted | missing | coverage % | contradiction % | net % |
|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b | decisions | 123 | 85 | 14 | 24 | 69.1 | 11.4 | 46.3 |
| qwen3.5-4b | actions | 53 | 29 | 2 | 22 | 54.7 | 3.8 | 47.2 |
| qwen3.5-4b | all | 176 | 114 | 16 | 46 | 64.8 | 9.1 | 46.6 |
| qwen3.5-9b | decisions | 123 | 82 | 11 | 30 | 66.7 | 8.9 | 48.8 |
| qwen3.5-9b | actions | 53 | 29 | 4 | 20 | 54.7 | 7.5 | 39.6 |
| qwen3.5-9b | all | 176 | 111 | 15 | 50 | 63.1 | 8.5 | 46.0 |
