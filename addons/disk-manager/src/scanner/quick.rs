use super::*;
use std::collections::VecDeque;

/// Check every saved directory's own time, including descendants of unchanged directories.
/// Unchanged directories reuse their direct children without enumerating them or stat-ing files.
pub fn rescan(
    old: Arc<Vec<Node>>,
    previous: Progress,
    shared: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
) {
    let started = Instant::now();
    let mut p = shared.lock().unwrap().progress.clone();
    p.files = 0;
    p.bytes = 0;
    p.folders = 0;
    p.skipped = 0;
    p.errors.clear();
    p.extensions.clear();
    p.cancelled = false;
    p.scanned_at = now();
    p.full_scanned_at = previous.full_scanned_at;
    p.volume = previous.volume;
    p.exclusions = previous.exclusions;
    let mut changes = Changes::default();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for n in old.iter().skip(1) {
        if let Some(parent) = n.parent {
            children.entry(parent).or_default().push(n.id);
        }
    }
    // Directory metadata is independent: bounded parallel probes avoid serial disk latency.
    let folders: Vec<_> = old.iter().filter(|n| n.dir).collect();
    let mut probes = vec![(0, false); old.len()];
    let workers = std::thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(1, 8);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = folders
            .chunks(folders.len().div_ceil(workers).max(1))
            .map(|chunk| {
                let cancel = &cancel;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .take_while(|_| !cancel.load(Ordering::Relaxed))
                        .map(|n| (n.id, probe(Path::new(&n.path))))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for job in jobs {
            for (id, value) in job.join().unwrap() {
                probes[id] = value;
            }
        }
    });
    let mut root = old[0].clone();
    root.bytes = 0;
    root.files = 0;
    let mut nodes = vec![root];
    let mut queue = VecDeque::from([(0, Some(0))]);
    let mut last_update = Instant::now();
    while let Some((id, old_id)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            p.cancelled = true;
            break;
        }
        let path = PathBuf::from(&nodes[id].path);
        p.current = nodes[id].path.clone();
        p.folders += 1;
        let (current_stamp, blocked) = old_id.map_or_else(|| probe(&path), |i| probes[i]);
        // Never follow a directory that has been replaced with a link or a file.
        if blocked {
            nodes[id].dir_stamp = 0;
            p.skipped += 1;
            continue;
        }
        let unchanged =
            old_id.is_some_and(|i| old[i].dir_stamp != 0 && old[i].dir_stamp == current_stamp);
        let old_children = old_id
            .and_then(|i| children.get(&i))
            .map_or(&[][..], Vec::as_slice);
        if unchanged {
            changes.folders_reused += 1;
            for &child in old_children {
                append(
                    old[child].clone(),
                    Some(child),
                    id,
                    &mut nodes,
                    &mut queue,
                    &mut p,
                );
            }
        } else {
            changes.folders_read += 1;
            let batch = read_dir_job(Job { path, parent: id }, &p.exclusions, &cancel);
            nodes[id].dir_stamp = batch.stamp;
            p.skipped += batch.skipped + batch.error_count;
            for e in batch.errors {
                if p.errors.len() < MAX_ERRORS {
                    p.errors.push(e);
                }
            }
            let mut remaining: HashMap<&str, usize> = old_children
                .iter()
                .map(|&i| (old[i].name.as_str(), i))
                .collect();
            for entry in batch.entries {
                let dir = entry.dir_path.is_some();
                let matched = remaining.remove(entry.name.as_str());
                let same = matched.filter(|&i| old[i].dir == dir);
                if let Some(i) = matched.filter(|_| same.is_none()) {
                    changes.removed += subtree_entries(i, &children);
                }
                match same {
                    None => changes.new += 1,
                    Some(i)
                        if !dir
                            && (entry.bytes != old[i].bytes
                                || entry.modified != old[i].modified) =>
                    {
                        changes.changed += 1
                    }
                    _ => {}
                }
                let node = Node {
                    id: 0,
                    parent: None,
                    name: entry.name,
                    path: entry.path,
                    dir,
                    bytes: entry.bytes,
                    files: if dir { 0 } else { 1 },
                    modified: entry.modified,
                    dir_stamp: entry.dir_stamp,
                };
                append(node, same, id, &mut nodes, &mut queue, &mut p);
            }
            for (_, i) in remaining {
                if batch.error_count > 0 {
                    // An unreadable entry is unknown, not evidence of a deletion.
                    append(old[i].clone(), Some(i), id, &mut nodes, &mut queue, &mut p);
                } else {
                    changes.removed += subtree_entries(i, &children);
                }
            }
        }
        if last_update.elapsed() >= PROGRESS_EVERY {
            p.elapsed = started.elapsed().as_secs_f64();
            p.changes = Some(changes.clone());
            shared.lock().unwrap().progress = p.clone();
            last_update = Instant::now();
        }
    }
    for id in (1..nodes.len()).rev() {
        let (parents, children) = nodes.split_at_mut(id);
        let child = &children[0];
        if let Some(parent) = child.parent.and_then(|i| parents.get_mut(i)) {
            parent.bytes = parent.bytes.saturating_add(child.bytes);
            parent.files = parent.files.saturating_add(child.files);
        }
    }
    let mut ext: HashMap<String, (u64, u64)> = HashMap::new();
    for n in nodes.iter().filter(|n| !n.dir) {
        let name = Path::new(&n.name)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or("No extension".into());
        let e = ext.entry(name).or_default();
        e.0 = e.0.saturating_add(n.bytes);
        e.1 += 1;
    }
    p.extensions = ext
        .into_iter()
        .map(|(name, (bytes, files))| Extension { name, bytes, files })
        .collect();
    p.extensions.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    p.running = false;
    p.elapsed = started.elapsed().as_secs_f64();
    p.changes = Some(changes);
    let mut state = shared.lock().unwrap();
    state.nodes = Arc::new(nodes);
    state.progress = p;
}
fn probe(path: &Path) -> (u64, bool) {
    match fs::symlink_metadata(path) {
        Ok(m) => (stamp(&m), linked(&m) || !m.is_dir()),
        Err(_) => (0, false),
    }
}
fn append(
    mut node: Node,
    old: Option<usize>,
    parent: usize,
    nodes: &mut Vec<Node>,
    queue: &mut VecDeque<(usize, Option<usize>)>,
    p: &mut Progress,
) {
    node.id = nodes.len();
    node.parent = Some(parent);
    if node.dir {
        node.bytes = 0;
        node.files = 0;
        queue.push_back((node.id, old));
    } else {
        p.files += 1;
        p.bytes = p.bytes.saturating_add(node.bytes);
    }
    nodes.push(node);
}
fn subtree_entries(id: usize, children: &HashMap<usize, Vec<usize>>) -> u64 {
    let mut count = 0;
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        count += 1;
        if let Some(c) = children.get(&id) {
            stack.extend(c);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    fn full(root: &Path) -> (Progress, Arc<Vec<Node>>) {
        let shared = Arc::new(Mutex::new(State::default()));
        scan(
            root.into(),
            vec![],
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let s = shared.lock().unwrap();
        (s.progress.clone(), s.nodes.clone())
    }
    fn quick(p: Progress, nodes: Arc<Vec<Node>>) -> (Progress, Arc<Vec<Node>>) {
        let shared = Arc::new(Mutex::new(State {
            progress: Progress {
                generation: p.generation + 1,
                running: true,
                root: p.root.clone(),
                ..Default::default()
            },
            nodes: Default::default(),
        }));
        rescan(nodes, p, shared.clone(), Arc::new(AtomicBool::new(false)));
        let s = shared.lock().unwrap();
        (s.progress.clone(), s.nodes.clone())
    }
    #[test]
    fn unchanged_parent_still_checks_descendants_and_rereads_only_changed_folders() {
        let root = super::super::tests::temp("quick");
        fs::create_dir_all(root.join("unchanged/deep")).unwrap();
        fs::create_dir_all(root.join("changed/removed/deep")).unwrap();
        fs::write(root.join("unchanged/deep/in-place"), b"old").unwrap();
        for name in ["delete", "rename", "resize"] {
            fs::write(root.join("changed").join(name), b"old").unwrap();
        }
        fs::write(root.join("changed/removed/deep/gone"), b"gone").unwrap();
        let (p, nodes) = full(&root);
        let (same, same_nodes) = quick(p.clone(), nodes.clone());
        let c = same.changes.unwrap();
        assert_eq!((c.new, c.removed, c.changed, c.folders_read), (0, 0, 0, 0));
        assert_eq!(c.folders_reused, p.folders);
        assert_eq!((same.bytes, same.files), (p.bytes, p.files));
        assert_eq!(same_nodes[0].bytes, nodes[0].bytes);
        fs::write(
            root.join("unchanged/deep/in-place"),
            b"grown in place; parent time stays unchanged",
        )
        .unwrap();
        let (limited, _) = quick(p.clone(), nodes.clone());
        assert_eq!(limited.bytes, p.bytes, "honest in-place size limit");
        // Changes in a descendant must be found even when its ancestor's own time is unchanged.
        fs::write(root.join("changed/new"), b"new").unwrap();
        fs::remove_file(root.join("changed/delete")).unwrap();
        fs::rename(root.join("changed/rename"), root.join("changed/renamed")).unwrap();
        fs::write(
            root.join("changed/resize"),
            b"size is refreshed when this folder changes",
        )
        .unwrap();
        fs::remove_dir_all(root.join("changed/removed")).unwrap();
        fs::create_dir_all(root.join("changed/added/nested")).unwrap();
        fs::write(root.join("changed/added/nested/file"), b"added").unwrap();
        let (updated, updated_nodes) = quick(p.clone(), nodes);
        let c = updated.changes.as_ref().unwrap();
        assert_eq!((c.new, c.removed, c.changed), (5, 5, 1));
        assert!(c.folders_read > 0 && c.folders_reused > 0);
        let has = |suffix: &str| {
            updated_nodes
                .iter()
                .any(|n| Path::new(&n.path) == root.join(suffix))
        };
        assert!(has("changed/new") && has("changed/renamed") && has("changed/added/nested/file"));
        assert!(
            !has("changed/delete") && !has("changed/rename") && !has("changed/removed/deep/gone")
        );
        let (fresh, fresh_nodes) = full(&root);
        assert!(
            fresh.bytes > updated.bytes,
            "full scan refreshes the intentionally unobserved in-place edit"
        );
        // Refresh all sizes, then quick and full have identical trees and totals.
        let (quick_full, qnodes) = quick(fresh.clone(), fresh_nodes.clone());
        assert_eq!(
            (quick_full.bytes, quick_full.files),
            (fresh.bytes, fresh.files)
        );
        let tree = |nodes: &[Node]| {
            let mut v: Vec<_> = nodes
                .iter()
                .map(|n| (n.path.clone(), n.bytes, n.files))
                .collect();
            v.sort();
            v
        };
        assert_eq!(tree(&qnodes), tree(&fresh_nodes));
    }
    #[test]
    fn cancelled_quick_scan_is_partial_and_never_marked_finished() {
        let root = super::super::tests::temp("quick-cancel");
        fs::create_dir_all(&root).unwrap();
        let (p, nodes) = full(&root);
        let shared = Arc::new(Mutex::new(State::default()));
        rescan(nodes, p, shared.clone(), Arc::new(AtomicBool::new(true)));
        assert!(shared.lock().unwrap().progress.cancelled);
    }
    #[test]
    #[ignore = "read-only full-drive benchmark; mutates only the explicitly named fixture folder"]
    fn bench_full_and_quick_real_drive() {
        let root = PathBuf::from(std::env::var("TOOLBOKS_SCAN_BENCH").expect("scan root"));
        let fixture = PathBuf::from("D:/CodexBuilds/disk-manager-issues-22-24/bench-mutations");
        fs::create_dir_all(&fixture).unwrap();
        for name in ["delete", "rename", "resize"] {
            fs::write(fixture.join(name), b"before").unwrap();
        }
        let (p, nodes) = full(&root);
        fs::remove_file(fixture.join("delete")).unwrap();
        fs::rename(fixture.join("rename"), fixture.join("renamed")).unwrap();
        fs::write(fixture.join("new"), b"new").unwrap();
        fs::write(fixture.join("resize"), b"grown inside a changed folder").unwrap();
        let (q, qnodes) = quick(p.clone(), nodes);
        for name in ["new", "renamed"] {
            assert!(qnodes
                .iter()
                .any(|n| Path::new(&n.path) == fixture.join(name)));
        }
        for name in ["delete", "rename"] {
            assert!(!qnodes
                .iter()
                .any(|n| Path::new(&n.path) == fixture.join(name)));
        }
        let (f, _) = full(&root);
        println!("REAL root={} full={:.6}s quick={:.6}s verify_full={:.6}s files={} folders={} bytes={} changes={:?} verify_files={} verify_bytes={}",root.display(),p.elapsed,q.elapsed,f.elapsed,q.files,q.folders,q.bytes,q.changes,f.files,f.bytes);
        if let Ok(dir) = std::env::var("TOOLBOKS_BENCH_SAVE_DIR") {
            crate::saved_scan::save(Path::new(&dir), &q, &qnodes).unwrap();
        }
        for name in ["renamed", "new", "resize"] {
            fs::remove_file(fixture.join(name)).unwrap();
        }
        fs::remove_dir(fixture).unwrap();
    }
}
