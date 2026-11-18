use super::*;
#[cfg(test)]
use egui::Vec2;
use toolboks_addon_sdk::TutorialStep as S;
pub(super) fn steps() -> Vec<S> {
    [
        ("0", "disk-drives", "Choose a drive", "The drive cards list detected volumes with free and total capacity. Choosing one fills the scan path; it does not start a scan. A folder opens the folder picker."),
        ("0", "disk-path", "Choose the scan location", "Scan a whole drive or enter a specific folder path. Narrower scans finish sooner and focus on the area you want to understand. The path is remembered locally."),
        ("0", "disk-browse", "Browse to a folder", "Browse to folder opens the Windows folder picker. Select a folder to fill the path, or cancel to leave it unchanged. The walkthrough does not open the picker."),
        ("0", "disk-analyze", "Scan, rescan and stop", "Scan reads file and folder metadata in the background. Remember my last scan keeps completed results on this PC. Rescan (quick) checks folder times and can miss in-place size edits; Full rescan refreshes every size. Stop scan keeps partial results and never deletes your files. This tutorial starts no scan."),
        ("0/options", "disk-options", "Scan options and exclusions", "Enter folder names separated by semicolons to skip them. Matching ignores case and uses exact names. Exclusions affect scan coverage and can reduce totals. Leave this empty for the broadest scan; the tour changes no settings."),
        ("0", "disk-stats", "Understand the scan totals", "This bar shows discovered file bytes, file and folder counts, and elapsed time. A cancelled scan contains partial totals. Sizes are logical lengths and can differ from allocated disk space. If no scan is ready, this tour uses clearly labeled sample data."),
        ("0", "disk-workspace-title", "Explore the five views", "Use the Disk Manager pages in the Toolboks sidebar or the Pages menu when the sidebar is collapsed. Next will visit each view for you."),
        ("0", "disk-map", "Space map: find the largest folders", "Rectangles represent immediate children of the current folder. Larger entries have more area; tiny entries are enlarged so they remain clickable. Tiles stretch to fill gaps. Hover for full names and exact sizes, scroll sideways for more, and click a folder to drill down."),
        ("0", "disk-filters", "Search, size, age and sorting", "Search narrows scanned paths; size and age filters reduce the result list. Sort by size, name, file count or modification time. These filters affect the list, not what was scanned or the map's underlying totals."),
        ("0", "disk-breadcrumbs", "Find your way back", "The path above the table shows how you reached this folder. Select any earlier folder to return to it."),
        ("0", "disk-list", "Folder list and file actions", "The list reports entry sizes, share of the entire scan, and file counts. Select an entry to inspect it; double-click a folder or use Open folder to explore it. Right-click an entry to copy its path or reveal it in Explorer. Pagination limits how many entries are drawn at once."),
        ("1", "disk-list", "Largest files · across the scan", "This view ranks individual files across all scanned folders. It is not limited to the folder you last explored. Use the filters and sorting to find large or old files, then inspect their paths before making any decisions."),
        ("1", "disk-filters", "Largest files · narrow the ranking", "Combine text, minimum size and age filters to find files that deserve attention. These are filters over the saved scan snapshot; they do not rescan or modify the files."),
        ("2", "disk-types", "File types · what fills the space", "Extension totals show which kinds of files dominate the scan. Bars show each type's share of all discovered file bytes. An extension describes a name, not a guarantee about a file's contents or whether it is safe to remove."),
        ("3", "disk-details", "Scan details · coverage and errors", "This page reports skipped entries and the first recorded access or read errors. Links and junctions are skipped, hard links count per entry, and sparse or cloud files can differ from allocated space. Files may change during scanning."),
        ("3", "disk-export", "Export your findings", "Export report offers CSV or JSON for every discovered entry, including those hidden by list filters. Choose a full destination filename; existing files are not overwritten. Export runs in the background only when you explicitly save a report."),
        ("4", "optimize-drive", "Optimization · choose a drive", "Select a local fixed drive independently of the folder scan path. The drive information shows its filesystem, free space and detected media type. Refresh reloads metadata and available Windows history; it does not optimize a drive."),
        ("4", "optimize-fragmentation", "Fragmentation · measure first", "Analyze fragmentation requests a numeric Windows report. Total, file and free-space fragmentation are separate measurements, shown with their analysis time. An unavailable value is not zero. These figures are not an SSD health or wear score."),
        ("4", "optimize-history", "Last defrag and last TRIM", "Windows success records distinguish defragmentation from TRIM. Dates are local times for this drive letter. No available record means the retained log contains no matching record; it does not establish that the drive was never optimized."),
        ("4", "optimize-actions", "HDD defrag or SSD TRIM", "Hard drives offer Defrag. Detected SSDs offer TRIM for unused blocks. If media type is unavailable, Windows chooses the appropriate optimization. Administrator access and the add-on's optimization permission are required. A separate administrator window asks you to confirm or cancel; the tutorial never performs an operation."),
        ("4", "optimize-progress", "Windows activity and results", "Drive maintenance runs in the background. The report shows Windows output and any reported phase percentage, which is not an overall completion estimate. Windows operations can continue if Toolboks closes. After completion, the app refreshes history and attempts a fresh analysis."),
    ].into_iter().map(|(p,t,h,d)|S::new(p,t,h,d)).collect()
}
pub(super) fn sample() -> (Progress, Arc<Vec<Node>>) {
    let mib = 1024 * 1024;
    let nodes = vec![
        Node {
            id: 0,
            parent: None,
            name: "Sample scan".into(),
            path: "Sample scan".into(),
            dir: true,
            bytes: 950 * mib,
            files: 3,
            modified: 0,
            dir_stamp: 0,
        },
        Node {
            id: 1,
            parent: Some(0),
            name: "Projects".into(),
            path: "Sample scan/Projects".into(),
            dir: true,
            bytes: 600 * mib,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        },
        Node {
            id: 2,
            parent: Some(0),
            name: "Videos".into(),
            path: "Sample scan/Videos".into(),
            dir: true,
            bytes: 300 * mib,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        },
        Node {
            id: 3,
            parent: Some(0),
            name: "archive.zip".into(),
            path: "Sample scan/archive.zip".into(),
            dir: false,
            bytes: 50 * mib,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        },
        Node {
            id: 4,
            parent: Some(1),
            name: "assets.bin".into(),
            path: "Sample scan/Projects/assets.bin".into(),
            dir: false,
            bytes: 600 * mib,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        },
        Node {
            id: 5,
            parent: Some(2),
            name: "recording.mp4".into(),
            path: "Sample scan/Videos/recording.mp4".into(),
            dir: false,
            bytes: 300 * mib,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        },
    ];
    (
        Progress {
            generation: u64::MAX,
            root: "Sample scan".into(),
            files: 3,
            folders: 3,
            bytes: 950 * mib,
            elapsed: 1.2,
            extensions: vec![
                scanner::Extension {
                    name: ".bin".into(),
                    bytes: 600 * mib,
                    files: 1,
                },
                scanner::Extension {
                    name: ".mp4".into(),
                    bytes: 300 * mib,
                    files: 1,
                },
                scanner::Extension {
                    name: ".zip".into(),
                    bytes: 50 * mib,
                    files: 1,
                },
            ],
            ..Default::default()
        },
        Arc::new(nodes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_rect(output: &egui::FullOutput, label: &str) -> egui::Rect {
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.visual_bounding_rect())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing text: {label}"))
    }

    #[test]
    fn table_selects_for_inspection_then_opens_on_double_click() {
        let ctx = egui::Context::default();
        toolboks_shared::ui::apply_style(&ctx);
        let mut addon = DiskManager::default();
        let (progress, nodes) = sample();
        let mut time = 0.0;
        let mut frame = |addon: &mut DiskManager, events| {
            time += 0.02;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1280.0, 900.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| addon.overview(ui, &nodes, &progress),
            );
            output.textures_delta.clear();
            output
        };
        frame(&mut addon, vec![]);
        let output = frame(&mut addon, vec![]);
        let position = text_rect(&output, "Projects (folder)").center();
        for click in 0..2 {
            for pressed in [true, false] {
                frame(
                    &mut addon,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            if click == 0 {
                assert_eq!(
                    addon.parent, 0,
                    "A single click must keep the current folder open"
                );
                assert_eq!(addon.map_state.selected, Some(1));
                let output = frame(&mut addon, vec![]);
                assert!(text_rect(&output, "Folder · Sample scan/Projects").is_positive());
            }
        }
        assert_eq!(addon.parent, 1);
        assert_eq!(addon.map_state.selected, None);
    }

    #[test]
    fn drive_cards_share_one_row_and_reflow_without_overflow() {
        for width in [1280.0, 820.0, 620.0] {
            let ctx = egui::Context::default();
            toolboks_shared::ui::apply_style(&ctx);
            let mut addon = DiskManager::default();
            addon.drives = ['C', 'D', 'E']
                .into_iter()
                .map(|letter| Drive {
                    path: format!("{letter}:\\"),
                    total: 1024,
                    free: 512,
                    description: "SSD · NTFS".into(),
                })
                .collect();
            let mut output = egui::FullOutput::default();
            let mut bounds = egui::Rect::NOTHING;
            for _ in 0..3 {
                output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(width, 900.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        addon.drive_cards(ui, &Progress::default());
                        bounds = ui.min_rect();
                    },
                );
                output.textures_delta.clear();
            }
            assert!(
                bounds.right() <= width + 1.0,
                "Cards exceed the viewport: {bounds:?}"
            );
            let first = text_rect(&output, "C:");
            let folder = text_rect(&output, "A folder");
            if width == 1280.0 {
                assert!(
                    (first.top() - folder.top()).abs() < 4.0,
                    "Drive {first:?}, folder {folder:?}, bounds {bounds:?}"
                );
            } else {
                assert!(folder.top() > first.bottom());
            }
        }
    }

    #[test]
    fn overview_keeps_table_first_and_reflows_the_inspector() {
        for width in [1280.0, 820.0, 620.0] {
            let ctx = egui::Context::default();
            let mut addon = DiskManager::default();
            let (progress, nodes) = sample();
            let mut anchors = Vec::new();
            for target in ["disk-map", "disk-list"] {
                for _ in 0..3 {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                Vec2::new(width, 850.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            guide::prepare(ui.ctx(), Some(target), false);
                            addon.overview(ui, &nodes, &progress);
                        },
                    );
                    output.textures_delta.clear();
                }
                anchors.push(guide::anchor(&ctx).unwrap());
            }
            let [map, list] = anchors.as_slice() else {
                unreachable!()
            };
            if width == 1280.0 {
                assert!(list.right() <= map.left(), "{list:?} overlaps {map:?}");
            } else {
                assert!(list.bottom() <= map.top(), "{list:?} overlaps {map:?}");
            }
            assert!(list.right() <= width + 1.0, "{list:?} exceeds {width}");
            assert!(map.right() <= width + 1.0, "{map:?} exceeds {width}");
        }
    }
    #[test]
    fn every_tour_step_resolves_without_a_scan_and_restores_state() {
        for width in [1280.0, 820.0, 620.0] {
            let ctx = egui::Context::default();
            let mut addon = DiskManager::default();
            addon.page = 2;
            addon.parent = 4;
            addon.offset = 100;
            addon.options = true;
            let mut prefs = BTreeMap::new();
            addon.tutorial_start();
            for step in steps() {
                addon.tutorial_navigate(&step.page);
                for _ in 0..20 {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                Vec2::new(width, 720.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            guide::prepare(ui.ctx(), Some(&step.target), true);
                            ui.disable();
                            addon.ui(ui, &mut prefs);
                        },
                    );
                    output.textures_delta.clear();
                }
                assert!(
                    guide::anchor(&ctx).is_some(),
                    "Missing anchor at width {width} for {} ({})",
                    step.title,
                    step.target
                );
                assert_eq!(addon.shared.lock().unwrap().progress.generation, 0);
                assert!(!addon.shared.lock().unwrap().progress.running);
                assert!(addon.export_job.is_none() && addon.picker.is_none());
            }
            addon.tutorial_finish();
            assert_eq!(
                (addon.page, addon.options, addon.parent, addon.offset),
                (2, true, 4, 100)
            );
            assert!(addon.tutorial_snapshot.is_none());
        }
    }
}
