//! Measures how much of the machine is free, from `/proc`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::api::Load;

/// The window idle time is averaged over. Short enough to notice a build
/// that started compiling, long enough to smooth out bursts.
const WINDOW: Duration = Duration::from_secs(5);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

#[derive(Clone)]
pub struct LoadSampler {
    cpus: u32,
    samples: Arc<Mutex<VecDeque<(Instant, CpuTimes)>>>,
}

impl LoadSampler {
    pub fn start() -> anyhow::Result<Self> {
        let stat = std::fs::read_to_string("/proc/stat").context("reading /proc/stat")?;
        let (cpus, times) = parse_stat(&stat)?;
        let sampler = Self {
            cpus,
            samples: Arc::new(Mutex::new(VecDeque::from([(Instant::now(), times)]))),
        };
        let samples = sampler.samples.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SAMPLE_INTERVAL).await;
                let times = match std::fs::read_to_string("/proc/stat")
                    .map_err(anyhow::Error::from)
                    .and_then(|stat| parse_stat(&stat))
                {
                    Ok((_, times)) => times,
                    Err(error) => {
                        tracing::warn!("sampling CPU load: {error:#}");
                        continue;
                    }
                };
                let mut samples = samples.lock().expect("not poisoned");
                samples.push_back((Instant::now(), times));
                while samples.len() > 2 && samples[1].0.elapsed() >= WINDOW {
                    samples.pop_front();
                }
            }
        });
        Ok(sampler)
    }

    pub fn cpus(&self) -> u32 {
        self.cpus
    }

    pub fn current(&self) -> Load {
        let idle_cores = {
            let samples = self.samples.lock().expect("not poisoned");
            let (first, last) = (
                samples.front().expect("never empty").1,
                samples.back().expect("never empty").1,
            );
            let total = last.total.saturating_sub(first.total);
            if total == 0 {
                // No interval measured yet: assume the machine is busy.
                0.0
            } else {
                last.idle.saturating_sub(first.idle) as f64 / total as f64 * f64::from(self.cpus)
            }
        };
        Load {
            cpus: self.cpus,
            idle_cores,
            window_secs: WINDOW.as_secs(),
            mem_available_bytes: mem_available().unwrap_or_else(|error| {
                tracing::warn!("reading available memory: {error:#}");
                0
            }),
        }
    }
}

/// The number of CPUs and the aggregate CPU times.
fn parse_stat(stat: &str) -> anyhow::Result<(u32, CpuTimes)> {
    let mut lines = stat.lines();
    let aggregate = lines.next().context("empty /proc/stat")?;
    let fields: Vec<u64> = aggregate
        .strip_prefix("cpu ")
        .context("unexpected /proc/stat")?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .context("unexpected /proc/stat")?;
    anyhow::ensure!(fields.len() >= 8, "unexpected /proc/stat");
    // user nice system idle iowait irq softirq steal; guest time is part of user.
    let times = CpuTimes {
        idle: fields[3] + fields[4],
        total: fields[..8].iter().sum(),
    };
    let cpus = lines
        .take_while(|line| line.starts_with("cpu"))
        .count()
        .try_into()?;
    Ok((cpus, times))
}

fn mem_available() -> anyhow::Result<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .context("no MemAvailable in /proc/meminfo")?
        .trim()
        .parse()?;
    Ok(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat() {
        let stat = "cpu  100 5 50 800 20 3 2 0 7 0\n\
                    cpu0 50 2 25 400 10 1 1 0 3 0\n\
                    cpu1 50 3 25 400 10 2 1 0 4 0\n\
                    intr 12345\n";
        let (cpus, times) = parse_stat(stat).unwrap();
        assert_eq!(cpus, 2);
        assert_eq!(times.idle, 820);
        assert_eq!(times.total, 980);
    }
}
