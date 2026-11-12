//! マシン全体の負荷（CPU・メモリ・スワップ・ロードアベレージ）を計測する。
//!
//! モニターペインのグラフ用に 1 秒ごとに呼ばれる前提で、1 回の計測はブロックしない（sleep せず、
//! OS の累積カウンタと現在値を読むだけ）。CPU 使用率とスワップアウト量は累積カウンタの差分から
//! 求めるので、[`Sampler`] が前回の値を持つ。
//!
//! macOS は Mach（`host_statistics` / `host_statistics64`）と
//! `sysctlbyname`、Linux は `/proc` を読む。`unsafe` を含む OS 固有の呼び出しは `macos.rs` /
//! `linux.rs` / `unix.rs` に閉じ込め、差分と換算の計算はここに置いてどの OS でもテストする。
//! `/proc` の解析も純粋関数として `procfs.rs` に分け、Linux 以外でもコンパイル・テストする。
//! Windows は Win32 API で CPU と物理メモリを読む。未対応の項目は None にする。

use std::time::Instant;

// Linux の `/proc` の解析は純粋関数だけなので、Linux 以外でもコンパイルしてテストする。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod procfs;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(windows)]
use windows as os;

/// macOS と Linux 以外では何も読まない（全項目 None）。コンパイルが通ることだけを保証する。
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod os {
    use super::Reading;

    pub(super) struct Probe;

    impl Probe {
        pub(super) fn new() -> Self {
            Self
        }

        pub(super) fn read(&self) -> Reading {
            Reading::default()
        }
    }
}

/// メモリの逼迫度。macOS は `kern.memorystatus_vm_pressure_level`（1=normal, 2=warn, 4=critical）。
/// それ以外の OS と、読めなかったときは Unknown。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    Unknown,
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub total_bytes: u64,
    /// macOS: Activity Monitor の「使用済みメモリ」= (internal − purgeable + wired + compressor) × ページサイズ。
    /// Linux: MemTotal − MemAvailable。ファイルキャッシュを含めない。
    pub used_bytes: u64,
    /// macOS: compressor_page_count × ページサイズ（圧縮メモリが実際に占める量）。Linux は None。
    pub compressed_bytes: Option<u64>,
    pub pressure: Pressure,
}

impl Memory {
    /// 使用率（0〜100）。total が 0 なら 0.0。
    ///
    /// 計測値は used ≤ total に揃えてあるが、手で組み立てた値でも 0〜100 を外れないようにする
    /// （ゲージの描画は 0〜100 を前提にする）。
    pub fn used_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 * 100.0 / self.total_bytes as f64).clamp(0.0, 100.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Swap {
    pub total_bytes: u64,
    pub used_bytes: u64,
    /// 直前の計測からのスワップアウト量 / 実測の経過秒。初回・カウンタの巻き戻り・経過 0 は None。
    pub out_bytes_per_sec: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub at: Instant,
    /// マシン全体の CPU 使用率 0〜100。直前の計測からの平均。初回・差分 0・読めないときは None。
    pub cpu_percent: Option<f64>,
    /// getloadavg の 1/5/15 分。
    pub load: Option<[f64; 3]>,
    /// sysconf(_SC_NPROCESSORS_ONLN)。
    pub logical_cpus: Option<usize>,
    pub memory: Option<Memory>,
    pub swap: Option<Swap>,
}

/// 前回の累積カウンタを持ち、呼ぶたびに 1 サンプルを返す。1 秒ごとに呼ばれる前提で、1 回の呼び出しは
/// ブロックしない（数 ms 以内。sleep しない）。
///
/// 1 秒より短い間隔では呼ばない（取り損ねた tick をまとめて取り返すタイマーにもしない）。macOS は
/// `host_statistics` / `host_statistics64` を、OS に付属するプログラム以外のプロセス全体で 1 秒の
/// 窓ごとに数回しか計算し直さず、それを超えた呼び出しには直前の結果を成功として返す。同じ窓で
/// 2 回読むと同じ値が返り、CPU 使用率は差分が無いので None になる。1 秒ごとなら毎回別の窓の値になる。
///
/// macOS ではホストポートの送信権を 1 つ持ち、`Drop` で解放する。複製すると同じ権利を 2 度
/// 解放してしまうので `Clone` にはしない。
pub struct Sampler {
    probe: os::Probe,
    rates: RateState,
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            probe: os::Probe::new(),
            rates: RateState::default(),
        }
    }

    /// `now` は単調時計の現在時刻。スワップアウト率の分母（前回の計測からの経過）に使う。
    pub fn sample(&mut self, now: Instant) -> Sample {
        let reading = self.probe.read();
        self.rates.update(now, reading)
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// 1 回の計測で OS から読んだ値。累積カウンタは生のまま持ち、差分は [`RateState`] が取る。
#[derive(Clone, Debug, Default, PartialEq)]
struct Reading {
    cpu: Option<CpuTicks>,
    load: Option<[f64; 3]>,
    logical_cpus: Option<usize>,
    memory: Option<Memory>,
    swap_usage: Option<SwapUsage>,
    /// 起動からスワップアウトした量の累計（バイト）。
    swap_out_bytes: Option<u64>,
}

/// スワップ領域の大きさと使用量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapUsage {
    total_bytes: u64,
    used_bytes: u64,
}

/// 全 CPU を合計した CPU 時間の累積カウンタ。使用中と待機に分けて持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuTicks {
    /// macOS の `host_cpu_load_info`。`natural_t`（u32）なので、長時間の稼働で 2^32 を超えると
    /// 0 へ巻き戻る。user + system + nice を u32 の wrapping 加算でまとめて持つ。差分も u32 の
    /// wrapping で取るので、各カウンタが別々の時点で巻き戻っても合計の差分は正しい。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Wrapping32 { busy: u32, idle: u32 },
    /// Linux の `/proc/stat`（u64）。減ることはないので、減ったら再起動などの巻き戻りとみなす。
    Monotonic64 { busy: u64, idle: u64 },
}

/// 2 回の累積カウンタから、その間の CPU 使用率（0〜100）を求める。
///
/// tick の差分が 0（同じ tick の中で 2 回呼ばれた）と、u64 のカウンタが減った（巻き戻り）ときは None。
fn cpu_busy_percent(previous: CpuTicks, current: CpuTicks) -> Option<f64> {
    let (busy, idle) = match (previous, current) {
        (
            CpuTicks::Wrapping32 {
                busy: previous_busy,
                idle: previous_idle,
            },
            CpuTicks::Wrapping32 { busy, idle },
        ) => (
            u64::from(busy.wrapping_sub(previous_busy)),
            u64::from(idle.wrapping_sub(previous_idle)),
        ),
        (
            CpuTicks::Monotonic64 {
                busy: previous_busy,
                idle: previous_idle,
            },
            CpuTicks::Monotonic64 { busy, idle },
        ) => (
            busy.checked_sub(previous_busy)?,
            idle.checked_sub(previous_idle)?,
        ),
        // 取得元の違うカウンタは比べられない（同じ OS では起きない）。
        _ => return None,
    };
    let total = busy.checked_add(idle)?;
    if total == 0 {
        return None;
    }
    Some(busy as f64 * 100.0 / total as f64)
}

/// 累積量（バイト）の差分を、実測の経過秒で割る。
///
/// 経過が 0 以下（同じ時刻・時刻の逆行）とカウンタの減少（巻き戻り）は None。
fn bytes_per_sec(previous_at: Instant, previous: u64, now: Instant, current: u64) -> Option<f64> {
    let elapsed = now.checked_duration_since(previous_at)?;
    if elapsed.is_zero() {
        return None;
    }
    let delta = current.checked_sub(previous)?;
    Some(delta as f64 / elapsed.as_secs_f64())
}

/// 率を求めるための前回の累積カウンタ。
///
/// 読めなかった回は基準を更新しない。次に読めた回は最後に読めた回との差分になり、
/// その間の平均として正しい（スワップアウト率の経過時間も、基準を読んだ回の時刻から測る）。
/// 巻き戻りの回は率を出さず、基準だけを新しい値へ取り直す。
#[derive(Debug, Default)]
struct RateState {
    cpu: Option<CpuTicks>,
    swap_out: Option<(Instant, u64)>,
}

impl RateState {
    fn update(&mut self, now: Instant, reading: Reading) -> Sample {
        let cpu_percent = reading.cpu.and_then(|current| {
            let previous = self.cpu.replace(current)?;
            cpu_busy_percent(previous, current)
        });
        let out_bytes_per_sec = reading.swap_out_bytes.and_then(|current| {
            let (previous_at, previous) = self.swap_out.replace((now, current))?;
            bytes_per_sec(previous_at, previous, now, current)
        });
        Sample {
            at: now,
            cpu_percent,
            load: reading.load,
            logical_cpus: reading.logical_cpus,
            memory: reading.memory,
            swap: reading.swap_usage.map(|usage| Swap {
                total_bytes: usage.total_bytes,
                used_bytes: usage.used_bytes,
                out_bytes_per_sec,
            }),
        }
    }
}

// ---- macOS の値の換算（Mach の呼び出しとは分け、どの OS でもテストする） ----

/// `kern.memorystatus_vm_pressure_level` の値を逼迫度へ読み替える。1/2/4 以外は Unknown。
///
/// この sysctl はカーネル内部の段階ではなく、dispatch の通知と同じ値（1/2/4）を返す。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pressure_from_level(level: i32) -> Pressure {
    match level {
        1 => Pressure::Normal,
        2 => Pressure::Warning,
        4 => Pressure::Critical,
        _ => Pressure::Unknown,
    }
}

/// Activity Monitor の「使用済みメモリ」の計算に使う `vm_statistics64` のページ数。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MachVmPages {
    /// 匿名メモリ（`internal_page_count`）。アプリが確保したメモリの大半。
    internal: u64,
    /// 捨ててよいとアプリが印を付けたページ（`purgeable_count`）。internal の一部で、
    /// 逼迫すればすぐ捨てられるので使用中に数えない。
    purgeable: u64,
    /// 固定されたページ（`wire_count`）。カーネルなどが使い、追い出せない。
    wired: u64,
    /// 圧縮メモリが実際に占めるページ（`compressor_page_count`）。
    compressor: u64,
}

/// macOS のメモリ使用量を Activity Monitor の「使用済みメモリ」と同じ式で求める。
///
/// ファイルキャッシュ（external）は含めない。含めると、メモリに余裕があってもキャッシュで
/// ほぼ満杯に見え、逼迫しているのかを読み取れない。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mach_memory(
    total_bytes: u64,
    page_size: u64,
    pages: MachVmPages,
    pressure: Pressure,
) -> Option<Memory> {
    if total_bytes == 0 || page_size == 0 {
        return None;
    }
    // 各カウンタは同時には読めないので、purgeable が internal をわずかに上回っても負にしない。
    let used_pages = pages
        .internal
        .saturating_sub(pages.purgeable)
        .saturating_add(pages.wired)
        .saturating_add(pages.compressor);
    Some(Memory {
        total_bytes,
        // 読む時刻のずれで総量をわずかに超えても、used ≤ total を保つ。
        used_bytes: used_pages.saturating_mul(page_size).min(total_bytes),
        compressed_bytes: Some(pages.compressor.saturating_mul(page_size).min(total_bytes)),
        pressure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    fn linux_ticks(busy: u64, idle: u64) -> CpuTicks {
        CpuTicks::Monotonic64 { busy, idle }
    }

    fn reading(cpu: Option<CpuTicks>, swap_out_bytes: Option<u64>) -> Reading {
        Reading {
            cpu,
            swap_usage: Some(SwapUsage {
                total_bytes: 4 * GIB,
                used_bytes: GIB,
            }),
            swap_out_bytes,
            ..Reading::default()
        }
    }

    fn out_rate(sample: &Sample) -> Option<f64> {
        sample.swap.as_ref().and_then(|swap| swap.out_bytes_per_sec)
    }

    #[test]
    fn cpu_percent_is_the_busy_share_of_elapsed_ticks() {
        // 使用中 300 tick・待機 700 tick 進んだ → 30%。
        assert_eq!(
            cpu_busy_percent(linux_ticks(100, 900), linux_ticks(400, 1600)),
            Some(30.0)
        );
        assert_eq!(
            cpu_busy_percent(linux_ticks(100, 900), linux_ticks(100, 1900)),
            Some(0.0)
        );
        assert_eq!(
            cpu_busy_percent(linux_ticks(100, 900), linux_ticks(1100, 900)),
            Some(100.0)
        );
    }

    #[test]
    fn cpu_percent_survives_u32_wraparound() {
        // macOS の tick は u32。使用中は 2^32 をまたいで 300、待機もまたいで 700 進んだ。
        let previous = CpuTicks::Wrapping32 {
            busy: u32::MAX - 99,
            idle: u32::MAX - 299,
        };
        let current = CpuTicks::Wrapping32 {
            busy: 200,
            idle: 400,
        };
        assert_eq!(cpu_busy_percent(previous, current), Some(30.0));
    }

    #[test]
    fn cpu_percent_is_none_without_elapsed_ticks() {
        assert_eq!(
            cpu_busy_percent(linux_ticks(100, 900), linux_ticks(100, 900)),
            None
        );
        let ticks = CpuTicks::Wrapping32 { busy: 7, idle: 11 };
        assert_eq!(cpu_busy_percent(ticks, ticks), None);
    }

    #[test]
    fn cpu_percent_is_none_when_a_u64_counter_goes_back() {
        assert_eq!(
            cpu_busy_percent(linux_ticks(500, 900), linux_ticks(400, 1600)),
            None
        );
        assert_eq!(
            cpu_busy_percent(linux_ticks(100, 900), linux_ticks(400, 800)),
            None
        );
    }

    #[test]
    fn first_sample_has_no_rates() {
        let mut rates = RateState::default();
        let now = Instant::now();
        let sample = rates.update(now, reading(Some(linux_ticks(100, 900)), Some(MIB)));
        assert_eq!(sample.at, now);
        assert_eq!(sample.cpu_percent, None);
        assert_eq!(
            sample.swap,
            Some(Swap {
                total_bytes: 4 * GIB,
                used_bytes: GIB,
                out_bytes_per_sec: None,
            })
        );
    }

    #[test]
    fn second_sample_reports_cpu_and_swap_out_rates() {
        let mut rates = RateState::default();
        let base = Instant::now();
        rates.update(base, reading(Some(linux_ticks(100, 900)), Some(MIB)));
        // 2 秒で 8 MiB スワップアウトした → 4 MiB/s。
        let sample = rates.update(
            base + Duration::from_secs(2),
            reading(Some(linux_ticks(400, 1600)), Some(9 * MIB)),
        );
        assert_eq!(sample.cpu_percent, Some(30.0));
        assert_eq!(out_rate(&sample), Some((4 * MIB) as f64));
    }

    #[test]
    fn swap_out_rate_divides_by_the_measured_elapsed_time() {
        // 呼び出しが 1 秒ごとからずれても、実測の経過（1.5 秒）で割る。
        let mut rates = RateState::default();
        let base = Instant::now();
        rates.update(base, reading(None, Some(0)));
        let sample = rates.update(
            base + Duration::from_millis(1500),
            reading(None, Some(3 * MIB)),
        );
        assert_eq!(out_rate(&sample), Some((2 * MIB) as f64));
    }

    #[test]
    fn rates_are_none_after_a_rewind_and_resume_from_the_new_baseline() {
        let mut rates = RateState::default();
        let base = Instant::now();
        rates.update(
            base,
            reading(Some(linux_ticks(1_000, 9_000)), Some(10 * MIB)),
        );
        // カウンタが巻き戻った回は率を出さない。
        let rewound = rates.update(
            base + Duration::from_secs(1),
            reading(Some(linux_ticks(10, 90)), Some(MIB)),
        );
        assert_eq!(rewound.cpu_percent, None);
        assert_eq!(out_rate(&rewound), None);
        // 次の回は巻き戻った後の値を基準にする。
        let next = rates.update(
            base + Duration::from_secs(2),
            reading(Some(linux_ticks(60, 140)), Some(3 * MIB)),
        );
        assert_eq!(next.cpu_percent, Some(50.0));
        assert_eq!(out_rate(&next), Some((2 * MIB) as f64));
    }

    #[test]
    fn swap_out_rate_is_none_for_zero_or_backward_elapsed_time() {
        let mut rates = RateState::default();
        let base = Instant::now() + Duration::from_secs(10);
        rates.update(base, reading(None, Some(MIB)));
        let same_time = rates.update(base, reading(None, Some(2 * MIB)));
        assert_eq!(out_rate(&same_time), None);
        let backward = rates.update(base - Duration::from_secs(1), reading(None, Some(3 * MIB)));
        assert_eq!(out_rate(&backward), None);
    }

    #[test]
    fn unreadable_counters_keep_the_last_baseline() {
        // 途中の 1 回が読めなくても、次に読めた回は最後に読めた回からの平均になる。
        let mut rates = RateState::default();
        let base = Instant::now();
        rates.update(base, reading(Some(linux_ticks(0, 0)), Some(0)));
        let missing = rates.update(base + Duration::from_secs(1), reading(None, None));
        assert_eq!(missing.cpu_percent, None);
        assert_eq!(out_rate(&missing), None);
        let next = rates.update(
            base + Duration::from_secs(2),
            reading(Some(linux_ticks(50, 150)), Some(4 * MIB)),
        );
        assert_eq!(next.cpu_percent, Some(25.0));
        assert_eq!(out_rate(&next), Some((2 * MIB) as f64));
    }

    #[test]
    fn swap_is_none_when_usage_is_unreadable_but_the_baseline_still_advances() {
        let mut rates = RateState::default();
        let base = Instant::now();
        let no_usage = Reading {
            swap_out_bytes: Some(MIB),
            ..Reading::default()
        };
        assert_eq!(rates.update(base, no_usage).swap, None);
        let sample = rates.update(base + Duration::from_secs(1), reading(None, Some(3 * MIB)));
        assert_eq!(out_rate(&sample), Some((2 * MIB) as f64));
    }

    #[test]
    fn instantaneous_values_pass_through() {
        let memory = Memory {
            total_bytes: 16 * GIB,
            used_bytes: 4 * GIB,
            compressed_bytes: None,
            pressure: Pressure::Unknown,
        };
        let mut rates = RateState::default();
        let sample = rates.update(
            Instant::now(),
            Reading {
                load: Some([1.5, 1.0, 0.5]),
                logical_cpus: Some(8),
                memory: Some(memory.clone()),
                ..Reading::default()
            },
        );
        assert_eq!(sample.load, Some([1.5, 1.0, 0.5]));
        assert_eq!(sample.logical_cpus, Some(8));
        assert_eq!(sample.memory, Some(memory));
        assert_eq!(sample.swap, None);
    }

    #[test]
    fn used_percent_is_zero_for_zero_total_and_stays_within_0_to_100() {
        let memory = |total_bytes, used_bytes| Memory {
            total_bytes,
            used_bytes,
            compressed_bytes: None,
            pressure: Pressure::Unknown,
        };
        assert_eq!(memory(0, 0).used_percent(), 0.0);
        assert_eq!(memory(0, GIB).used_percent(), 0.0);
        assert_eq!(memory(4 * GIB, GIB).used_percent(), 25.0);
        assert_eq!(memory(GIB, 2 * GIB).used_percent(), 100.0);
    }

    #[test]
    fn mach_memory_follows_the_activity_monitor_formula() {
        // 実機（24 GiB、16 KiB ページ）の vm_stat の値。
        // (internal 636551 − purgeable 4012 + wired 222221 + compressor 355028) × 16384。
        let pages = MachVmPages {
            internal: 636_551,
            purgeable: 4_012,
            wired: 222_221,
            compressor: 355_028,
        };
        let memory = mach_memory(24 * GIB, 16_384, pages, Pressure::Normal).unwrap();
        assert_eq!(memory.total_bytes, 24 * GIB);
        assert_eq!(memory.used_bytes, 1_209_788 * 16_384);
        assert_eq!(memory.compressed_bytes, Some(355_028 * 16_384));
        assert_eq!(memory.pressure, Pressure::Normal);
        assert!((memory.used_percent() - 76.916).abs() < 0.001);
    }

    #[test]
    fn mach_memory_saturates_and_never_exceeds_the_total() {
        let pages = MachVmPages {
            internal: 10,
            purgeable: 20,
            wired: 5,
            compressor: 1,
        };
        // purgeable が internal を上回っても負にならない（使用中 = wired + compressor）。
        let memory = mach_memory(GIB, 4_096, pages, Pressure::Unknown).unwrap();
        assert_eq!(memory.used_bytes, 6 * 4_096);
        // ページ数の合計が総量を超えても used ≤ total。
        let huge = MachVmPages {
            internal: u64::MAX,
            purgeable: 0,
            wired: u64::MAX,
            compressor: u64::MAX,
        };
        let memory = mach_memory(GIB, 16_384, huge, Pressure::Critical).unwrap();
        assert_eq!(memory.used_bytes, GIB);
        assert_eq!(memory.compressed_bytes, Some(GIB));
        // 総量やページサイズが読めていなければ計算しない。
        assert_eq!(mach_memory(0, 16_384, pages, Pressure::Normal), None);
        assert_eq!(mach_memory(GIB, 0, pages, Pressure::Normal), None);
    }

    #[test]
    fn pressure_level_maps_only_the_dispatch_levels() {
        assert_eq!(pressure_from_level(1), Pressure::Normal);
        assert_eq!(pressure_from_level(2), Pressure::Warning);
        assert_eq!(pressure_from_level(4), Pressure::Critical);
        for level in [-1, 0, 3, 5, 8] {
            assert_eq!(pressure_from_level(level), Pressure::Unknown, "{level}");
        }
        // 段階の大小で比べられる（列の最大値を取る用途）。
        assert!(Pressure::Unknown < Pressure::Normal);
        assert!(Pressure::Warning < Pressure::Critical);
    }

    #[test]
    fn sampler_can_move_to_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Sampler>();
    }

    /// 実機で 2 回計測する。間は 1 秒の窓より長く空ける。macOS は host_statistics を OS に付属する
    /// プログラム以外のプロセス全体で 1 秒の窓ごとに数回しか計算し直さないので、同じ窓で読むと、
    /// ほかのプロセス（並行して走る別の cargo test など）が先に呼んでいれば同じ値が返り、CPU の
    /// 差分が 0 になる。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sample_this_machine_twice() -> (Sample, Sample) {
        let mut sampler = Sampler::new();
        let first = sampler.sample(Instant::now());
        std::thread::sleep(Duration::from_millis(1100));
        let second = sampler.sample(Instant::now());
        // 失敗したときに実測値を見られるよう出しておく（成功時は cargo test が捨てる）。
        println!("{second:#?}");
        (first, second)
    }

    /// 実機の計測値として妥当か（OS に依らない項目）を確かめ、メモリを返す。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn assert_plausible(first: &Sample, second: &Sample) -> Memory {
        assert_eq!(first.cpu_percent, None, "初回は差分が無い");
        let cpu = second.cpu_percent.expect("2 回目は CPU 使用率が出る");
        assert!((0.0..=100.0).contains(&cpu), "cpu={cpu}");
        let load = second.load.expect("load average");
        assert!(load.iter().all(|value| *value >= 0.0), "{load:?}");
        assert!(second.logical_cpus.is_some_and(|count| count > 0));
        let swap = second.swap.as_ref().expect("swap");
        assert!(swap.used_bytes <= swap.total_bytes, "{swap:?}");
        if let Some(rate) = swap.out_bytes_per_sec {
            assert!(rate >= 0.0, "{swap:?}");
        }
        let memory = second.memory.clone().expect("memory");
        assert!(memory.total_bytes > 0);
        assert!(memory.used_bytes <= memory.total_bytes, "{memory:?}");
        memory
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sampler_reads_plausible_values_on_macos() {
        let (first, second) = sample_this_machine_twice();
        let memory = assert_plausible(&first, &second);
        assert_ne!(memory.pressure, Pressure::Unknown);
        assert!(
            memory
                .compressed_bytes
                .is_some_and(|bytes| bytes <= memory.total_bytes)
        );
        assert!(
            second
                .swap
                .and_then(|swap| swap.out_bytes_per_sec)
                .is_some()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_reads_plausible_values_on_linux() {
        let (first, second) = sample_this_machine_twice();
        let memory = assert_plausible(&first, &second);
        // Linux には macOS の memorystatus に当たる段階の値が無い。
        assert_eq!(memory.pressure, Pressure::Unknown);
        assert_eq!(memory.compressed_bytes, None);
    }
}
