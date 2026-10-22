//! macOS の計測。Mach の `host_statistics` / `host_statistics64` と `sysctlbyname` を使う。
//!
//! ホストポートは [`Probe::new`] で 1 回だけ取り、`Drop` で解放する。`mach_host_self` は呼ぶたびに
//! 送信権の参照を 1 つ増やすので、毎秒取り直すとポートの参照がリークし続ける。
//!
//! `host_statistics` / `host_statistics64` には呼び出し回数の制限がある。OS に付属するプログラム
//! 以外からの呼び出しは、統計の種類ごとに 1 秒の窓あたり数回（窓ごとに変わり、実測では 7 回や
//! 9 回）までしか計算し直されず、それを超えると直前の結果がそのまま、成功（KERN_SUCCESS）として
//! 返る。窓と回数はシステム全体で共有で、別のプロセスが呼び続けている間は、こちらの 1 回目から
//! 古い値になる（いずれも実機で確認）。1 秒ごとの計測なら毎回別の窓の値になるので、古くても
//! 1 秒以内の値で、差分も進む。

use std::ffi::CStr;
use std::mem::{MaybeUninit, offset_of};

use super::{
    CpuTicks, MachVmPages, Pressure, Reading, SwapUsage, mach_memory, pressure_from_level, unix,
};

// libc 0.2 に宣言が無いか、非推奨（mach2 クレートへ移す方針）の Mach の関数と変数。依存は
// 増やさず、libSystem が提供するものをここで宣言する。ヘッダーでの型（ipc_space_t / host_t /
// mach_port_name_t）は、ユーザー空間ではどれも mach_port_t と同じ。
unsafe extern "C" {
    /// このタスクのタスクポート。C の `mach_task_self()` はこの変数を読むだけのマクロ。
    static mut mach_task_self_: libc::mach_port_t;
    /// ホストポートの送信権を返す。呼ぶたびに送信権の参照が 1 つ増える。
    fn mach_host_self() -> libc::mach_port_t;
    /// タスクの IPC 空間から、ポートの送信権の参照を 1 つ解放する。
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
    /// カーネルのページサイズ（`vm_kernel_page_size`）。
    fn host_page_size(
        host: libc::mach_port_t,
        out_page_size: *mut libc::vm_size_t,
    ) -> libc::kern_return_t;
}

const MACH_PORT_NULL: libc::mach_port_t = 0;

/// `vm_statistics64` のうち、読む項目（`internal_page_count` まで）が書かれたと言える件数。
///
/// libc の構造体は新しい macOS で増えた項目まで含むので、古いカーネルは先頭から途中までしか
/// 書かない。書かれなかった項目は 0 のまま残り、それを実測の 0 として読んでしまうため、
/// 返ってきた件数で確かめる。
const VM_INFO64_REQUIRED_COUNT: libc::mach_msg_type_number_t =
    ((offset_of!(libc::vm_statistics64, internal_page_count) + size_of::<libc::natural_t>())
        / size_of::<libc::integer_t>()) as libc::mach_msg_type_number_t;

pub(super) struct Probe {
    /// `mach_host_self` で受け取ったホストポートの送信権。`Drop` で 1 回だけ解放する。
    host: libc::mach_port_t,
}

impl Probe {
    pub(super) fn new() -> Self {
        // SAFETY: mach_host_self は引数を取らず、このタスクにホストポートの送信権を返すだけ。
        let host = unsafe { mach_host_self() };
        Self { host }
    }

    pub(super) fn read(&self) -> Reading {
        let page_size = self.page_size();
        let vm = self.vm_statistics();
        let memory = match (vm, page_size, total_memory()) {
            (Some(vm), Some(page_size), Some(total_bytes)) => {
                mach_memory(total_bytes, page_size, vm_pages(&vm), memory_pressure())
            }
            _ => None,
        };
        let swap_out_bytes = vm
            .zip(page_size)
            .and_then(|(vm, page_size)| vm.swapouts.checked_mul(page_size));
        Reading {
            cpu: self.cpu_ticks(),
            load: unix::load_average(),
            logical_cpus: unix::logical_cpus(),
            memory,
            swap_usage: swap_usage(),
            swap_out_bytes,
        }
    }

    /// 全 CPU を合計した累積 tick（`HOST_CPU_LOAD_INFO`）。
    fn cpu_ticks(&self) -> Option<CpuTicks> {
        let mut info = libc::host_cpu_load_info {
            cpu_ticks: [0; libc::CPU_STATE_MAX as usize],
        };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: info は natural_t が CPU_STATE_MAX 個並んだ書き込み可能な領域で、count にその
        // 件数（HOST_CPU_LOAD_INFO_COUNT）を渡している。カーネルは count 件までしか書かない。
        let result = unsafe {
            libc::host_statistics(
                self.host,
                libc::HOST_CPU_LOAD_INFO,
                (&raw mut info).cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS || count < libc::HOST_CPU_LOAD_INFO_COUNT {
            return None;
        }
        let ticks = info.cpu_ticks;
        let busy = ticks[libc::CPU_STATE_USER as usize]
            .wrapping_add(ticks[libc::CPU_STATE_SYSTEM as usize])
            .wrapping_add(ticks[libc::CPU_STATE_NICE as usize]);
        Some(CpuTicks::Wrapping32 {
            busy,
            idle: ticks[libc::CPU_STATE_IDLE as usize],
        })
    }

    /// ページ数で表した仮想メモリの統計（`HOST_VM_INFO64`）。
    fn vm_statistics(&self) -> Option<libc::vm_statistics64> {
        let mut stat = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: stat は vm_statistics64 の大きさの書き込み可能な領域で、count にその件数
        // （integer_t 単位の HOST_VM_INFO64_COUNT）を渡している。カーネルは count 件までしか書かない。
        let result = unsafe {
            libc::host_statistics64(
                self.host,
                libc::HOST_VM_INFO64,
                stat.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS || count < VM_INFO64_REQUIRED_COUNT {
            return None;
        }
        // SAFETY: zeroed で全体を初期化したうえで、カーネルが先頭から count 件を書いた。
        // vm_statistics64 は整数だけの構造体なので、どのビット列も有効な値になる。
        Some(unsafe { stat.assume_init() })
    }

    /// `vm_statistics64` のページ数の単位。
    ///
    /// ページ数はカーネルのページ単位で数えられるので、`vm_stat` と同じく host_page_size を使う。
    /// sysconf のページサイズはプロセスから見た値で、カーネルの単位と一致するとは限らないため、
    /// 失敗しても sysconf へは戻さず None にする（誤った単位で換算するより、読めない方がよい）。
    fn page_size(&self) -> Option<u64> {
        let mut size: libc::vm_size_t = 0;
        // SAFETY: size は書き込み可能な vm_size_t で、host_page_size はそこへ 1 つ書くだけ。
        let result = unsafe { host_page_size(self.host, &mut size) };
        if result != libc::KERN_SUCCESS || size == 0 {
            return None;
        }
        u64::try_from(size).ok()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.host == MACH_PORT_NULL {
            return;
        }
        // SAFETY: mach_task_self_ は libSystem がプロセスの開始時に設定する変数で、値を読むだけ
        // （参照は作らない）。host は new で mach_host_self から受け取った送信権で、ここで 1 回だけ
        // 解放する（Probe は Clone できないので、同じ権利を 2 度解放することはない）。失敗しても
        // Drop では打つ手が無いので結果は見ない。
        unsafe {
            mach_port_deallocate(mach_task_self_, self.host);
        }
    }
}

/// Activity Monitor の「使用済みメモリ」の計算に使う項目を取り出す。
fn vm_pages(vm: &libc::vm_statistics64) -> MachVmPages {
    MachVmPages {
        internal: u64::from(vm.internal_page_count),
        purgeable: u64::from(vm.purgeable_count),
        wired: u64::from(vm.wire_count),
        compressor: u64::from(vm.compressor_page_count),
    }
}

/// 物理メモリの総量（`hw.memsize`）。
fn total_memory() -> Option<u64> {
    // SAFETY: u64 はどのビット列も有効な値。
    unsafe { sysctl_read::<u64>(c"hw.memsize") }
}

/// スワップ領域の大きさと使用量（`vm.swapusage`）。
fn swap_usage() -> Option<SwapUsage> {
    // SAFETY: xsw_usage は整数だけの構造体で、どのビット列も有効な値。
    let usage = unsafe { sysctl_read::<libc::xsw_usage>(c"vm.swapusage") }?;
    Some(SwapUsage {
        total_bytes: usage.xsu_total,
        used_bytes: usage.xsu_used.min(usage.xsu_total),
    })
}

/// メモリの逼迫度（`kern.memorystatus_vm_pressure_level`）。一般ユーザーの権限で読める。
fn memory_pressure() -> Pressure {
    // SAFETY: c_int はどのビット列も有効な値。
    unsafe { sysctl_read::<libc::c_int>(c"kern.memorystatus_vm_pressure_level") }
        .map_or(Pressure::Unknown, pressure_from_level)
}

/// `sysctlbyname` で固定長の値を 1 つ読む。返ってきた長さが型の大きさと違えば None。
///
/// # Safety
///
/// `T` は、どのビット列も有効な値になる型（整数や、整数だけからなる `#[repr(C)]` の構造体）で
/// あること。カーネルが書いたバイト列をそのまま `T` として読むため。
unsafe fn sysctl_read<T: Copy>(name: &CStr) -> Option<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    // SAFETY: name は NUL 終端の文字列。value は T の大きさの書き込み可能な領域で、len にその
    // 大きさを渡しているので、カーネルは len バイトまでしか書かない。新しい値は設定しない
    // （newp は null、newlen は 0）。
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || len != size_of::<T>() {
        return None;
    }
    // SAFETY: zeroed で初期化したうえで、カーネルが T の大きさぶんを書いた。T がどのビット列でも
    // 有効な型であることは呼び出し側が保証する。
    Some(unsafe { value.assume_init() })
}
