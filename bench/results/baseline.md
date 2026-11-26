# aoloxide vs axum: 2026-11-26T10:11Z

* Host: 4 CPUs, Intel(R) Xeon(R) Processor @ 2.10GHz, Linux 6.18.44-fc-v880
* Servers pinned to CPUs 0-1, wrk (2 threads) to CPUs 2-3
* 10s per point after 2s warmup; HTTP/1.1 keep-alive; aoloxide conn threads: 20
* aoloxide 2848819, wrk debian/4.1.0-4build2, rustc 1.97.0 (2d8144b78 2026-07-07)
* Peak RSS: aoloxide 36 MiB, axum 34 MiB, axum-cached 12 MiB

## Throughput

| scenario | conns | aoloxide req/s | axum req/s | axum ÷ aoloxide | axum-cached req/s | axum-cached ÷ aoloxide |
|---|---:|---:|---:|---:|---:|---:|
| hello | 16 | 31,999 | 135,565 | 4.24× | — | — |
| hello | 64 | 32,198 | 152,596 | 4.74× | — | — |
| hello | 256 | 32,800 | 167,553 | 5.11× | — | — |
| static-1k | 16 | 43,616 | 14,985 | 0.34× | 128,463 | 2.95× |
| static-1k | 64 | 45,294 | 14,730 | 0.33× | 140,593 | 3.10× |
| static-1k | 256 | 43,597 | 13,127 | 0.30× | 145,503 | 3.34× |
| static-64k | 16 | 35,019 | 11,918 | 0.34× | 85,607 | 2.44× |
| static-64k | 64 | 37,035 | 11,951 | 0.32× | 95,541 | 2.58× |
| static-64k | 256 | 37,222 | 10,305 | 0.28× | 97,900 | 2.63× |
| adp-page | 16 | 18,922 | 120,519 | 6.37× | — | — |
| adp-page | 64 | 20,306 | 140,738 | 6.93× | — | — |
| adp-page | 256 | 19,684 | 145,605 | 7.40× | — | — |

## Latency, server CPU per request, errors

| scenario | conns | server | p50 | p90 | p99 | max | CPU µs/req | errors |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| hello | 16 | aoloxide | 490 µs | 726 µs | 1.50 ms | 29.63 ms | 61.0 | 0 |
| hello | 16 | axum | 110 µs | 173 µs | 411 µs | 8.25 ms | 14.0 | 0 |
| hello | 64 | aoloxide | 1.96 ms | 3.08 ms | 4.42 ms | 26.24 ms | 60.9 | 0 |
| hello | 64 | axum | 401 µs | 607 µs | 1.09 ms | 22.27 ms | 12.6 | 0 |
| hello | 256 | aoloxide | 7.17 ms | 12.79 ms | 19.48 ms | 35.92 ms | 60.2 | 0 |
| hello | 256 | axum | 1.47 ms | 2.23 ms | 4.01 ms | 13.84 ms | 11.6 | 0 |
| static-1k | 16 | aoloxide | 354 µs | 548 µs | 1.57 ms | 11.10 ms | 44.8 | 0 |
| static-1k | 16 | axum | 1.01 ms | 1.61 ms | 3.15 ms | 13.25 ms | 120.5 | 0 |
| static-1k | 16 | axum-cached | 117 µs | 181 µs | 797 µs | 16.81 ms | 14.7 | 0 |
| static-1k | 64 | aoloxide | 1.39 ms | 2.19 ms | 3.31 ms | 19.78 ms | 43.3 | 0 |
| static-1k | 64 | axum | 4.22 ms | 6.40 ms | 9.16 ms | 24.29 ms | 124.4 | 0 |
| static-1k | 64 | axum-cached | 443 µs | 643 µs | 1.18 ms | 10.69 ms | 13.6 | 0 |
| static-1k | 256 | aoloxide | 5.62 ms | 9.18 ms | 14.73 ms | 48.73 ms | 45.0 | 0 |
| static-1k | 256 | axum | 19.10 ms | 27.91 ms | 36.57 ms | 53.40 ms | 135.6 | 0 |
| static-1k | 256 | axum-cached | 1.72 ms | 2.57 ms | 4.16 ms | 27.31 ms | 13.4 | 0 |
| static-64k | 16 | aoloxide | 437 µs | 672 µs | 1.91 ms | 20.85 ms | 56.0 | 0 |
| static-64k | 16 | axum | 1.29 ms | 1.98 ms | 3.48 ms | 62.95 ms | 155.0 | 0 |
| static-64k | 16 | axum-cached | 166 µs | 261 µs | 526 µs | 22.88 ms | 22.4 | 0 |
| static-64k | 64 | aoloxide | 1.69 ms | 2.68 ms | 4.00 ms | 20.53 ms | 52.9 | 0 |
| static-64k | 64 | axum | 5.27 ms | 7.67 ms | 10.82 ms | 35.71 ms | 158.9 | 0 |
| static-64k | 64 | axum-cached | 613 µs | 897 µs | 1.38 ms | 19.16 ms | 20.1 | 0 |
| static-64k | 256 | aoloxide | 6.40 ms | 10.99 ms | 17.28 ms | 51.20 ms | 52.3 | 0 |
| static-64k | 256 | axum | 24.49 ms | 36.69 ms | 56.33 ms | 130.82 ms | 181.5 | 0 |
| static-64k | 256 | axum-cached | 2.31 ms | 3.42 ms | 4.91 ms | 20.60 ms | 19.7 | 0 |
| adp-page | 16 | aoloxide | 829 µs | 1.22 ms | 2.59 ms | 19.88 ms | 105.1 | 0 |
| adp-page | 16 | axum | 125 µs | 197 µs | 402 µs | 7.11 ms | 15.9 | 0 |
| adp-page | 64 | aoloxide | 3.02 ms | 4.88 ms | 8.39 ms | 55.13 ms | 97.8 | 0 |
| adp-page | 64 | axum | 439 µs | 656 µs | 1.10 ms | 16.89 ms | 13.7 | 0 |
| adp-page | 256 | aoloxide | 11.68 ms | 21.42 ms | 35.24 ms | 81.87 ms | 100.0 | 0 |
| adp-page | 256 | axum | 1.73 ms | 2.53 ms | 3.91 ms | 15.38 ms | 13.4 | 0 |
