# First Discord benchmark

A confirmed 20 MB upload took 3.95 s on average. Downloads took 0.74 s on the
first pass and 0.27 s on the second, including TCP/TLS connection setup.
Across the ten files, effective throughput was 5.06 MB/s for uploads,
26.90 MB/s for first downloads and 75.31 MB/s for repeated downloads.
These figures describe this host and this run.

## Environment and workload

The host information was supplied with the results:

| Item | Configuration |
| --- | --- |
| Operating system | Linux x86_64, NixOS 26.05 |
| Rust | 1.95.0 |
| CPU | AMD Ryzen 5 3600 |
| Memory | 32 GB DDR4 |
| Connection | 1,000 Mbps download, 500 Mbps upload |

The [recorded settings](../benches/bench1/settings.json) specify one round,
ten files of 20,000,000 bytes, a two-second pause between uploads and a
180-second timeout per operation. The dataset contains 120 phase observations:
ten completed uploads and twenty completed downloads. Each file was downloaded
once for the first pass, then immediately again for the second pass. Transfers
were sequential, each using a fresh TCP/TLS connection. Download timing includes
byte-for-byte content verification with a reused buffer.

All rates use decimal MB/s. The nominal connection corresponds to 125 MB/s down
and 62.5 MB/s up; those are line-rate conversions, not measured application limits.

## Complete transfer latency

The following totals sum the measured phases for each file. Upload totals
include connection setup, open, writing and `finish`. Download totals include
connection setup, open and reading through EOF. Scheduled pauses, reporting and
resource-probe overhead between phases are excluded.

| Operation | Mean ± sigma, s | p50, s | p90, s | p95/p99, s | Min–max, s |
| --- | --- | --- | --- | --- | --- |
| Confirmed upload | 3.950 ± 0.440 | 3.718 | 4.353 | 5.109 | 3.620–5.109 |
| First download | 0.743 ± 0.115 | 0.717 | 0.804 | 1.022 | 0.545–1.022 |
| Second download | 0.266 ± 0.006 | 0.262 | 0.273 | 0.279 | 0.260–0.279 |

Each row has ten observations. Sigma is the population standard deviation.
Percentiles use nearest rank, so p95 and p99 both equal the maximum for this run.
They do not provide a reliable estimate of rare latency spikes.

## Complete transfer throughput

These distributions are calculated as file size divided by each file's total
transfer time above.

| Operation | Mean ± sigma, MB/s | p50 | p90 | p95/p99 | Min–max |
| --- | --- | --- | --- | --- | --- |
| Confirmed upload | 5.12 ± 0.48 | 5.31 | 5.45 | 5.52 | 3.91–5.52 |
| First download | 27.52 ± 4.10 | 27.59 | 29.94 | 36.71 | 19.57–36.71 |
| Second download | 75.35 ± 1.69 | 76.08 | 77.02 | 77.03 | 71.81–77.03 |

The rate across all ten files is total bytes divided by total measured time.
It differs from the arithmetic mean of individual file rates:

| Operation | Aggregate MB/s |
| --- | --- |
| Confirmed upload | 5.06 |
| First download | 26.90 |
| Second download | 75.31 |

For uploads, the nine configured two-second pauses add another 18 s. Including
those pauses gives approximately 3.48 MB/s over the upload sequence. The README
uses the aggregate transfer rates without those scheduled pauses.

## Phase timings

All values in this table are milliseconds. TCP/TLS rows measure acquisition of
a fresh connection, including any DNS work performed by that acquisition.

| Phase | Mean ± sigma, ms | p50 | p90 | p95/p99 | Min–max |
| --- | --- | --- | --- | --- | --- |
| Upload TCP/TLS | 17.017 ± 2.034 | 16.519 | 19.605 | 20.375 | 13.550–20.375 |
| Upload open | 0.016 ± 0.001 | 0.016 | 0.017 | 0.018 | 0.015–0.018 |
| Upload write, before finish | 293.596 ± 19.397 | 293.062 | 300.555 | 343.489 | 265.061–343.489 |
| Upload finish | 3639.474 ± 440.113 | 3391.841 | 4043.131 | 4799.289 | 3320.944–4799.289 |
| First download TCP/TLS | 14.486 ± 1.072 | 14.296 | 15.380 | 16.902 | 13.185–16.902 |
| First download open | 237.900 ± 30.033 | 224.991 | 263.742 | 315.330 | 205.874–315.330 |
| First download read and verify | 491.100 ± 114.226 | 446.841 | 571.185 | 773.159 | 325.672–773.159 |
| First download open + read | 729.000 ± 114.441 | 702.022 | 788.250 | 1005.186 | 531.545–1005.186 |
| Second download TCP/TLS | 14.205 ± 1.043 | 13.782 | 15.769 | 16.107 | 12.702–16.107 |
| Second download open | 68.397 ± 5.245 | 65.598 | 76.664 | 79.562 | 63.223–79.562 |
| Second download read and verify | 182.966 ± 0.907 | 183.178 | 183.973 | 184.464 | 181.494–184.464 |
| Second download open + read | 251.363 ± 5.485 | 249.007 | 258.943 | 262.762 | 244.717–262.762 |

Upload `open` returns after writing headers and multipart metadata; its
0.016 ms average does not mean Discord has accepted the file. Writing the
payload takes about 294 ms, then `finish` takes another 3.64 s on average.
`finish` accounts for about 92% of the measured complete upload time.
The slowest upload was file 1, at 5.109 s, including 4.799 s in `finish`.

The public API measures `finish` as a whole. The results cannot split its time
between pending writes, flush, network delay, Discord processing and reading
the response, or establish whether flush waits for TCP acknowledgements.

First-pass download open takes 238 ms on average, versus 68 ms on the second
pass. Reading takes 491 ms versus 183 ms. The complete second download is about
2.8 times faster than the first. This is consistent with a benefit from repeated
access, but neither a first-pass CDN cache miss nor a second-pass cache hit was
observed directly. Cache headers were not collected.

## Phase throughput

| Operation | Mean ± sigma, MB/s | p50 | p90 | p95/p99 | Min–max |
| --- | --- | --- | --- | --- | --- |
| Upload write, before finish | 68.40 ± 4.22 | 67.98 | 72.36 | 75.45 | 58.23–75.45 |
| First download read | 42.69 ± 8.87 | 41.93 | 48.33 | 61.41 | 25.87–61.41 |
| First download open + read | 28.08 ± 4.25 | 28.15 | 30.63 | 37.63 | 19.90–37.63 |
| Second download read | 109.31 ± 0.54 | 109.17 | 110.00 | 110.20 | 108.42–110.20 |
| Second download open + read | 79.60 ± 1.71 | 80.27 | 81.27 | 81.73 | 76.11–81.73 |

Upload writing averages 68.40 MB/s before `finish`, exceeding the nominal
62.5 MB/s upstream line rate. This measures payload acceptance by the writer,
including buffering; it is not a confirmed delivery rate. The complete confirmed
upload rate is about 5 MB/s for these 20 MB files.

Second-pass body reading averages 109.31 MB/s, about 875 Mbps. Including request
latency and connection setup brings the aggregate rate to 75.31 MB/s, about
602 Mbps. First-pass aggregate throughput is about 215 Mbps. Reading-only rates
also exclude body bytes that may already have arrived during `open`, which is
why complete transfer rates are used for the README.

## CPU cost

The final checkpoints record 0.29 s of user CPU and 0.29 s of system CPU.
The resource timeline spans approximately 67.7 s, including scheduled pauses,
so process CPU time is approximately 0.58 s, or 0.86% of one logical core over
that interval. CPU counters include the sampling thread and report generation.

| Phase | Mean user CPU, ms/file | Mean system CPU, ms/file | Mean CPU usage, % of one core |
| --- | --- | --- | --- |
| Upload write | 7.0 | 3.0 | 3.46 |
| Upload finish | 0.0 | 3.0 | 0.08 |
| First download read | 10.0 | 10.0 | 4.27 |
| Second download read | 8.0 | 10.0 | 9.84 |

The observed CPU counters advance in 10 ms increments. Zero CPU readings for
short phases therefore do not establish zero work. Overall CPU use and the
reading-phase measurements suggest that CPU was not the limiting resource in
this run; this is an inference, not a comparison across different CPUs.

## Memory and leak-check status

| Checkpoint | RSS, MiB |
| --- | --- |
| First resource sample, before payload allocation | 4.61 |
| Payload and download buffer allocated | 22.70 |
| Round complete, reusable buffers retained | 24.25 |
| Payload and download buffer dropped | 4.93 |

The largest observed RSS was 24,836 KiB, or 24.25 MiB. RSS increased by
1,588 KiB between the buffers-allocated and round-complete checkpoints, then
fell by 19,792 KiB when the buffers were dropped. The final RSS is 324 KiB above
the first resource sample. These numbers include the runtime, TLS, allocator,
reporting and monitoring overhead, not just the library.

The final checkpoint reports a lower `peak_rss_kib` than the preceding checkpoint.
For this report, peak memory therefore uses the largest observed RSS rather
than treating the final peak counter as authoritative.

The large drop after releasing the buffers is evidence that most resident
payload memory was released. There is only one round and no Valgrind report in
this dataset, so absence of allocation leaks is not established. Multiple rounds
and the [documented Memcheck run](benchmark.md#allocation-leak-check-on-the-benchmark-host)
are still needed for that check. Allocator retention and RSS alone cannot identify
or exclude small leaks.

## Raw results and reproduction

- [Run settings](../benches/bench1/settings.json)
- [Individual observations](../benches/bench1/samples.jsonl)
- [Recorded phase summaries](../benches/bench1/summary.jsonl)
- [Process resource timeline](../benches/bench1/resources.csv)
- [Lifecycle checkpoints](../benches/bench1/checkpoints.jsonl)
- [Benchmark procedure and metric definitions](benchmark.md)

Complete-transfer statistics in this report were calculated from the individual
observations; phase statistics come from the recorded summaries. The connection
speed and hardware describe the supplied environment. One short run cannot
establish Discord-wide throughput limits, behavior under concurrent transfers,
other file sizes or sustained rate limits.
