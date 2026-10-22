//! マシン負荷の履歴（グラフに使う時系列）。
//!
//! 計測（[`super::system::Sampler`]）は 1 秒ごと、グラフは直近 5 分を描画幅に合わせて
//! 時間で集約する。サンプル数で切ると、ペインの幅によって表示する時間が変わってしまう。

use std::time::{Duration, Instant};

use super::history::Series;
use super::system::{Pressure, Sample};

/// グラフが表す時間。
pub const WINDOW: Duration = Duration::from_secs(5 * 60);
/// 計測の間隔。
pub const INTERVAL: Duration = Duration::from_secs(1);
/// 数値で出す「現在の値」を平均する時間。1 秒ごとの CPU 使用率は振れが大きく、
/// そのまま出すと読み取れない。
const RECENT: Duration = Duration::from_secs(5);

pub struct LoadHistory {
    /// CPU 使用率（0〜100）。
    pub cpu: Series,
    /// メモリ使用率（0〜100）。
    pub memory: Series,
    /// メモリの逼迫度（[`pressure_code`]）。
    pub pressure: Series,
    pub latest: Option<Sample>,
}

impl Default for LoadHistory {
    fn default() -> Self {
        Self {
            cpu: Series::new(WINDOW, INTERVAL),
            memory: Series::new(WINDOW, INTERVAL),
            pressure: Series::new(WINDOW, INTERVAL),
            latest: None,
        }
    }
}

impl LoadHistory {
    pub fn record(&mut self, sample: Sample) {
        if let Some(cpu) = sample.cpu_percent.filter(|cpu| cpu.is_finite()) {
            self.cpu.push(sample.at, cpu.clamp(0.0, 100.0));
        }
        if let Some(memory) = &sample.memory {
            self.memory
                .push(sample.at, memory.used_percent().clamp(0.0, 100.0));
            if let Some(code) = pressure_code(memory.pressure) {
                self.pressure.push(sample.at, code);
            }
        }
        self.latest = Some(sample);
    }

    /// 直近数秒の CPU 使用率の平均。
    pub fn recent_cpu(&self, now: Instant) -> Option<f64> {
        recent(&self.cpu, now)
    }

    /// 直近数秒のメモリ使用率の平均。
    pub fn recent_memory(&self, now: Instant) -> Option<f64> {
        recent(&self.memory, now)
    }
}

fn recent(series: &Series, now: Instant) -> Option<f64> {
    let columns = (WINDOW.as_secs() / RECENT.as_secs()) as usize;
    series
        .columns_mean(now, columns)
        .last()
        .copied()
        .flatten()
        .or_else(|| series.latest())
}

/// 逼迫度をグラフの集約に使う数値へ。不明は記録しない（欠測として描く）。
pub fn pressure_code(pressure: Pressure) -> Option<f64> {
    match pressure {
        Pressure::Unknown => None,
        Pressure::Normal => Some(0.0),
        Pressure::Warning => Some(1.0),
        Pressure::Critical => Some(2.0),
    }
}

/// [`pressure_code`] の逆。集約で最大を取った値を段階へ戻す。
pub fn pressure_from_code(code: f64) -> Pressure {
    if code >= 2.0 {
        Pressure::Critical
    } else if code >= 1.0 {
        Pressure::Warning
    } else {
        Pressure::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::super::system::Memory;
    use super::*;

    fn sample(at: Instant, cpu: Option<f64>, used: u64, pressure: Pressure) -> Sample {
        Sample {
            at,
            cpu_percent: cpu,
            load: None,
            logical_cpus: None,
            memory: Some(Memory {
                total_bytes: 100,
                used_bytes: used,
                compressed_bytes: None,
                pressure,
            }),
            swap: None,
        }
    }

    #[test]
    fn records_values_within_range_and_skips_unknown_pressure() {
        let t0 = Instant::now();
        let mut history = LoadHistory::default();
        history.record(sample(t0, None, 50, Pressure::Unknown));
        history.record(sample(t0 + INTERVAL, Some(150.0), 60, Pressure::Warning));
        history.record(sample(
            t0 + INTERVAL * 2,
            Some(f64::NAN),
            70,
            Pressure::Normal,
        ));
        assert_eq!(history.cpu.latest(), Some(100.0));
        assert_eq!(history.memory.latest(), Some(70.0));
        assert_eq!(history.pressure.latest(), Some(0.0));
        assert!(history.latest.is_some());
    }

    #[test]
    fn pressure_codes_round_trip() {
        for pressure in [Pressure::Normal, Pressure::Warning, Pressure::Critical] {
            assert_eq!(
                pressure_from_code(pressure_code(pressure).unwrap()),
                pressure
            );
        }
        assert_eq!(pressure_code(Pressure::Unknown), None);
    }
}
