//! The real reading source: sysinfo plus the GPU counters, read-only.
//!
//! While Resource Monitor's page is open its live process worker already refreshes the process
//! table; the recorder then takes those refreshes from [`super::feed`] and does not refresh the
//! table itself (#90). It refreshes it only when the page is closed or hidden. The first own
//! refresh after the page closes covers the whole time since the previous own refresh (the
//! process counters are deltas against that refresh), so its per-app figures are averages over
//! that time: per-app disk rates divide by it, never by one step, so they cannot spike.
//!
//! It opens processes only as sysinfo does for the existing Processes and Top lists
//! (query-information rights, to read times, memory and I/O counters). It never asks for a
//! right to write to, end or change a process; acting on processes stays behind
//! `protected-processes`. Only the exe name (`Process::name`) is kept from each process.
//!
//! GPU temperature and power are not read here: they need NVIDIA's driver library, which adds
//! about 19 MB of private memory to the process (measured: 26 MB with it, 7 MB without). The
//! live page still shows them; history records them as unavailable.

use std::collections::HashMap;
use std::time::Instant;

use sysinfo::{DiskRefreshKind, Disks, Networks, System};

use super::apps::ProcReading;
use super::core::Reading;
use super::record::SystemTotals;
use super::Source;
use crate::gpu::GpuReader;
use crate::sampler::{
    bytes_per_second, cpu_percent_of_machine, disk_totals, net_totals, refresh_procs,
};

pub struct SystemSource {
    sys: System,
    /// Asks the live worker for its refreshes while this source exists.
    _feed: super::feed::Active,
    /// Process-table refreshes this source made itself (tests and measurements).
    pub(crate) own_refreshes: u64,
    /// When this source last refreshed the process table itself: the per-process counters
    /// are deltas against that refresh.
    own_last: Instant,
    /// Seconds the per-app figures of the newest own refresh cover (tests).
    pub(crate) own_secs: f64,
    disks: Option<Disks>,
    nets: Option<Networks>,
    gpu: GpuReader,
    logical: usize,
    last: Instant,
}

fn disk_kind() -> DiskRefreshKind {
    DiskRefreshKind::nothing().with_io_usage()
}

impl SystemSource {
    /// Creates the source and takes the baseline reading, so the first real reading has deltas.
    pub fn new() -> Option<Self> {
        let mut sys = System::new().ok()?;
        sys.refresh_cpu_all();
        sys.refresh_memory();
        refresh_procs(&mut sys);
        let feed = super::feed::Active::new();
        super::feed::clear();
        let logical = sys.cpus().len();
        let mut gpu = GpuReader::new();
        let _ = gpu.read();
        Some(Self {
            sys,
            _feed: feed,
            own_refreshes: 0,
            own_last: Instant::now(),
            own_secs: 0.0,
            disks: Disks::new_with_refreshed_list_specifics(disk_kind()).ok(),
            nets: Networks::new_with_refreshed_list().ok(),
            gpu,
            logical,
            last: Instant::now(),
        })
    }
}

impl Source for SystemSource {
    fn read(&mut self) -> Reading {
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        let secs = self.last.elapsed().as_secs_f64();
        // The live worker's refreshes when the page is open; our own otherwise.
        let fed = super::feed::take(secs);
        if fed.is_none() {
            self.own_secs = self.own_last.elapsed().as_secs_f64();
            refresh_procs(&mut self.sys);
            self.own_last = Instant::now();
            self.own_refreshes += 1;
        }
        let own_secs = self.own_secs;
        self.last = Instant::now();
        let (read, written) = match self.disks.as_mut() {
            Some(d) => {
                d.refresh_specifics(true, disk_kind());
                disk_totals(d)
            }
            None => (0, 0),
        };
        let (down, up) = match self.nets.as_mut() {
            Some(n) => {
                n.refresh(true);
                net_totals(n)
            }
            None => (0, 0),
        };
        let mut gpu = self.gpu.read();
        let per_pid: HashMap<u32, f32> = gpu
            .as_mut()
            .map(|g| std::mem::take(&mut g.per_process))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let totals = SystemTotals {
            cpu: self.sys.global_cpu_usage(),
            mem_used: self
                .sys
                .total_memory()
                .saturating_sub(self.sys.available_memory()),
            mem_total: self.sys.total_memory(),
            gpu_load: gpu.as_ref().and_then(|g| g.load),
            gpu_mem_used: gpu.as_ref().and_then(|g| g.mem_used),
            gpu_temp_c: None,
            gpu_power_w: None,
            disk_read_bps: bytes_per_second(read, secs) as f32,
            disk_write_bps: bytes_per_second(written, secs) as f32,
            net_down_bps: bytes_per_second(down, secs) as f32,
            net_up_bps: bytes_per_second(up, secs) as f32,
        };
        let logical = self.logical;
        if let Some(fed) = fed {
            let procs = fed
                .into_iter()
                .map(|(pid, mut p)| {
                    p.gpu = per_pid.get(&pid).copied().unwrap_or(0.0);
                    p
                })
                .collect();
            return Reading { totals, procs };
        }
        let procs = self
            .sys
            .processes()
            .iter()
            .filter(|(pid, _)| pid.as_u32() != 0)
            .map(|(pid, p)| {
                let io = p.disk_usage();
                // Windows gives no I/O counters for a process it will not open (protected
                // processes without admin rights): every total is then 0. Unknown, not zero.
                let known = io.total_read_bytes > 0 || io.total_written_bytes > 0;
                let rate = |b: u64| known.then(|| bytes_per_second(b, own_secs) as f32);
                ProcReading {
                    name: p.name().to_string_lossy().into_owned(),
                    cpu: cpu_percent_of_machine(p.cpu_usage(), logical) as f32,
                    mem: p.memory(),
                    gpu: per_pid.get(&pid.as_u32()).copied().unwrap_or(0.0),
                    disk_read_bps: rate(io.read_bytes),
                    disk_write_bps: rate(io.written_bytes),
                }
            })
            .collect();
        Reading { totals, procs }
    }
}

#[cfg(test)]
mod measure {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::sampler::{Sampler, PROCESS_TABLE_REFRESHES};

    /// #90 measurement, not part of the gate: the live sampler as with the page open (pinged
    /// every 250 ms) and the history recorder with the real system source at its real 5 s step,
    /// saving nothing. Prints the process-table refreshes per minute.
    ///
    /// `cargo test -p resource-monitor --lib -- --ignored --nocapture process_table_refreshes`
    /// (`MEASURE_SECS`, default 60).
    #[test]
    #[ignore]
    fn process_table_refreshes_with_the_page_open_and_recording_on() {
        let _l = super::super::feed::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let secs: u64 = std::env::var("MEASURE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let ctx = egui::Context::default();
        let sampler = Sampler::start(&ctx);
        let recorder = super::super::Recorder::spawn(super::super::STEP, || {
            SystemSource::new().map(|s| Box::new(s) as Box<dyn Source>)
        })
        .expect("recorder");
        let run_for = |d: Duration| {
            let until = Instant::now() + d;
            while Instant::now() < until {
                sampler.ping();
                let _ = sampler.drain();
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        // Warm-up: both threads primed and running at their normal pace.
        run_for(Duration::from_secs(12));
        let start = PROCESS_TABLE_REFRESHES.load(Ordering::Relaxed);
        let steps0 = recorder.store.lock().unwrap().fine().len();
        let t0 = Instant::now();
        run_for(Duration::from_secs(secs));
        let n = PROCESS_TABLE_REFRESHES.load(Ordering::Relaxed) - start;
        let steps = recorder.store.lock().unwrap().fine().len() - steps0;
        let minutes = t0.elapsed().as_secs_f64() / 60.0;
        println!(
            "process-table refreshes: {n} in {:.0} s = {:.1} per minute; recorder steps: {steps}",
            minutes * 60.0,
            n as f64 / minutes
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// #90: with the page open (the live worker refreshing the table and feeding it), a
    /// recorder step makes no process-table refresh of its own; with the page closed it does.
    #[test]
    fn with_the_page_open_the_recorder_refreshes_no_process_table() {
        let _l = super::super::feed::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut src = SystemSource::new().expect("source");
        let mut live = System::new().expect("system");
        refresh_procs(&mut live);
        let logical = live.cpus().len().max(1);
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(400));
            refresh_procs(&mut live);
            crate::sampler::feed_recorder(&live, logical, 0.4);
        }
        let r = src.read();
        assert_eq!(src.own_refreshes, 0, "the live refreshes were used");
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(
            r.procs.iter().any(|p| p.name.to_lowercase() == me),
            "this test process is in the reading"
        );
        // The page closes: no more live refreshes, so the recorder reads the table itself.
        std::thread::sleep(Duration::from_millis(300));
        let r = src.read();
        assert_eq!(src.own_refreshes, 1);
        assert!(!r.procs.is_empty());
        // Its process counters are deltas since its own refresh at creation (about 1.5 s ago),
        // so the per-app rates divide by that time, not by the 0.3 s since the last step:
        // bytes moved while the page was open are never squeezed into one step.
        assert!(src.own_secs >= 1.4, "{}", src.own_secs);
        // The next own step covers just its own step again.
        std::thread::sleep(Duration::from_millis(300));
        src.read();
        assert_eq!(src.own_refreshes, 2);
        assert!(src.own_secs < 1.0, "{}", src.own_secs);
    }
}
