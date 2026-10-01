//! Sequential live Discord benchmark. See docs/benchmark.md for metrics and leak checks.

mod support;

use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use discord_webhook_storage::{DiscordFile, ReadFile, WebhookCredentials, WriteConfig, WriteFile};
use futures::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use serde::Serialize;
use support::{Monitor, Probe, Resources, Summary};
use tokio_tcp_pool::{Pool, Route, rustls};

type BenchResult<T> = Result<T, Box<dyn Error>>;

#[derive(Serialize)]
struct Settings {
    files: usize,
    bytes: usize,
    rounds: usize,
    gap_ms: u64,
    timeout_secs: u64,
    out: PathBuf,
}

impl Settings {
    fn parse() -> BenchResult<Option<Self>> {
        let mut settings = Self {
            files: 10,
            bytes: 20_000_000,
            rounds: 1,
            gap_ms: 2_000,
            timeout_secs: 180,
            out: PathBuf::from(format!(
                "target/discord-benchmark/{}",
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
            )),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--bench" {
                continue;
            }
            if arg == "--help" || arg == "-h" {
                println!(
                    "DISCORD_WEBHOOK_URL=... cargo bench --bench discord_benchmark --features tokio-tcp-pool -- [options]\n\
                    --files N          files per round (default 10)\n\
                    --bytes N          bytes per file, 8..=20000000 (default 20000000)\n\
                    --rounds N         repeat with new uploads (default 1)\n\
                    --gap-ms N         pause between uploads (default 2000)\n\
                    --timeout-secs N   timeout per operation (default 180)\n\
                    --out DIR          output directory (default target/discord-benchmark/<timestamp>)"
                );
                return Ok(None);
            }
            let value = args
                .next()
                .ok_or_else(|| io::Error::other(format!("missing value for {arg}")))?;
            match arg.as_str() {
                "--files" => settings.files = value.parse()?,
                "--bytes" => settings.bytes = value.parse()?,
                "--rounds" => settings.rounds = value.parse()?,
                "--gap-ms" => settings.gap_ms = value.parse()?,
                "--timeout-secs" => settings.timeout_secs = value.parse()?,
                "--out" => settings.out = value.into(),
                _ => return Err(io::Error::other(format!("unknown option: {arg}")).into()),
            }
        }
        if settings.files == 0
            || settings.rounds == 0
            || settings.timeout_secs == 0
            || !(8..=20_000_000).contains(&settings.bytes)
        {
            return Err(io::Error::other(
                "files, rounds and timeout must be positive; bytes must be 8..=20000000",
            )
            .into());
        }
        Ok(Some(settings))
    }
}

#[derive(Serialize)]
struct Measurement {
    wall_ms: f64,
    cpu_user_ms: Option<f64>,
    cpu_system_ms: Option<f64>,
    cpu_percent: Option<f64>,
    rss_before_kib: Option<u64>,
    rss_after_kib: Option<u64>,
    peak_rss_kib: Option<u64>,
}

async fn timed<T, E: Error + 'static>(
    probe: &Probe,
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<T, E>>,
) -> BenchResult<(T, Measurement)> {
    let before = probe.snapshot();
    let start = Instant::now();
    let value = tokio::time::timeout(timeout, operation).await??;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let after = probe.snapshot();
    let cpu_user_ms = before
        .user_s
        .zip(after.user_s)
        .map(|(a, b)| (b - a) * 1000.0);
    let cpu_system_ms = before
        .system_s
        .zip(after.system_s)
        .map(|(a, b)| (b - a) * 1000.0);
    Ok((
        value,
        Measurement {
            wall_ms,
            cpu_user_ms,
            cpu_system_ms,
            cpu_percent: cpu_user_ms
                .zip(cpu_system_ms)
                .map(|(u, s)| (u + s) / wall_ms * 100.0),
            rss_before_kib: before.rss_kib,
            rss_after_kib: after.rss_kib,
            peak_rss_kib: after.peak_rss_kib,
        },
    ))
}

#[derive(Serialize)]
struct Sample<'a> {
    round: usize,
    file: usize,
    phase: &'a str,
    measurement: &'a Measurement,
    bytes: Option<usize>,
    mb_per_s: Option<f64>,
}

struct Report {
    samples: BufWriter<File>,
    summaries: BufWriter<File>,
    checkpoints: BufWriter<File>,
    values: BTreeMap<String, Vec<f64>>,
}

fn json_line(output: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    writeln!(output)
}

fn create(path: PathBuf) -> io::Result<BufWriter<File>> {
    Ok(BufWriter::new(
        OpenOptions::new().write(true).create_new(true).open(path)?,
    ))
}

impl Report {
    fn record(
        &mut self,
        round: usize,
        file: usize,
        phase: &str,
        m: &Measurement,
        bytes: Option<usize>,
    ) -> io::Result<()> {
        let rate = bytes.map(|n| n as f64 / (m.wall_ms * 1000.0));
        json_line(
            &mut self.samples,
            &Sample {
                round,
                file,
                phase,
                measurement: m,
                bytes,
                mb_per_s: rate,
            },
        )?;
        self.samples.flush()?;
        self.values
            .entry(format!("{phase}.wall_ms"))
            .or_default()
            .push(m.wall_ms);
        for (metric, value) in [
            ("MB_s", rate),
            ("cpu_user_ms", m.cpu_user_ms),
            ("cpu_system_ms", m.cpu_system_ms),
            ("cpu_percent", m.cpu_percent),
        ] {
            if let Some(value) = value {
                self.values
                    .entry(format!("{phase}.{metric}"))
                    .or_default()
                    .push(value);
            }
        }
        Ok(())
    }

    fn checkpoint(&mut self, round: usize, stage: &str, probe: &Probe) -> io::Result<()> {
        #[derive(Serialize)]
        struct Checkpoint<'a> {
            round: usize,
            stage: &'a str,
            resources: Resources,
        }
        json_line(
            &mut self.checkpoints,
            &Checkpoint {
                round,
                stage,
                resources: probe.snapshot(),
            },
        )?;
        self.checkpoints.flush()
    }

    fn summarize(&mut self, round: usize) -> io::Result<()> {
        #[derive(Serialize)]
        struct Entry<'a> {
            round: usize,
            metric: &'a str,
            summary: Summary,
        }
        println!(
            "\nRound {round}: metric | n | mean +/- sigma | p50 | p90 | p95 | p99 | min | max"
        );
        for (metric, mut values) in std::mem::take(&mut self.values) {
            let summary = Summary::new(&mut values);
            println!(
                "{metric} | {} | {:.3} +/- {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3}",
                summary.n,
                summary.mean,
                summary.sigma,
                summary.p50,
                summary.p90,
                summary.p95,
                summary.p99,
                summary.min,
                summary.max
            );
            json_line(
                &mut self.summaries,
                &Entry {
                    round,
                    metric: &metric,
                    summary,
                },
            )?;
        }
        self.summaries.flush()
    }
}

fn pool(target: &str, tls: &Arc<rustls::ClientConfig>) -> BenchResult<Pool> {
    Ok(Pool::builder(Route::Direct {
        target: target.parse()?,
    })
    .max_open(1)
    .tls(Arc::clone(tls))
    .build()?)
}

async fn read_verified<T: AsyncRead + Unpin>(
    reader: &mut T,
    expected: &[u8],
    buffer: &mut [u8],
) -> io::Result<()> {
    let mut offset = 0;
    loop {
        let n = reader.read(buffer).await?;
        if n == 0 {
            break;
        }
        let end = offset + n;
        if expected.get(offset..end) != Some(&buffer[..n]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("download differs at offset {offset}"),
            ));
        }
        offset = end;
    }
    if offset != expected.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "download length differs from upload",
        ));
    }
    Ok(())
}

async fn run_round(
    round: usize,
    settings: &Settings,
    credentials: &WebhookCredentials<'_>,
    tls: &Arc<rustls::ClientConfig>,
    probe: &Probe,
    report: &mut Report,
    buffers: (&mut [u8], &mut [u8]),
) -> BenchResult<()> {
    let (payload, buffer) = buffers;
    let timeout = Duration::from_secs(settings.timeout_secs);
    let uploads = pool("discord.com:443", tls)?;
    let downloads = pool("cdn.discordapp.com:443", tls)?;
    let mut files: Vec<DiscordFile> = Vec::with_capacity(settings.files);
    for index in 0..settings.files {
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(settings.gap_ms)).await;
        }
        payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
        // No release: every operation gets a fresh TCP/TLS connection. This
        // keeps CDN cache comparisons separate from connection reuse effects.
        let (mut connection, m) = timed(probe, timeout, uploads.acquire()).await?;
        report.record(round, index + 1, "upload.connect", &m, None)?;
        let (mut writer, m) = timed(
            probe,
            timeout,
            WriteFile::open(&mut connection, credentials, &WriteConfig::default()),
        )
        .await?;
        report.record(round, index + 1, "upload.open", &m, None)?;
        let (_, m) = timed(probe, timeout, writer.write_all(payload)).await?;
        report.record(round, index + 1, "upload.write", &m, Some(payload.len()))?;
        let (file, m) = timed(probe, timeout, writer.finish()).await?;
        report.record(round, index + 1, "upload.finish", &m, None)?;
        println!(
            "Round {round}: uploaded {}/{} (message {})",
            index + 1,
            settings.files,
            file.id
        );
        files.push(file);
    }
    for (index, file) in files.iter().enumerate() {
        payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
        for pass in ["first", "second"] {
            let (mut connection, m) = timed(probe, timeout, downloads.acquire()).await?;
            report.record(
                round,
                index + 1,
                &format!("download.{pass}.connect"),
                &m,
                None,
            )?;
            let (mut reader, m) =
                timed(probe, timeout, ReadFile::open(&mut connection, file)).await?;
            let open_ms = m.wall_ms;
            report.record(round, index + 1, &format!("download.{pass}.open"), &m, None)?;
            let (_, m) = timed(probe, timeout, read_verified(&mut reader, payload, buffer)).await?;
            report.record(
                round,
                index + 1,
                &format!("download.{pass}.read"),
                &m,
                Some(payload.len()),
            )?;
            // Include time spent receiving headers and any prefetched body.
            let total = Measurement {
                wall_ms: open_ms + m.wall_ms,
                cpu_user_ms: None,
                cpu_system_ms: None,
                cpu_percent: None,
                ..m
            };
            report.record(
                round,
                index + 1,
                &format!("download.{pass}.open_and_read"),
                &total,
                Some(payload.len()),
            )?;
            println!(
                "Round {round}: verified download {}/{} ({pass} pass)",
                index + 1,
                settings.files
            );
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> BenchResult<()> {
    let Some(settings) = Settings::parse()? else {
        return Ok(());
    };
    let webhook_url = std::env::var("DISCORD_WEBHOOK_URL")?;
    let credentials = WebhookCredentials::parse(&webhook_url)?;
    fs::create_dir_all(&settings.out)?;
    let mut metadata = create(settings.out.join("settings.json"))?;
    json_line(&mut metadata, &settings)?;
    metadata.flush()?;
    let mut report = Report {
        samples: create(settings.out.join("samples.jsonl"))?,
        summaries: create(settings.out.join("summary.jsonl"))?,
        checkpoints: create(settings.out.join("checkpoints.jsonl"))?,
        values: BTreeMap::new(),
    };
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let probe = Probe::new();
    let monitor = Monitor::start(&settings.out.join("resources.csv"))?;
    let mut payload = vec![0; settings.bytes];
    rand::fill(&mut payload[..]);
    let mut buffer = vec![0; 256 * 1024];
    println!(
        "{} files of {} bytes, {} rounds; fresh connections, sequential transfers.\nResults: {}\nAttachments remain in Discord. No retries. p95/p99 with 10 samples both equal the maximum.",
        settings.files,
        settings.bytes,
        settings.rounds,
        settings.out.display()
    );
    report.checkpoint(0, "buffers_allocated", &probe)?;
    for round in 1..=settings.rounds {
        if let Err(error) = run_round(
            round,
            &settings,
            &credentials,
            &tls,
            &probe,
            &mut report,
            (&mut payload, &mut buffer),
        )
        .await
        {
            let mut failure = create(settings.out.join("failure.json"))?;
            json_line(
                &mut failure,
                &serde_json::json!({"round": round, "error": error.to_string()}),
            )?;
            failure.flush()?;
            report.summarize(round)?;
            monitor.finish()?;
            return Err(error);
        }
        report.summarize(round)?;
        // Pools, connections, file references and summary vectors are dropped.
        tokio::time::sleep(Duration::from_millis(100)).await;
        report.checkpoint(round, "round_dropped_buffers_retained", &probe)?;
    }
    drop(payload);
    drop(buffer);
    drop(tls);
    report.checkpoint(settings.rounds, "buffers_dropped", &probe)?;
    monitor.finish()?;
    println!(
        "Completed. RSS checkpoints are diagnostic; use Valgrind as documented to check for leaks."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_verification_detects_corruption_and_wrong_lengths() {
        use super::read_verified;

        futures::executor::block_on(async {
            let mut buffer = [0; 2];
            read_verified(
                &mut futures::io::Cursor::new(b"abcdef"),
                b"abcdef",
                &mut buffer,
            )
            .await
            .unwrap();
            for data in [
                b"abXdef".as_slice(),
                b"abcdefg".as_slice(),
                b"abc".as_slice(),
            ] {
                assert!(
                    read_verified(&mut futures::io::Cursor::new(data), b"abcdef", &mut buffer)
                        .await
                        .is_err()
                );
            }
        });
    }
}
