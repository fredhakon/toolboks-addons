//! The memory breakdown under the Overview memory graph: committed, cached, paged and non-paged
//! pool from `GetPerformanceInfo` (no admin needed, read with the counts every tick), and
//! compressed memory when the Memory Compression process can be read without admin.

use egui::{self, RichText};
use toolboks_shared::format::size_plain;
use toolboks_shared::ui::{card, TEXT_SECTION};

/// The system process that holds compressed memory. It has no exe path and its parent is the
/// System process (pid 4); a user exe can share the name but not that parent.
const COMPRESSION_NAME: &str = "Memory Compression";
const SYSTEM_PID: u32 = 4;

/// Memory sizes in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryBreakdown {
    /// Memory the system has promised to programs (`CommitTotal`).
    pub committed: u64,
    /// The most that can be committed before the page file must grow (`CommitLimit`).
    pub commit_limit: u64,
    /// File data Windows keeps in memory to reuse (`SystemCache`).
    pub cached: u64,
    /// Kernel memory that can be written to the page file (`KernelPaged`).
    pub paged_pool: u64,
    /// Kernel memory that must stay in RAM (`KernelNonpaged`).
    pub nonpaged_pool: u64,
    /// Working set of the Memory Compression process; `None` when it is off or cannot be read
    /// without admin, and then the line is left out.
    pub compressed: Option<u64>,
}

/// Page counts times the page size, saturating instead of overflowing.
pub(crate) fn pages_to_bytes(pages: usize, page_size: usize) -> u64 {
    (pages as u64).saturating_mul(page_size as u64)
}

/// The `PERFORMANCE_INFORMATION` fields this breakdown keeps, as page counts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pages {
    pub commit_total: usize,
    pub commit_limit: usize,
    pub system_cache: usize,
    pub kernel_paged: usize,
    pub kernel_nonpaged: usize,
    pub page_size: usize,
}

impl MemoryBreakdown {
    /// Converts the page counts to bytes; `compressed` is filled in separately.
    pub(crate) fn from_pages(p: Pages) -> MemoryBreakdown {
        let b = |pages| pages_to_bytes(pages, p.page_size);
        MemoryBreakdown {
            committed: b(p.commit_total),
            commit_limit: b(p.commit_limit),
            cached: b(p.system_cache),
            paged_pool: b(p.kernel_paged),
            nonpaged_pool: b(p.kernel_nonpaged),
            compressed: None,
        }
    }
}

/// Working set of the Memory Compression process from `(name, parent pid, working set)` rows.
/// `None` if it is not running (compression off) or its size could not be read (0 without admin).
pub(crate) fn compressed_bytes<'a>(
    procs: impl IntoIterator<Item = (&'a str, Option<u32>, u64)>,
) -> Option<u64> {
    procs
        .into_iter()
        .find(|(name, parent, _)| *name == COMPRESSION_NAME && *parent == Some(SYSTEM_PID))
        .map(|(_, _, bytes)| bytes)
        .filter(|bytes| *bytes > 0)
}

/// "12.3 GB of 40.0 GB", or just the committed size when the limit is unknown.
pub(crate) fn committed_label(committed: u64, limit: u64) -> String {
    if limit == 0 {
        size_plain(committed)
    } else {
        format!("{} of {}", size_plain(committed), size_plain(limit))
    }
}

/// `(value, label, explanation)` for each stat, in display order.
pub(crate) fn stats(b: &MemoryBreakdown) -> Vec<(String, &'static str, &'static str)> {
    let mut v = vec![
        (
            committed_label(b.committed, b.commit_limit),
            "Committed",
            "Memory programs have reserved, of the most RAM plus the page file can back.",
        ),
        (
            size_plain(b.cached),
            "Cached",
            "File data Windows keeps in memory to reuse. It is given up when programs need it.",
        ),
        (
            size_plain(b.paged_pool),
            "Paged pool",
            "Memory used by Windows and drivers that can be moved to the page file.",
        ),
        (
            size_plain(b.nonpaged_pool),
            "Non-paged pool",
            "Memory used by Windows and drivers that must always stay in RAM.",
        ),
    ];
    if let Some(c) = b.compressed {
        v.push((
            size_plain(c),
            "Compressed",
            "Memory held by the Memory Compression process: pages Windows packed to save room.",
        ));
    }
    v
}

/// The breakdown card; `b` is `None` until the first sample or if Windows does not report it.
pub(crate) fn breakdown_card(ui: &mut egui::Ui, width: f32, b: Option<&MemoryBreakdown>) {
    card(ui, width, 0.0, |ui| {
        ui.label(RichText::new("Breakdown").size(TEXT_SECTION).strong());
        let Some(b) = b else {
            ui.label(RichText::new("Collecting...").weak());
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            for (value, label, tip) in stats(b) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(value).strong());
                    ui.label(RichText::new(label).weak().small());
                })
                .response
                .on_hover_text(tip);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_counts_convert_to_bytes() {
        assert_eq!(pages_to_bytes(0, 4096), 0);
        assert_eq!(pages_to_bytes(3, 4096), 12_288);
        assert_eq!(pages_to_bytes(1, 0), 0);
        assert_eq!(pages_to_bytes(usize::MAX, usize::MAX), u64::MAX);
    }

    #[test]
    fn breakdown_from_pages() {
        let b = MemoryBreakdown::from_pages(Pages {
            commit_total: 1_000,
            commit_limit: 4_000,
            system_cache: 250,
            kernel_paged: 100,
            kernel_nonpaged: 50,
            page_size: 4096,
        });
        assert_eq!(b.committed, 4_096_000);
        assert_eq!(b.commit_limit, 16_384_000);
        assert_eq!(b.cached, 1_024_000);
        assert_eq!(b.paged_pool, 409_600);
        assert_eq!(b.nonpaged_pool, 204_800);
        assert_eq!(b.compressed, None);
    }

    #[test]
    fn compressed_only_from_the_system_process() {
        let mc = COMPRESSION_NAME;
        assert_eq!(
            compressed_bytes([("System", None, 1), (mc, Some(4), 500)]),
            Some(500)
        );
        // Same name but not started by System: some other program.
        assert_eq!(compressed_bytes([(mc, Some(1234), 500)]), None);
        assert_eq!(
            compressed_bytes([("memory compression", Some(4), 500)]),
            None
        );
        // Not running (compression off) or unreadable without admin: leave the line out.
        assert_eq!(compressed_bytes([("System", None, 1)]), None);
        assert_eq!(compressed_bytes([(mc, Some(4), 0)]), None);
    }

    #[test]
    fn stats_leave_out_compressed_when_unknown() {
        let mut b = MemoryBreakdown {
            committed: 1,
            commit_limit: 2,
            ..Default::default()
        };
        let labels: Vec<&str> = stats(&b).iter().map(|s| s.1).collect();
        assert_eq!(
            labels,
            ["Committed", "Cached", "Paged pool", "Non-paged pool"]
        );
        b.compressed = Some(5);
        assert_eq!(stats(&b).last().map(|s| s.1), Some("Compressed"));
    }

    #[test]
    fn committed_label_cases() {
        assert!(committed_label(1 << 30, 4 << 30).contains(" of "));
        assert!(!committed_label(1 << 30, 0).contains(" of "));
    }
}
