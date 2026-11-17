# Resource Monitor: Processes page overhead (#70)

Measured 2026-11-17 on the 32 GB PC (i7-10700K, 16 logical CPUs, RTX 3080), not elevated,
release exes, memory compression off on this PC. Other cargo builds were running at the same time.

## Method

- **Budgets**: `addon-repo measure --runs 5 resource-monitor` (median of 5 starts; idle memory is
  the working set 3 s after the first frame on the Overview, nothing navigated).
- **CPU and memory per page**: `addons/resource-monitor/tests/overhead.rs` (ignored test). It
  drives the exe like the host: one frame per repaint request, at most 60 per second, a ping every
  second, 1100 x 700 points. Each phase: 10 s warm-up, then 60 s measured. CPU is the add-on's own
  kernel plus user time (GetProcessTimes); memory is GetProcessMemoryInfo at the end of the phase.
  History recording is off (its default), so "hidden" has no recorder thread.

```text
ADDON_TEST_EXE=<release exe> OVERHEAD_DATA=<fresh folder> \
  cargo test --release -p resource-monitor --test overhead -- --ignored --nocapture
```

## Results

Builds: **before** = staging 8d47412 (before Processes slice 1, #61); **8/9** = staging bdac9e1
(slices 1 disk, 2 GPU, 3 connections, 8 memory breakdown); **now** = staging 20f9ed9 (also slice 4
groups, #66). Slices 5 (tree, #71), 6 (detail facts, #67) and 7 (graphs, #68) were not in staging
yet and are not measured.

| Build | Phase | CPU (of machine) | CPU (of one core) | Frames/s | Working set | Private bytes |
|---|---|---|---|---|---|---|
| before | Overview shown | 0.08% | 1.3% | 2.0 | 44.8 MB | 32.4 MB |
| before | Processes shown | 0.09% | 1.5% | 2.4 | 46.4 MB | 31.6 MB |
| before | Hidden (suspended) | 0.00% | 0.0% | 0.0 | 44.8 MB | 29.4 MB |
| 8/9 | Overview shown | 0.09% | 1.5% | 2.0 | 45.9 MB | 32.7 MB |
| 8/9 | Processes shown | 0.11% | 1.7% | 2.4 | 48.5 MB | 33.2 MB |
| 8/9 | Hidden (suspended) | 0.00% | 0.0% | 0.0 | 46.4 MB | 30.3 MB |
| now | Overview shown | 0.10% | 1.5% | 2.1 | 47.8 MB | 32.1 MB |
| now | Processes shown | 0.13% | 2.0% | 3.0 | 48.9 MB | 33.5 MB |
| now | Hidden (suspended) | 0.00% | 0.0% | 0.0 | 46.5 MB | 30.3 MB |

| Build | Exe size | First frame | Idle memory (working set) | Private bytes | Budgets |
|---|---|---|---|---|---|
| before | 3.7 MB | 17 ms | 42.3 MB | 29.3 MB | idle memory over 25.0 MB |
| 8/9 | 3.8 MB | 16 ms | 42.2 MB | 29.2 MB | idle memory over 25.0 MB |
| now | 3.8 MB | 14 ms | 42.4 MB | 29.4 MB | idle memory over 25.0 MB |

For comparison, the same tool: `hello` 7.8 MB, `aural` 10.7 MB idle memory (both within budget).

## Reading

- The Processes page costs about 0.03% of the machine (0.5 points of one core) and 1.4 MB private
  memory more than the Overview. Slices 1 to 4 added about 0.04% of the machine and 2 MB private
  bytes on that page. Hidden, the add-on uses no CPU.
- The two process refreshes (page worker and sampler) together stay near 2% of one core with the
  page open, so merging them is not called for.
- **Idle memory is over the 25 MB budget in every build, including the one before the Processes
  work** (42 MB working set, about 29 MB private). The Processes slices did not cause it. NVIDIA's
  `nvml.dll` is the largest loaded module (22 MB image), but skipping it in a diagnostic build saved
  only about 3 MB of working set and no private bytes, so most of the cost is elsewhere (likely
  the sampler's PDH counters and process table, or the UI runtime). Not attributed further here.

## Budget decision

Decided 2026-11-17 for the owner: the unattributed memory is not chased in #70 and not hidden.
`budgets.json` gives Resource Monitor its own idle memory ceiling of **50 MB**: the measured
42.4 MB baseline plus headroom. It was already 42.3 MB before the Processes work, and history
recording (off in these runs) adds to it. The ceiling is a guard against growth, not a target.
The first-frame budget (1000 ms) and the CPU numbers are unchanged.
