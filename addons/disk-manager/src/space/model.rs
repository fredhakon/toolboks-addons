//! The Space explorer's view of one scan: each folder's non-empty children sorted by the size
//! that is shown, with the list's filters and "Hide smaller than" applied, plus each entry's
//! file type, age and top folder. Built once per scan and filter set, on a worker thread.
use crate::category::{self, Category};
use crate::scanner::Node;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// What hides files from the map. Hidden bytes are drawn as one quiet strip, so totals add up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Filters {
    /// The list's "Over ..." size filter: smaller files are hidden.
    pub min_bytes: u64,
    /// The list's "Over ... old" filter: newer files (and files without a date) are hidden.
    pub older_days: u64,
    /// "Hide smaller than" on the explorer itself.
    pub hide_below: u64,
}
impl Filters {
    fn shows(&self, n: &Node, now: u64) -> bool {
        n.bytes >= self.min_bytes
            && n.bytes >= self.hide_below
            && self.older_days.checked_mul(86400).is_some_and(|age| {
                age == 0 || (n.modified > 0 && now.saturating_sub(n.modified) >= age)
            })
    }
}

/// Folder names whose otherwise unknown files are counted as games.
fn games_folder(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "games" | "steamapps" | "steamlibrary" | "epic games" | "gog games" | "xboxgames"
    )
}
fn system_folder(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "windows" | "system32" | "syswow64" | "winsxs"
    )
}

/// No top folder: the scan root itself.
pub const NO_TOP: u32 = u32::MAX;

static SERIAL: AtomicU64 = AtomicU64::new(1);

pub struct Model {
    /// Unique per build, so a layout cache never mixes two models.
    pub serial: u64,
    pub generation: u64,
    pub filters: Filters,
    pub nodes: Arc<Vec<Node>>,
    /// Bytes drawn for each entry: a file's size when it is shown, a folder's shown total.
    pub shown: Vec<u64>,
    /// Files drawn under each entry (1 for a shown file).
    pub shown_files: Vec<u64>,
    start: Vec<u32>,
    kids: Vec<u32>,
    /// A file's type; a folder's largest type by bytes.
    kind: Vec<u8>,
    /// Rank (largest first) of the top folder each entry is in, or [`NO_TOP`].
    pub top: Vec<u32>,
    /// The top folders' node ids, by rank.
    pub top_ids: Vec<u32>,
    /// Files of 0 bytes in the scan: never drawn.
    pub empty_files: u64,
    /// Files of 0 bytes under each folder, so they are not counted as hidden.
    empties: Vec<u32>,
    pub now: u64,
}

impl Model {
    /// Builds the model. `nodes` must list parents before their children with `id == index`,
    /// which the scanner and the delete patch guarantee; anything else is skipped, not trusted.
    pub fn build(nodes: Arc<Vec<Node>>, generation: u64, filters: Filters, now: u64) -> Self {
        let n = nodes.len();
        let mut shown = vec![0u64; n];
        let mut shown_files = vec![0u64; n];
        let mut kind = vec![Category::Other as u8; n];
        let mut games = vec![false; n];
        let mut empty_files = 0;
        let mut empties = vec![0u32; n];
        let parent_of = |node: &Node| node.parent.filter(|&p| p < node.id && node.id < n);
        for node in nodes.iter() {
            if node.id >= n {
                continue;
            }
            let in_games = parent_of(node).is_some_and(|p| games[p]);
            if node.dir {
                games[node.id] = in_games || games_folder(&node.name);
            } else {
                let mut c = category::file(&node.name);
                if c == Category::Other && in_games {
                    c = Category::Games;
                }
                kind[node.id] = c as u8;
                if node.bytes == 0 {
                    empty_files += 1;
                    empties[node.id] = 1;
                } else if filters.shows(node, now) {
                    shown[node.id] = node.bytes;
                    shown_files[node.id] = 1;
                }
            }
        }
        // Folder totals and each folder's largest type, children first.
        let n_nodes = n;
        let mut slot = vec![u32::MAX; n];
        let mut hist: Vec<[u64; 10]> = Vec::new();
        for node in nodes.iter().filter(|n| n.dir && n.id < n_nodes) {
            slot[node.id] = hist.len() as u32;
            hist.push([0; 10]);
        }
        for id in (0..n).rev() {
            let node = &nodes[id];
            if node.id != id {
                continue;
            }
            if node.dir {
                let h = hist[slot[id] as usize];
                kind[id] = if system_folder(&node.name) {
                    Category::System as u8
                } else if games[id] && h[Category::Other as usize] >= h.iter().sum::<u64>() / 2 {
                    Category::Games as u8
                } else {
                    let (i, b) = h.iter().enumerate().max_by_key(|(_, b)| **b).unwrap();
                    if *b == 0 {
                        Category::Other as u8
                    } else {
                        i as u8
                    }
                };
            }
            let Some(p) = parent_of(node) else { continue };
            shown[p] = shown[p].saturating_add(shown[id]);
            shown_files[p] += shown_files[id];
            empties[p] = empties[p].saturating_add(empties[id]);
            let ps = slot[p];
            if ps == u32::MAX {
                continue;
            }
            if node.dir {
                let h = hist[slot[id] as usize];
                for (to, from) in hist[ps as usize].iter_mut().zip(h) {
                    *to = to.saturating_add(from);
                }
            } else {
                let w = &mut hist[ps as usize][kind[id] as usize];
                *w = w.saturating_add(shown[id]);
            }
        }
        // Children lists: only entries that are drawn, largest first.
        let mut start = vec![0u32; n + 1];
        for node in nodes.iter() {
            if let Some(p) = parent_of(node).filter(|_| shown[node.id] > 0) {
                start[p + 1] += 1;
            }
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut kids = vec![0u32; start[n] as usize];
        for node in nodes.iter() {
            if let Some(p) = parent_of(node).filter(|_| shown[node.id] > 0) {
                kids[fill[p] as usize] = node.id as u32;
                fill[p] += 1;
            }
        }
        for p in 0..n {
            let (a, b) = (start[p] as usize, start[p + 1] as usize);
            if b - a > 1 {
                kids[a..b]
                    .sort_by(|x, y| shown[*y as usize].cmp(&shown[*x as usize]).then(x.cmp(y)));
            }
        }
        // Top folders, ranked by their full size, and each entry's top folder.
        let mut top_ids: Vec<u32> = nodes
            .iter()
            .filter(|node| node.parent == Some(0) && node.id > 0 && node.id < n)
            .map(|node| node.id as u32)
            .collect();
        top_ids.sort_by(|a, b| {
            nodes[*b as usize]
                .bytes
                .cmp(&nodes[*a as usize].bytes)
                .then(a.cmp(b))
        });
        let mut top = vec![NO_TOP; n];
        for (rank, id) in top_ids.iter().enumerate() {
            top[*id as usize] = rank as u32;
        }
        for node in nodes.iter() {
            if let Some(p) = parent_of(node).filter(|&p| p != 0) {
                top[node.id] = top[p];
            }
        }
        Self {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            generation,
            filters,
            nodes,
            shown,
            shown_files,
            start,
            kids,
            kind,
            top,
            top_ids,
            empty_files,
            empties,
            now,
        }
    }
    /// The drawn children of `id`, largest first.
    pub fn children(&self, id: usize) -> &[u32] {
        match (self.start.get(id), self.start.get(id + 1)) {
            (Some(&a), Some(&b)) => &self.kids[a as usize..b as usize],
            _ => &[],
        }
    }
    pub fn category(&self, id: usize) -> Category {
        Category::ALL
            .into_iter()
            .find(|c| *c as u8 == self.kind.get(id).copied().unwrap_or(8))
            .unwrap_or(Category::Other)
    }
    pub fn node(&self, id: usize) -> Option<&Node> {
        self.nodes.get(id)
    }
    /// Bytes under `id` that the filters hide.
    pub fn hidden(&self, id: usize) -> (u64, u64) {
        let Some(n) = self.nodes.get(id) else {
            return (0, 0);
        };
        let files = if n.dir { n.files } else { 1 };
        (
            n.bytes.saturating_sub(self.shown[id]),
            files
                .saturating_sub(self.shown_files[id])
                .saturating_sub(self.empties[id] as u64),
        )
    }
    /// The folders from the root down to `id`.
    pub fn path_to(&self, id: usize) -> Vec<usize> {
        let mut out = vec![];
        let mut next = Some(id).filter(|&i| i < self.nodes.len());
        while let Some(i) = next {
            out.push(i);
            next = self.nodes[i].parent.filter(|&p| p < i);
        }
        out.reverse();
        out
    }
    pub fn age_bucket(&self, id: usize) -> u8 {
        age_bucket(self.nodes.get(id).map_or(0, |n| n.modified), self.now)
    }
}

/// New to old: last 30 days, 1 to 6 months, 6 to 12 months, 1 to 3 years, older, no date.
pub fn age_bucket(modified: u64, now: u64) -> u8 {
    if modified == 0 {
        return 5;
    }
    let days = now.saturating_sub(modified) / 86400;
    match days {
        0..30 => 0,
        30..182 => 1,
        182..365 => 2,
        365..1095 => 3,
        _ => 4,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn node(
        id: usize,
        parent: Option<usize>,
        name: &str,
        dir: bool,
        bytes: u64,
    ) -> Node {
        Node {
            id,
            parent,
            name: name.into(),
            path: format!(r"D:\{name}"),
            dir,
            bytes,
            files: u64::from(!dir),
            modified: 0,
            dir_stamp: 0,
        }
    }
    /// Sets folder sizes and file counts from their files, like the scanner does.
    pub(crate) fn totals(mut nodes: Vec<Node>) -> Vec<Node> {
        for id in (1..nodes.len()).rev() {
            let (bytes, files) = (nodes[id].bytes, nodes[id].files);
            if let Some(p) = nodes[id].parent {
                nodes[p].bytes += bytes;
                nodes[p].files += files;
            }
        }
        nodes
    }
    pub(crate) fn sample() -> Arc<Vec<Node>> {
        Arc::new(totals(vec![
            node(0, None, r"\\?\D:\", true, 0),
            node(1, Some(0), "SteamLibrary", true, 0),
            node(2, Some(1), "game.pak", false, 600),
            node(3, Some(1), "movie.mkv", false, 200),
            node(4, Some(0), "Docs", true, 0),
            node(5, Some(4), "a.pdf", false, 100),
            node(6, Some(4), "empty.txt", false, 0),
            node(7, Some(0), "loose.zip", false, 50),
        ]))
    }
    #[test]
    fn children_are_sorted_and_empty_entries_are_left_out() {
        let m = Model::build(sample(), 1, Filters::default(), 0);
        assert_eq!(m.children(0), [1, 4, 7]);
        assert_eq!(m.children(1), [2, 3]);
        assert_eq!(m.children(4), [5]);
        assert_eq!(m.shown[0], 950);
        assert_eq!(m.empty_files, 1);
        assert_eq!(
            m.category(2),
            Category::Games,
            "unknown files under Steam are games"
        );
        assert_eq!(m.category(3), Category::Video);
        assert_eq!(m.category(1), Category::Games);
        assert_eq!(m.category(4), Category::Documents);
        assert_eq!(m.top_ids, [1, 4, 7]);
        assert_eq!((m.top[2], m.top[5], m.top[7], m.top[0]), (0, 1, 2, NO_TOP));
        assert_eq!(m.path_to(5), [0, 4, 5]);
    }
    #[test]
    fn filters_hide_files_and_hidden_bytes_add_up() {
        let filters = Filters {
            hide_below: 150,
            ..Default::default()
        };
        let m = Model::build(sample(), 1, filters, 0);
        assert_eq!(m.children(0), [1]);
        assert_eq!(m.shown[0], 800);
        assert_eq!(m.hidden(0), (150, 2));
        assert_eq!(m.shown[0] + m.hidden(0).0, m.nodes[0].bytes);
        let old = Filters {
            older_days: 1,
            ..Default::default()
        };
        let m = Model::build(sample(), 1, old, 10 * 86400);
        assert_eq!(m.shown[0], 0, "files without a date are not old");
    }
    #[test]
    fn ages_go_from_new_to_old() {
        let day = 86400;
        let now = 4000 * day;
        let b: Vec<u8> = [now, now - 40 * day, now - 200 * day, now - 400 * day, 1, 0]
            .iter()
            .map(|&m| age_bucket(m, now))
            .collect();
        assert_eq!(b, [0, 1, 2, 3, 4, 5]);
    }
}
