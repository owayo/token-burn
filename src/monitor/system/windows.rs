//! Windows CPU and physical memory counters. Unavailable Unix load/swap counters stay absent.
use super::{CpuTicks, Memory, Pressure, Reading};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::GetSystemTimes,
    },
};

pub(super) struct Probe;

impl Probe {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn read(&self) -> Reading {
        let mut reading = Reading {
            logical_cpus: std::thread::available_parallelism().ok().map(usize::from),
            ..Default::default()
        };
        unsafe {
            let mut idle: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0 {
                let ticks = |time: FILETIME| {
                    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
                };
                let idle = ticks(idle);
                if let Some(busy) = ticks(kernel)
                    .checked_add(ticks(user))
                    .and_then(|total| total.checked_sub(idle))
                {
                    reading.cpu = Some(CpuTicks::Monotonic64 { busy, idle });
                }
            }
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of_val(&status) as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 && status.ullTotalPhys > 0 {
                reading.memory = Some(Memory {
                    total_bytes: status.ullTotalPhys,
                    used_bytes: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
                    compressed_bytes: None,
                    pressure: Pressure::Unknown,
                });
            }
        }
        reading
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_cpu_and_physical_memory() {
        let reading = super::Probe::new().read();
        assert!(reading.cpu.is_some());
        let memory = reading.memory.unwrap();
        assert!(memory.total_bytes > 0 && memory.used_bytes <= memory.total_bytes);
    }
}
