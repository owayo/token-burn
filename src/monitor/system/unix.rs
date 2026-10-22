//! macOS と Linux で共通の POSIX 呼び出し（ロードアベレージ・論理 CPU 数・ページサイズ）。

/// `getloadavg` の 1/5/15 分平均。3 つ揃わないとき・負や有限でない値が混じるときは None。
pub(super) fn load_average() -> Option<[f64; 3]> {
    let mut loads = [0.0_f64; 3];
    // SAFETY: `loads` は f64 が 3 つ並んだ書き込み可能な配列で、getloadavg は第 2 引数（3）個
    // までしか書かない。
    let filled = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    (filled == 3 && loads.iter().all(|load| load.is_finite() && *load >= 0.0)).then_some(loads)
}

/// オンラインの論理 CPU 数（`sysconf(_SC_NPROCESSORS_ONLN)`）。
pub(super) fn logical_cpus() -> Option<usize> {
    sysconf(libc::_SC_NPROCESSORS_ONLN).and_then(|count| usize::try_from(count).ok())
}

/// このプロセスから見たページサイズ（`sysconf(_SC_PAGESIZE)`）。4096 と決め打ちしない。
///
/// Linux 専用。macOS の Mach の統計はカーネルのページ単位で数えるので `host_page_size` を使い、
/// プロセスから見たページサイズへは戻さない（両者が一致するとは限らず、誤った単位で換算するより
/// 読めない方がよい）。
#[cfg(target_os = "linux")]
pub(super) fn page_size() -> Option<u64> {
    sysconf(libc::_SC_PAGESIZE).and_then(|size| u64::try_from(size).ok())
}

/// 正の値だけを返す。-1 は未対応の名前か失敗。
fn sysconf(name: libc::c_int) -> Option<libc::c_long> {
    // SAFETY: sysconf は名前の整数を受け取って値を返すだけで、呼び出し元のメモリには触れない。
    // 未知の名前には -1 を返す。
    let value = unsafe { libc::sysconf(name) };
    (value > 0).then_some(value)
}
