# T12.27 Ornith per-meeting tokens and time (dev, reference source, chunked 600 s, CUDA)

Same meaning as `dev-tokens.md`. Taken on a shared host: the times are not timing figures. Counts and times only.

## ornith-1.5-9b-q4_k_m (cuda, prefix '<think>\n\n</think>\n\n', format indented-numbered-r5-chunk600)
| meeting | ok | joined | prompt tokens read | tokens written | s (all) | s at hang-up | repairs | truncated | looping |
|---|---|---|---|---|---|---|---|---|---|
| ES2003a | True | 0 | 9371 | 1491 | 13.6 | 8.4 | 0 | 0 | 0 |
| ES2003b | True | 0 | 19858 | 2567 | 28.8 | 13.2 | 0 | 0 | 0 |
| ES2003c | True | 0 | 22919 | 6521 | 62.4 | 31.8 | 0 | 0 | 0 |
| ES2003d | True | 0 | 24493 | 4188 | 39.9 | 22.0 | 0 | 0 | 0 |
| ES2011a | True | 0 | 11616 | 1752 | 16.9 | 14.4 | 0 | 0 | 0 |
| ES2011b | True | 0 | 18399 | 4375 | 40.0 | 19.0 | 0 | 0 | 0 |
| ES2011c | True | 0 | 19961 | 5789 | 53.1 | 32.2 | 0 | 0 | 0 |
| ES2011d | True | 0 | 22755 | 4481 | 41.5 | 21.6 | 0 | 0 | 0 |
| IS1008a | True | 0 | 10468 | 3135 | 27.5 | 22.6 | 0 | 0 | 0 |
| IS1008b | True | 0 | 16306 | 4535 | 38.7 | 22.8 | 0 | 0 | 0 |
| IS1008c | True | 0 | 16428 | 3871 | 33.4 | 17.1 | 0 | 0 | 0 |
| IS1008d | True | 0 | 17934 | 4205 | 36.5 | 18.0 | 0 | 0 | 0 |
| TS3004a | True | 0 | 18312 | 4715 | 40.0 | 31.6 | 1 | 1 | 0 |
| TS3004b | True | 0 | 27381 | 6270 | 54.5 | 28.0 | 0 | 0 | 0 |
| TS3004c | True | 0 | 31694 | 5311 | 49.3 | 21.0 | 0 | 0 | 0 |
| TS3004d | True | 0 | 33663 | 6784 | 62.8 | 30.3 | 0 | 0 | 0 |
| TS3006a | True | 0 | 16667 | 2847 | 25.1 | 13.6 | 0 | 0 | 0 |
| TS3006b | True | 0 | 31979 | 5975 | 53.4 | 26.6 | 0 | 0 | 0 |
| TS3006c | True | 0 | 34356 | 5754 | 51.5 | 24.5 | 0 | 0 | 0 |
| TS3006d | True | 0 | 40608 | 4411 | 42.4 | 19.6 | 0 | 0 | 0 |
| mean of 20 | | | 22258 | 4449 | 40.6 | 21.9 | | | |

