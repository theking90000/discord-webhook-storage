use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::Path,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

#[derive(Clone, Copy, Default, Serialize)]
pub struct Resources {
    pub user_s: Option<f64>,
    pub system_s: Option<f64>,
    pub rss_kib: Option<u64>,
    pub peak_rss_kib: Option<u64>,
}

pub struct Probe {
    ticks: Option<f64>,
}

impl Probe {
    pub fn new() -> Self {
        let ticks = if cfg!(target_os = "linux") {
            Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|value| value.trim().parse::<f64>().ok())
                .filter(|value| *value > 0.0)
        } else {
            None
        };
        Self { ticks }
    }

    pub fn snapshot(&self) -> Resources {
        if !cfg!(target_os = "linux") {
            return Resources::default();
        }
        let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
        let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let cpu = self.ticks.and_then(|ticks| cpu_seconds(&stat, ticks));
        Resources {
            user_s: cpu.map(|(user, _)| user),
            system_s: cpu.map(|(_, system)| system),
            rss_kib: status_kib(&status, "VmRSS:"),
            peak_rss_kib: status_kib(&status, "VmHWM:"),
        }
    }
}

fn status_kib(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn cpu_seconds(stat: &str, ticks: f64) -> Option<(f64, f64)> {
    // comm can contain spaces and ')'; fields after its final ')' start at #3.
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    Some((
        fields.get(11)?.parse::<f64>().ok()? / ticks,
        fields.get(12)?.parse::<f64>().ok()? / ticks,
    ))
}

// Stream samples to disk rather than retaining a growing history in memory.
pub struct Monitor {
    stop: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Monitor {
    pub fn start(path: &Path) -> io::Result<Self> {
        let mut output = BufWriter::new(File::create(path)?);
        writeln!(output, "elapsed_s,user_s,system_s,rss_kib,peak_rss_kib")?;
        let (stop, receive) = mpsc::channel();
        let handle = thread::spawn(move || {
            let probe = Probe::new();
            let start = Instant::now();
            loop {
                let r = probe.snapshot();
                writeln!(
                    output,
                    "{:.6},{},{},{},{}",
                    start.elapsed().as_secs_f64(),
                    optional(r.user_s),
                    optional(r.system_s),
                    optional(r.rss_kib),
                    optional(r.peak_rss_kib)
                )?;
                match receive.recv_timeout(Duration::from_millis(100)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
            output.flush()
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.join()
    }

    fn join(&mut self) -> io::Result<()> {
        let _ = self.stop.send(());
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| io::Error::other("resource monitor panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn optional(value: Option<impl std::fmt::Display>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub sigma: f64,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Summary {
    pub fn new(values: &mut [f64]) -> Self {
        assert!(!values.is_empty());
        values.sort_by(f64::total_cmp);
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let sigma =
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt();
        Self {
            n: values.len(),
            mean,
            sigma,
            min: values[0],
            max: values[values.len() - 1],
            p50: percentile(values, 0.50),
            p90: percentile(values, 0.90),
            p95: percentile(values, 0.95),
            p99: percentile(values, 0.99),
        }
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    // Nearest rank: p95/p99 on ten observations both equal the maximum.
    sorted[((fraction * sorted.len() as f64).ceil() as usize).saturating_sub(1)]
}

#[cfg(test)]
mod tests {
    #[test]
    fn nearest_rank_percentiles_and_population_sigma() {
        use super::Summary;

        let summary = Summary::new(&mut [10., 1., 9., 2., 8., 3., 7., 4., 6., 5.]);
        assert_eq!(summary.mean, 5.5);
        assert!((summary.sigma - 8.25_f64.sqrt()).abs() < 1e-12);
        assert_eq!((summary.p50, summary.p95, summary.p99), (5., 10., 10.));
        assert_eq!(Summary::new(&mut [3.]).sigma, 0.);
    }

    #[test]
    fn parses_proc_stat_with_spaces_and_parentheses_in_comm() {
        use super::{cpu_seconds, status_kib};

        let stat = "123 (name ) with spaces) R 0 0 0 0 0 0 0 0 0 0 150 25 0 0";
        assert_eq!(cpu_seconds(stat, 100.), Some((1.5, 0.25)));
        assert_eq!(
            status_kib("VmRSS:\t123 kB\nVmHWM:\t456 kB", "VmRSS:"),
            Some(123)
        );
        assert_eq!(cpu_seconds("truncated", 100.), None);
    }
}
