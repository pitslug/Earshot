**Not validated:** the local judge failed its agreement rule (docs/summary-eval.md, T6.20a). These rates never choose between models.

| model | decisions covered | contradicted | missing | actions covered | contradicted | missing |
|---|---|---|---|---|---|---|
| qwen3.5-4b | 106 | 22 | 31 | 22 | 5 | 15 |
| qwen3.5-9b | 101 | 20 | 38 | 21 | 5 | 16 |

| model | list | facts | covered | contradicted | missing | coverage % | contradiction % | net % |
|---|---|---|---|---|---|---|---|---|
| qwen3.5-4b | decisions | 159 | 106 | 22 | 31 | 66.7 | 13.8 | 39.0 |
| qwen3.5-4b | actions | 42 | 22 | 5 | 15 | 52.4 | 11.9 | 28.6 |
| qwen3.5-4b | all | 201 | 128 | 27 | 46 | 63.7 | 13.4 | 36.8 |
| qwen3.5-9b | decisions | 159 | 101 | 20 | 38 | 63.5 | 12.6 | 38.4 |
| qwen3.5-9b | actions | 42 | 21 | 5 | 16 | 50.0 | 11.9 | 26.2 |
| qwen3.5-9b | all | 201 | 122 | 25 | 54 | 60.7 | 12.4 | 35.8 |
