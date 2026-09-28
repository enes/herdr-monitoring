//! Local OS process snapshots and resource readings.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System,
    UpdateKind,
};

use super::SystemCapacity;

#[derive(Clone)]
pub(super) struct ProcessMeta {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cmdline: String,
    pub children: Vec<u32>,
}

pub(super) struct ProcessReading {
    pub started: Option<SystemTime>,
    pub rss: u64,
    pub cpu_time: f64,
}

pub(super) struct SystemSource {
    sys: System,
}

impl SystemSource {
    pub fn new() -> Self {
        let mut sys = System::new();
        // Capacity is stable during this monitor's lifetime. Do not sample
        // system CPU usage or change the process collector's CPU baseline.
        sys.refresh_cpu_list(CpuRefreshKind::nothing());
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        Self { sys }
    }

    pub fn capacity(&self) -> SystemCapacity {
        SystemCapacity {
            logical_cpus: self.sys.cpus().len(),
            memory_bytes: self.sys.total_memory(),
        }
    }

    pub fn refresh(&mut self) -> Vec<ProcessMeta> {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always)
                .with_cpu()
                .with_memory(),
        );
        self.sys
            .processes()
            .iter()
            .map(|(pid, process)| ProcessMeta {
                pid: pid.as_u32(),
                ppid: process.parent().map(|pid| pid.as_u32()).unwrap_or(0),
                name: process.name().to_string_lossy().into_owned(),
                cmdline: process
                    .cmd()
                    .iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                children: Vec::new(),
            })
            .collect()
    }

    pub fn read(&self, pid: u32) -> Option<ProcessReading> {
        let process = self.sys.process(Pid::from_u32(pid))?;
        Some(ProcessReading {
            started: (process.start_time() != 0)
                .then(|| UNIX_EPOCH + Duration::from_secs(process.start_time())),
            rss: process.memory(),
            cpu_time: process.accumulated_cpu_time() as f64 / 1000.0,
        })
    }
}
