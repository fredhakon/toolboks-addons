//! Runs the six checks on this PC and prints the results. Read-only:
//! `cargo run -p pc-checks --example run`

#[cfg(windows)]
fn main() {
    use pc_checks::windows_source::WindowsSource;
    use pc_checks::{
        run_all, unix_now, GpuDriverSource, MemoryIntegritySource, MemorySource, Outcome,
        PowerSource, RestartSource,
    };

    // With `--raw`, also print the readings the verdicts are made from.
    if std::env::args().any(|a| a == "--raw") {
        let s = WindowsSource;
        println!("raw modules:   {:?}", s.modules());
        println!("raw plan:      {:?}", s.active_plan());
        println!("raw mode:      {:?}", s.power_mode());
        println!("raw status:    {:?}", s.status());
        println!("raw integrity: {:?}", s.state());
        println!(
            "raw restart:   {:?} {:?}",
            s.windows_update_reboot_required(),
            s.servicing_reboot_pending()
        );
        println!("raw gpu:       {:?}", s.drivers());
    }

    for r in run_all(&WindowsSource, &unix_now) {
        let what = match &r.outcome {
            Outcome::Ok => "OK".to_string(),
            Outcome::Finding(f) => format!(
                "FINDING ({:?}): {} -> {}",
                f.severity, f.observation, f.action
            ),
            Outcome::NotChecked { reason } => format!("NOT CHECKED: {reason}"),
            Outcome::Unavailable { reason } => format!("UNAVAILABLE: {reason}"),
        };
        println!("{} [{}] {}", r.check.title(), r.measured_at_iso(), what);
    }
}

#[cfg(not(windows))]
fn main() {
    println!("These checks read Windows; nothing to run here.");
}
