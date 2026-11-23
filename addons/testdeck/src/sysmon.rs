//! Real `LoadSource` on `sysinfo`: CPU rolling 10 s average, free/total RAM, and CPU/RAM per running ticket's
//! process tree. The broker's tick calls `refresh` once a second.
use crate::scheduler::{Secs, Ticket};
use crate::state::{JobLoad, Load, LoadPoint, LoadSource};
use crate::winproc::tree;
use std::collections::VecDeque;
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System,
};

const CPU_WINDOW_SECS: Secs = 10.0;
const GIB: f32 = 1024.0 * 1024.0 * 1024.0;
const MIB: f32 = 1024.0 * 1024.0;

pub struct SysMon {
    sys: System,
    cpu: VecDeque<(Secs, f32)>,
}

impl Default for SysMon {
    fn default() -> Self {
        let kind = RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::nothing().with_ram());
        Self {
            sys: System::new_with_specifics(kind),
            cpu: VecDeque::new(),
        }
    }
}

impl LoadSource for SysMon {
    fn refresh(&mut self, now: Secs, pids: &[(Ticket, u32)], load: &mut Load) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let cpu = self.sys.global_cpu_usage();
        self.cpu.push_back((now, cpu));
        while self
            .cpu
            .front()
            .is_some_and(|&(t, _)| now - t > CPU_WINDOW_SECS)
        {
            self.cpu.pop_front();
        }
        load.cpu_avg = self.cpu.iter().map(|&(_, c)| c).sum::<f32>() / self.cpu.len() as f32;
        load.ram_free_gb = self.sys.available_memory() as f32 / GIB;
        load.ram_total_gb = self.sys.total_memory() as f32 / GIB;
        load.push(LoadPoint {
            t: now,
            cpu,
            ram_free_gb: load.ram_free_gb,
        });

        load.per_ticket.clear();
        crate::winproc::forget_jobs_except(&pids.iter().map(|&(_, p)| p).collect::<Vec<_>>());
        if pids.is_empty() {
            return; // The process list is the costly part; skip it when nothing runs.
        }
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let procs = self.sys.processes();
        let pairs: Vec<(u32, u32)> = procs
            .iter()
            .filter_map(|(pid, p)| p.parent().map(|pp| (pid.as_u32(), pp.as_u32())))
            .collect();
        // sysinfo reports process CPU per core; the UI shows percent of the whole machine.
        let cores = self.sys.cpus().len().max(1) as f32;
        for &(ticket, root) in pids {
            let (mut cpu, mut mem) = (0.0f32, 0u64);
            for pid in tree(root, &pairs) {
                if let Some(p) = procs.get(&Pid::from_u32(pid)) {
                    cpu += p.cpu_usage();
                    mem += p.memory();
                }
            }
            load.per_ticket.insert(
                ticket,
                JobLoad {
                    cpu: cpu / cores,
                    ram_mb: mem as f32 / MIB,
                },
            );
        }
    }
}
