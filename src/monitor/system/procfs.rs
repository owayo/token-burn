//! Linux の `/proc` のテキストを解析する純粋関数。
//!
//! ファイルの読み込み（`linux.rs`）と分け、Linux 以外でもコンパイル・テストする。
//! 解析できない値は推測で埋めず、その項目だけ None にする。壊れた値を 0 として読むと、
//! 0% や 0 バイトの実測値に見えてしまう。

use super::{CpuTicks, Memory, Pressure, SwapUsage};

/// `/proc/stat` の先頭の `cpu` 行（全 CPU の合計）から累積 tick を取り出す。
///
/// 列は user nice system idle iowait irq softirq steal guest guest_nice。
/// - 使用中 = user + nice + system + irq + softirq + steal
/// - 待機 = idle + iowait
///
/// guest / guest_nice は user / nice に含まれているので足さない（足すと二重に数える）。
/// 古いカーネルで iowait 以降の列が無いときは 0 とみなす。user〜idle の 4 列が揃わないときと、
/// 使う列に数値でない値があるときは None。
pub(super) fn parse_proc_stat(text: &str) -> Option<CpuTicks> {
    let line = text
        .lines()
        .find(|line| line.split_ascii_whitespace().next() == Some("cpu"))?;
    let mut fields = line.split_ascii_whitespace().skip(1);
    let mut values = [0_u64; 8];
    for (index, slot) in values.iter_mut().enumerate() {
        match fields.next() {
            Some(field) => *slot = field.parse().ok()?,
            None if index >= 4 => break,
            None => return None,
        }
    }
    let [user, nice, system, idle, iowait, irq, softirq, steal] = values;
    let busy = [user, nice, system, irq, softirq, steal]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)?;
    let idle = idle.checked_add(iowait)?;
    Some(CpuTicks::Monotonic64 { busy, idle })
}

/// `/proc/meminfo` のうち計測に使う値（KiB）。読めなかった項目は None。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MemInfo {
    pub(super) mem_total_kib: Option<u64>,
    pub(super) mem_available_kib: Option<u64>,
    pub(super) swap_total_kib: Option<u64>,
    pub(super) swap_free_kib: Option<u64>,
}

/// `/proc/meminfo` を読む。各行は `MemTotal:       16303472 kB` の形。
pub(super) fn parse_meminfo(text: &str) -> MemInfo {
    let mut info = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "MemTotal" => &mut info.mem_total_kib,
            "MemAvailable" => &mut info.mem_available_kib,
            "SwapTotal" => &mut info.swap_total_kib,
            "SwapFree" => &mut info.swap_free_kib,
            _ => continue,
        };
        *slot = parse_kib(rest);
    }
    info
}

/// `       16303472 kB` を読む。単位が kB でなければ（形式が変わった）読まない。
fn parse_kib(rest: &str) -> Option<u64> {
    let mut parts = rest.split_ascii_whitespace();
    let value = parts.next()?.parse().ok()?;
    (parts.next() == Some("kB") && parts.next().is_none()).then_some(value)
}

fn kib_to_bytes(kib: u64) -> Option<u64> {
    kib.checked_mul(1024)
}

/// メモリの使用量 = MemTotal − MemAvailable。
///
/// 解放できるファイルキャッシュは MemAvailable に入るので使用中に数えない。MemFree で引くと、
/// キャッシュでほぼ満杯に見えて、逼迫しているのかを読み取れない。MemAvailable が無い
/// （3.14 より前の）カーネルでは None。
pub(super) fn memory_from_meminfo(info: &MemInfo) -> Option<Memory> {
    let total_bytes = kib_to_bytes(info.mem_total_kib?)?;
    if total_bytes == 0 {
        return None;
    }
    let available_bytes = kib_to_bytes(info.mem_available_kib?)?;
    Some(Memory {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(available_bytes),
        compressed_bytes: None,
        pressure: Pressure::Unknown,
    })
}

/// スワップの使用量 = SwapTotal − SwapFree（`free` と同じ）。スワップが無ければ総量 0 で返す。
pub(super) fn swap_usage_from_meminfo(info: &MemInfo) -> Option<SwapUsage> {
    let total_bytes = kib_to_bytes(info.swap_total_kib?)?;
    let free_bytes = kib_to_bytes(info.swap_free_kib?)?;
    Some(SwapUsage {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(free_bytes),
    })
}

/// `/proc/vmstat` の `pswpout`（起動からスワップアウトしたページ数の累計）。
pub(super) fn parse_vmstat_pswpout(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split_ascii_whitespace().next() == Some("pswpout"))?;
    let mut parts = line.split_ascii_whitespace().skip(1);
    let pages = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    const PROC_STAT: &str = "\
cpu  10132153 290696 3084719 46828483 16683 0 25195 0 175628 0
cpu0 1393280 32966 572056 13343292 6130 0 17875 0 23933 0
cpu1 1335516 46226 495530 13406282 4227 0 2286 0 31287 0
intr 1462898 18 0 0 0 0 0 0 0 1 0 0 0 0 0 0 0
ctxt 115315
btime 1769900000
processes 2915
procs_running 1
procs_blocked 0
softirq 1074436 0 386226 33 113 70345 0 1 297396 0 320322
";

    const MEMINFO: &str = "\
MemTotal:       16303472 kB
MemFree:         1234567 kB
MemAvailable:    9876543 kB
Buffers:          234567 kB
Cached:          5678901 kB
SwapCached:        12345 kB
SwapTotal:       2097148 kB
SwapFree:        1048576 kB
HugePages_Total:       0
Hugepagesize:       2048 kB
";

    const VMSTAT: &str = "\
nr_free_pages 123456
pswpin 1200
pswpout 3400
pgpgin 5600
";

    #[test]
    fn proc_stat_sums_busy_and_idle_columns_of_the_cpu_line() {
        // 使用中 = 10132153 + 290696 + 3084719 + 0 + 25195 + 0（guest の 175628 は user に含まれる）。
        // 待機 = 46828483 + 16683。
        assert_eq!(
            parse_proc_stat(PROC_STAT),
            Some(CpuTicks::Monotonic64 {
                busy: 13_532_763,
                idle: 46_845_166,
            })
        );
    }

    #[test]
    fn proc_stat_treats_missing_trailing_columns_as_zero() {
        // iowait 以降が無い古いカーネル。
        assert_eq!(
            parse_proc_stat("cpu 100 20 30 400\n"),
            Some(CpuTicks::Monotonic64 {
                busy: 150,
                idle: 400,
            })
        );
        assert_eq!(
            parse_proc_stat("cpu 100 20 30 400 5 6 7\n"),
            Some(CpuTicks::Monotonic64 {
                busy: 163,
                idle: 405,
            })
        );
    }

    #[test]
    fn proc_stat_rejects_missing_or_broken_values() {
        assert_eq!(parse_proc_stat(""), None);
        // 合計の cpu 行が無い（CPU ごとの行だけでは全体にならない）。
        assert_eq!(
            parse_proc_stat("cpu0 1 2 3 4 5 6 7 8\ncpu1 1 2 3 4 5 6 7 8\n"),
            None
        );
        // user〜idle が揃わない。
        assert_eq!(parse_proc_stat("cpu 100 20 30\n"), None);
        // 使う列に数値でない値・負の値がある。
        assert_eq!(parse_proc_stat("cpu 100 x 30 400 0 0 0 0\n"), None);
        assert_eq!(parse_proc_stat("cpu 100 20 30 400 -5 0 0 0\n"), None);
        // 合計があふれる。
        assert_eq!(parse_proc_stat(&format!("cpu {} 1 0 0\n", u64::MAX)), None);
    }

    #[test]
    fn proc_stat_ignores_columns_after_steal() {
        // guest 以降は使わないので、将来列が増えても、そこが壊れていても読める。
        assert_eq!(
            parse_proc_stat("cpu 1 2 3 4 5 6 7 8 9 10 11 garbage\n"),
            Some(CpuTicks::Monotonic64 {
                busy: 1 + 2 + 3 + 6 + 7 + 8,
                idle: 4 + 5,
            })
        );
    }

    #[test]
    fn meminfo_gives_memory_without_file_cache_and_swap_usage() {
        let info = parse_meminfo(MEMINFO);
        assert_eq!(
            memory_from_meminfo(&info),
            Some(Memory {
                total_bytes: 16_303_472 * KIB,
                used_bytes: (16_303_472 - 9_876_543) * KIB,
                compressed_bytes: None,
                pressure: Pressure::Unknown,
            })
        );
        assert_eq!(
            swap_usage_from_meminfo(&info),
            Some(SwapUsage {
                total_bytes: 2_097_148 * KIB,
                used_bytes: (2_097_148 - 1_048_576) * KIB,
            })
        );
    }

    #[test]
    fn meminfo_without_mem_available_has_no_memory_but_keeps_swap() {
        let text = MEMINFO.replace("MemAvailable:    9876543 kB\n", "");
        let info = parse_meminfo(&text);
        assert_eq!(info.mem_available_kib, None);
        assert_eq!(memory_from_meminfo(&info), None);
        assert!(swap_usage_from_meminfo(&info).is_some());
    }

    #[test]
    fn meminfo_rejects_broken_values_and_unknown_units() {
        let broken = MEMINFO.replace("16303472 kB", "abc kB");
        assert_eq!(parse_meminfo(&broken).mem_total_kib, None);
        assert_eq!(memory_from_meminfo(&parse_meminfo(&broken)), None);

        let unit = MEMINFO.replace("16303472 kB", "16303472 MB");
        assert_eq!(parse_meminfo(&unit).mem_total_kib, None);

        let no_unit = MEMINFO.replace("SwapFree:        1048576 kB", "SwapFree: 1048576");
        assert_eq!(swap_usage_from_meminfo(&parse_meminfo(&no_unit)), None);

        let zero_total = MEMINFO.replace("16303472 kB", "0 kB");
        assert_eq!(memory_from_meminfo(&parse_meminfo(&zero_total)), None);

        assert_eq!(parse_meminfo(""), MemInfo::default());
    }

    #[test]
    fn meminfo_without_swap_reports_zero_swap() {
        let text = "MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        assert_eq!(
            swap_usage_from_meminfo(&parse_meminfo(text)),
            Some(SwapUsage {
                total_bytes: 0,
                used_bytes: 0,
            })
        );
    }

    #[test]
    fn meminfo_used_never_goes_below_zero() {
        // 各行は同時に作られないので、MemAvailable が MemTotal を上回っても負にしない。
        let text = "MemTotal: 1000 kB\nMemAvailable: 1200 kB\n";
        assert_eq!(
            memory_from_meminfo(&parse_meminfo(text)).map(|memory| memory.used_bytes),
            Some(0)
        );
    }

    #[test]
    fn vmstat_reads_pswpout_pages() {
        assert_eq!(parse_vmstat_pswpout(VMSTAT), Some(3400));
    }

    #[test]
    fn vmstat_rejects_missing_or_broken_pswpout() {
        assert_eq!(parse_vmstat_pswpout(""), None);
        assert_eq!(parse_vmstat_pswpout("pswpin 1200\npgpgout 10\n"), None);
        assert_eq!(parse_vmstat_pswpout("pswpout x\n"), None);
        assert_eq!(parse_vmstat_pswpout("pswpout\n"), None);
        assert_eq!(parse_vmstat_pswpout("pswpout 1 2\n"), None);
        // 名前が前方一致するだけの別の項目は読まない。
        assert_eq!(parse_vmstat_pswpout("pswpout_extra 9\n"), None);
    }
}
