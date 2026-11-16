# pc-checks

Read-only checks for the settings that most often make a PC slow (host issue
[Gusseti/toolboks#56](https://github.com/Gusseti/toolboks/issues/56)). This is the check engine only:
no page, no protocol, no add-on wiring.

## Rules every check follows

- **Read only.** Nothing here changes a setting, writes to the registry, runs `powercfg /set`,
  starts another program, or touches drivers or the BIOS. Registry keys are opened with `KEY_READ`.
- **No administrator rights.** All six checks run on a normal account. If Windows ever refuses a
  read, the result is `NotChecked` with that reason, never a guess.
- **One of four answers:** `Ok`, `Finding` (plain observation, severity, one action),
  `NotChecked` (this PC has it but it could not be read) or `Unavailable` (does not apply here).
  A missing or zero reading is never `Ok`.
- **Plain text.** Findings are one line, no markup characters, at most 320 characters
  (reasons 200). There is no health score. Each result carries `measured_at` (Unix seconds, UTC).
- **Testable.** Each check is a function over a small trait (`MemorySource`, `PowerSource`,
  `MemoryIntegritySource`, `ThrottleSource`, `RestartSource`, `GpuDriverSource`); the tests use fakes
  for every branch. `windows_source::WindowsSource` is the real implementation.

The result types in `result.rs` are provisional: the design for how findings reach the Home page
(`docs/PLAN-health.md` in the host repo) was not merged when this was written.

Try it on this PC: `cargo run -p pc-checks --example run` (add `-- --raw` to see the raw readings).

## What each check reads (for the privacy page)

None of these read files, documents, browsing data or anything that identifies the user. Nothing is
sent anywhere by this crate.

### 1. Memory speed (`ram-speed`)

Reads the **SMBIOS memory table** with `GetSystemFirmwareTable('RSMB')`. This is the table Windows
turns into the WMI class `Win32_PhysicalMemory`; the crate reads the table directly (no WMI, no
PowerShell). Per installed module it uses `Speed` (rated, MT/s), the configured memory speed
(`ConfiguredClockSpeed`) and the slot label (`DeviceLocator`). The size field is only used to skip empty
slots; serial and part numbers are not read.

A module with a rated or configured speed of 0 or missing makes the check `NotChecked`.
**Limit:** the BIOS decides what it reports as "rated". Some boards report the module's base (JEDEC)
speed there even when it has an XMP or EXPO profile, so the check can only see a gap when the BIOS
reports the higher profile speed as the rated speed. It never says Ok on unreadable data.

### 2. Power plan, power mode, battery saver (`power-plan`)

- `PowerGetActiveScheme` (powrprof): the active plan's GUID, matched against Power saver, Balanced,
  High performance and Ultimate Performance. Any other plan is not judged.
- `PowerGetEffectiveOverlayScheme` (powrprof, looked up by name at run time): the Power mode slider
  ("Best power efficiency", "Balanced", ...). A Windows without it is fine.
- `GetSystemPowerStatus` (kernel32): plugged in or on battery, and the Battery saver flag.

No `powercfg` and no registry. Saving settings are only a finding while the PC is plugged in (on
battery they are the user's choice); Battery saver is reported whenever it is on.

### 3. Memory integrity (`memory-integrity`)

Registry value `Enabled` under
`HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity`.
This is the setting as saved; a change made since the last restart is not running yet. If Windows
has no saved setting the check is `NotChecked`.

When it is on, the finding is `Info` and says honestly that it protects against malicious drivers and
can cost a few percent in some games, mostly on older processors. It never recommends turning it off.

### 4. CPU throttling (`cpu-throttling`)

Windows performance counters through PDH (`PdhAddEnglishCounterW`, so it works in any language):

- `\Processor Information(_Total)\% Performance Limit` (100 means nothing holds the CPU back)
- `\Processor Information(_Total)\% of Maximum Frequency` (reported for context only: it is low at
  idle on a healthy PC)

**Sample window: 6 readings, 500 ms apart, about 3 seconds**, averaged (`SAMPLE_COUNT`,
`SAMPLE_INTERVAL`). Throttled means an average below 90% (`THROTTLED_BELOW_PERCENT`). A short sample
only shows what the CPU does right now; the finding says to run the check again while the PC is busy.
If `% Performance Limit` returns no value, the check is `NotChecked`.

### 5. Pending restart (`pending-restart`)

Whether these registry **keys exist** (their contents are not read):

- `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired`
- `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending`

`PendingFileRenameOperations` is deliberately not used: installers and antivirus set it all the time,
so it is not "after Windows Update". A missing key means no restart is pending; a key that cannot be
opened (other than missing) makes the check `NotChecked`.

### 6. Graphics driver age (`gpu-driver-age`)

For each numbered adapter key (`0000`, `0001`, ...) under
`HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}` (the Display
class) it reads the values `DriverDesc`, `ProviderName`, `DriverDate` (and `DriverDateData` as a
fallback). Remote and virtual adapters are ignored. The basic Microsoft display driver alone is its
own finding.

**"Old" is `GPU_DRIVER_OLD_AFTER_DAYS` = 365 days.** Vendors ship a driver about monthly, so being a
few months behind is normal and not worth a card; a year behind means a year of game fixes and
security updates missed. Windows Update also installs older, vendor-approved drivers, so a tighter
limit would flag healthy PCs. Over two years is Medium instead of Low. A driver date in the future is
not trusted (`NotChecked`).
**Limit:** a graphics card that was removed can leave its adapter key behind in the registry.

## Layout

| File | What |
|---|---|
| `src/result.rs` | `CheckResult`, `Outcome`, `Finding`, `Severity`, `ReadError`, text cleaning |
| `src/{ram,power,memory_integrity,throttle,restart,gpu_driver}.rs` | the six checks, their traits and tests |
| `src/smbios.rs` | SMBIOS memory-device parser (pure bytes, tested with fixtures) |
| `src/date.rs` | small calendar helpers (no date crate) |
| `src/windows_source.rs` | the real Windows implementation (`cfg(windows)`) |
| `examples/run.rs` | prints the six results on this PC |
