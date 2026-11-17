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

## Final measurement: the whole Processes page (staging 9ab1de2)

Same PC, release exe of staging 9ab1de2: Processes slices 1 to 8 (with tree #71, detail pane #67,
own graphs #68), the History page with #43, #44 and the #45 settings group, and #90 (the recorder
takes its per-app figures from the live process worker while a page is shown). History recording
**on** (the default now), so every phase includes the recorder.

**The harness counts processor cycles now, not CPU time.** Windows charges CPU time in whole
15.6 ms clock ticks, so threads that wake for a few milliseconds a second were counted as all or
nothing. Two CPU-time runs of the same exe gave 2.2 % and 0.7 % of one core for the Overview, and
the live sampler thread read 1.56 % in one run and 0.00 % in the other. The CPU numbers in the
tables above were taken that way and only tell you the scale. Two cycle-count runs (A and B,
60 s per phase after 10 s warm-up, 3.79 GHz measured cycle rate) agree to within 0.01 points:

| Phase | CPU (of machine) | CPU (of one core) | Frames/s | Working set | Private bytes |
|---|---|---|---|---|---|
| Overview shown | 0.07% | 1.2% | 2.4 | 46.2 to 46.7 MB | 33.2 to 33.6 MB |
| Processes shown | 0.11% | 1.7% | 3.5 | 48.2 to 48.9 MB | 34.6 to 34.9 MB |
| History shown | 0.07% | 1.2% | 2.0 | 49.7 to 49.8 MB | 36.0 to 36.2 MB |
| Hidden (suspended) | 0.02% | 0.2% | 0.0 | 47.2 to 47.3 MB | 33.2 to 33.4 MB |

CPU per thread, % of one core (run A; run B within 0.01):

| Thread | Overview | Processes | History | Hidden |
|---|---|---|---|---|
| main (UI) | 0.29% | 0.40% | 0.24% | 0.00% |
| resource-sampler (live totals, every 1 s) | 0.37% | 0.37% | 0.37% | 0.00% |
| resource-procs (live process table every 2 s, feeds the recorder) | 0.42% | 0.42% | 0.41% | 0.00% |
| process-sampler (Processes page worker, every 2 s) | 0.00% | 0.48% | 0.01% | 0.00% |
| resource-history (recorder, 5 s steps) | 0.07% | 0.07% | 0.07% | 0.24% |
| resource-history-query (History page reads) | 0.00% | 0.00% | 0.06% | 0.00% |
| resource-history-moments (#44) | 0.00% | 0.00% | 0.00% | 0.00% |

`addon-repo measure --runs 5 resource-monitor`: first frame 15 ms (budget 1000 ms), idle memory
42.4 MB (ceiling 50 MB): **met**. `measure` sends no preferences, so the recorder does not run
there. With recording on and every page visited, the working set stays between 46 and 50 MB.

### Reading

- **The Processes page's second process refresh** (`process-sampler`, every 2 s next to
  `resource-procs`) costs **0.48 % of one core, 0.03 % of the machine**. That figure also covers
  the identity checks, the window-owner scan and the row building, which a merge would keep.
  Merging the two refreshes could save about 0.4 % of one core while the page is open, and
  nothing at all when it is not. **Not worth it**; left as it is.
- **The History page** costs the same as the Overview (1.2 % of one core). Its query worker uses
  0.06 %, the #44 moments thread rounds to 0.00 %, and the #45 settings group (size on disk read
  every 5 s) shows nothing measurable on the UI thread. It holds about 2.8 MB more private memory
  than the Overview, for the loaded history.
- **Hidden**, only the recorder runs: 0.24 % of one core (it refreshes the process table itself
  every 5 s when no page feeds it, #90). With recording off, hidden is 0 %.
- No CPU or first-frame regression: the whole add-on stays under 2 % of one core on any page, and
  the first frame is 15 ms.
