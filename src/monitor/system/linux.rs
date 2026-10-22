//! Linux の計測。`/proc` を読み、解析は `procfs.rs` の純粋関数に任せる。
//!
//! メモリの逼迫度は Unknown のまま。macOS の `memorystatus` に当たる段階の値が無いため。

use std::fs;

use super::{Reading, procfs, unix};

/// Linux では保持する OS の資源が無い。
pub(super) struct Probe;

impl Probe {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn read(&self) -> Reading {
        let meminfo = read_proc("/proc/meminfo").map(|text| procfs::parse_meminfo(&text));
        // pswpout はページ数。ページサイズはカーネルとプロセスで同じ。
        let swap_out_bytes = read_proc("/proc/vmstat")
            .and_then(|text| procfs::parse_vmstat_pswpout(&text))
            .zip(unix::page_size())
            .and_then(|(pages, page_size)| pages.checked_mul(page_size));
        Reading {
            cpu: read_proc("/proc/stat").and_then(|text| procfs::parse_proc_stat(&text)),
            load: unix::load_average(),
            logical_cpus: unix::logical_cpus(),
            memory: meminfo.as_ref().and_then(procfs::memory_from_meminfo),
            swap_usage: meminfo.as_ref().and_then(procfs::swap_usage_from_meminfo),
            swap_out_bytes,
        }
    }
}

/// `/proc` のファイルを読む。読めなければ、その項目を None にする。
fn read_proc(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}
