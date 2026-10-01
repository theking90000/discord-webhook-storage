# Discord benchmark

Run on the host whose connection is being measured, with a webhook that accepts
20 MB attachments. The default run uploads ten files of 20,000,000 bytes
sequentially, then downloads each file twice. The second download immediately
follows the first. Uploads and downloads never overlap. Attachments remain in
the webhook's channel.

```sh
export DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>'
cargo bench --locked --bench discord_benchmark --features tokio-tcp-pool
```

Arguments follow `--`. For twenty distinct files, use `--files 20`, which produces
forty downloads. `--rounds N` repeats the entire experiment with new attachments.
`--bytes N` changes the file size up to the library's 20,000,000-byte limit.
`--gap-ms N` changes the default two-second pause between uploads. Pauses are
outside transfer timings. Each operation has a timeout of 180 seconds, adjustable
with `--timeout-secs N`. There are no retries, so failed attempts cannot disappear
from the results. On failure, completed samples and partial summaries remain
available, and `failure.json` records the error.

## Transfer measurements

Each transfer uses a fresh TCP/TLS connection to make both download passes
comparable. The payload contains random bytes generated before measurements.
Its first eight bytes identify the file within the round. Payload generation,
connection setup and report writing are outside `open`, writing, reading and
`finish` timings.

| Metric | Measured interval |
| --- | --- |
| `upload.connect.wall_ms` | Connection acquisition, including DNS, TCP and TLS |
| `upload.open.wall_ms` | `WriteFile::open` on an established connection |
| `upload.write.wall_ms` | `write_all` of the payload, before calling `finish` |
| `upload.write.MB_s` | Payload size divided by writing time |
| `upload.finish.wall_ms` | The complete `finish` call, including final framing, flush and reading Discord's response |
| `download.first.connect.wall_ms` | Fresh connection for the first download |
| `download.first.open.wall_ms` | `ReadFile::open`, including request and response headers |
| `download.first.read.wall_ms` | Reading through EOF and verifying the contents |
| `download.first.read.MB_s` | Payload size divided by reading time after `open` |
| `download.first.open_and_read.wall_ms` | Sum of open and reading time |
| `download.first.open_and_read.MB_s` | Payload size divided by open plus reading time |

`download.second.*` contains the same measurements for the second pass. Rates
use decimal MB/s, where 1 MB is 1,000,000 bytes. Every download is verified against
the upload with a reused 256 KiB buffer. Verification CPU time is included in
reading time. `open` can receive some body bytes, so `open_and_read` gives a more
complete download rate than `read` alone.

`WriteFile::open` sends request headers and multipart metadata. It does not wait
for Discord to accept a file. Likewise, writing speed before `finish` describes
how quickly the writer accepts the payload, including any buffering in the
library, TLS and operating system. It is not confirmation that Discord received
all bytes. `finish` confirms acceptance. Its duration includes outstanding
writes, flushing, network delay, server processing and response parsing. The
public API does not expose TCP ACK timing or separate these components.

The first pass is the first download performed by this program for that
attachment. The second is a candidate for a warm CDN cache. The public API does
not expose cache status. These labels therefore cannot guarantee a cache miss or
hit, and other clients can affect the CDN cache.

## Distributions and output

The console and `summary.jsonl` report each round separately with the sample
count, mean, population standard deviation, minimum, p50, p90, p95, p99 and
maximum. Percentiles use the nearest rank method. With ten observations, p95 and
p99 both equal the maximum. More files are needed to estimate the tail more
precisely. All individual observations are preserved for plots or other
statistical analysis.

The default output directory is `target/discord-benchmark/<timestamp>`. Use
`--out DIR` to choose another directory. Existing report files are not overwritten.

| File | Contents |
| --- | --- |
| `settings.json` | Run settings, without webhook credentials |
| `samples.jsonl` | One observation per file and phase, including timings, rates and resources |
| `summary.jsonl` | Distribution summaries per round and metric |
| `resources.csv` | Process resource samples every 100 ms throughout the experiment |
| `checkpoints.jsonl` | Memory and cumulative CPU at stable lifecycle checkpoints |

## CPU and memory

On Linux, the program reads `/proc/self/stat` for cumulative process user and
system CPU time and `/proc/self/status` for RSS and peak RSS. It obtains the CPU
clock tick frequency from `getconf CLK_TCK`. The reports contain null or empty
fields when a measurement is unavailable, including on other operating systems.
There is no added dependency or unsafe code.

Each transfer phase includes process CPU time and CPU usage as a percentage of
one logical core. These include the sampling thread. Short phases can register
zero CPU time because the operating system counts CPU in clock ticks. RSS is in
KiB. The CSV samples can miss peaks shorter than 100 ms; Linux's peak RSS field
also records the process lifetime high-water mark. Report writing, random data
generation and pauses appear in the cumulative process measurements but are
outside transfer phase timings.

The program retains one payload and one download buffer across rounds. File
references, connections, pools and statistics are dropped after each round.
Resource history and raw observations are streamed to disk, so they do not
accumulate in memory. Compare the `round_dropped_buffers_retained` checkpoints
after the first round to look for continuing RSS growth. Allocator caches can
retain freed memory, so RSS growth alone cannot prove an allocation leak, and
stable RSS cannot exclude a small leak.

## Allocation leak check on the benchmark host

For an allocation leak check on Linux, run the same executable under Valgrind
Memcheck after installing Valgrind on that host. Preserve debug symbols in the
release build. This run requires the webhook environment variable above and
uploads new files during every round.

```sh
BENCH_BIN=$(CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release --locked \
    --bench discord_benchmark --features tokio-tcp-pool --message-format=json | \
    python3 -c '
import json, sys
for line in sys.stdin:
    artifact = json.loads(line)
    if (artifact.get("reason") == "compiler-artifact"
        and artifact.get("target", {}).get("name") == "discord_benchmark"
        and artifact.get("executable")):
        print(artifact["executable"])
')

valgrind --tool=memcheck --leak-check=full --show-leak-kinds=all \
    --errors-for-leak-kinds=definite,indirect --error-exitcode=99 \
    --log-file=target/discord-leaks.log \
    "$BENCH_BIN" \
    --rounds 5 --files 10 --timeout-secs 900 --out target/discord-leak-check
```

The build command uses Python 3 to extract the executable path from Cargo's
JSON output, including when `CARGO_TARGET_DIR` is set. Choose a new output
directory for each run. Check that
all rounds completed, the process exited successfully, and Memcheck reports no
definitely or indirectly lost allocations and no other memory errors. Review
possibly lost allocations separately. Still-reachable process caches are
reported too. A clean result covers the executed paths and input sizes.

Use the normal release run for performance numbers. Memcheck changes CPU and
wall-clock timings. No live Discord or allocation leak result can be claimed
until this command runs successfully on the benchmark host.
