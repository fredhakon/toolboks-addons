//! Labelled file groups; folder colours follow their dominant bytes. Every group has its own
//! clear hue (the owner's wish overrides the grey category colours in DESIGN.md): only an entry
//! that holds 0 bytes is drawn grey, with [`EMPTY`].
use crate::scanner::Node;
use egui::Color32;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Category {
    Video,
    Images,
    Archives,
    Apps,
    Documents,
    Audio,
    Code,
    System,
    Other,
    Games,
}
impl Category {
    pub const ALL: [Self; 10] = [
        Self::Video,
        Self::Images,
        Self::Archives,
        Self::Apps,
        Self::Documents,
        Self::Audio,
        Self::Code,
        Self::System,
        Self::Other,
        Self::Games,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "Videos",
            Self::Images => "Photos and images",
            Self::Archives => "Archives",
            Self::Apps => "Apps and executables",
            Self::Documents => "Documents",
            Self::Audio => "Audio",
            Self::Code => "Code",
            Self::System => "System / Windows",
            Self::Other => "Other / mixed",
            Self::Games => "Games",
        }
    }
    pub fn color(self, variant: usize) -> Color32 {
        use crate::theme::DATA;
        let base = match self {
            Self::Video => DATA[3],
            Self::Images => DATA[4],
            Self::System => DATA[2],
            Self::Code => DATA[1],
            Self::Games => Color32::from_rgb(105, 70, 190),
            Self::Archives => Color32::from_rgb(210, 125, 104),
            Self::Audio => Color32::from_rgb(77, 188, 179),
            Self::Apps => Color32::from_rgb(222, 110, 190),
            Self::Documents => Color32::from_rgb(190, 210, 90),
            Self::Other => Color32::from_rgb(96, 128, 224),
        };
        // Opaque, bounded shades keep neighbouring cells distinct even in a Rust target tree.
        let factor = [1.0, 0.80, 1.13][variant % 3];
        Color32::from_rgb(
            (base.r() as f32 * factor).min(255.0) as u8,
            (base.g() as f32 * factor).min(255.0) as u8,
            (base.b() as f32 * factor).min(255.0) as u8,
        )
    }
}
/// The one grey: an entry that holds 0 bytes. The space map gives such entries no tile.
pub const EMPTY: Color32 = Color32::from_rgb(66, 69, 78);
/// Colour for an entry of `bytes` in `category`: grey only when it is empty.
pub fn color_for(category: Category, bytes: u64, variant: usize) -> Color32 {
    if bytes == 0 {
        EMPTY
    } else {
        category.color(variant)
    }
}
pub fn file(name: &str) -> Category {
    use Category::*;
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v" => Video,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "bmp" | "svg" | "raw" | "tiff" => Images,
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "iso" => Archives,
        "exe" | "dll" | "msi" | "com" => Apps,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "odt" => Documents,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" => Audio,
        "rs" | "rlib" | "rmeta" | "pdb" | "o" | "obj" | "c" | "h" | "cpp" | "py" | "js" | "ts"
        | "tsx" | "html" | "css" | "json" | "toml" | "lock" => Code,
        "sys" | "drv" | "efi" => System,
        _ => Other,
    }
}
/// Only a small histogram per visible child; no histogram per node in a large tree.
pub fn folders(children: &[&Node], all: &[Node]) -> std::collections::HashMap<usize, Category> {
    use std::collections::HashMap;
    let mut owners = vec![None; all.len()];
    let mut weights: HashMap<usize, [u64; 10]> = HashMap::new();
    for n in children {
        if n.id < owners.len() {
            owners[n.id] = Some(n.id);
            weights.insert(n.id, [0; 10]);
        }
    }
    for n in all {
        if owners[n.id].is_none() {
            owners[n.id] = n.parent.and_then(|p| owners[p]);
        }
        if !n.dir {
            if let Some(id) = owners[n.id] {
                let w = &mut weights.get_mut(&id).unwrap()[file(&n.name) as usize];
                *w = w.saturating_add(n.bytes.max(1));
            }
        }
    }
    children
        .iter()
        .map(|n| {
            let category = if !n.dir {
                file(&n.name)
            } else {
                match n.name.to_ascii_lowercase().as_str() {
                    "windows" | "system32" | "syswow64" | "winsxs" => Category::System,
                    "games" | "steamapps" => Category::Games,
                    _ => {
                        let w = weights.get(&n.id).copied().unwrap_or([0; 10]);
                        let sum: u64 = w.iter().sum();
                        let (index, bytes) = w.iter().enumerate().max_by_key(|(_, b)| **b).unwrap();
                        if sum > 0 && *bytes as f64 / sum as f64 >= 0.6 {
                            Category::ALL[index]
                        } else {
                            Category::Other
                        }
                    }
                }
            };
            (n.id, category)
        })
        .collect()
}
fn luminance(c: Color32) -> f64 {
    let channel = |v: u8| {
        let x = v as f64 / 255.0;
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}
pub fn text(bg: Color32) -> Color32 {
    if (luminance(bg) + 0.05) / 0.05 >= 1.05 / (luminance(bg) + 0.05) {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folder_uses_dominant_descendants_and_mixed_folders_are_other() {
        let n = |id, parent, name: &str, dir, bytes| Node {
            id,
            parent,
            name: name.into(),
            path: String::new(),
            dir,
            bytes,
            files: 0,
            modified: 0,
            dir_stamp: 0,
        };
        let nodes = [
            n(0, None, "root", true, 10),
            n(1, Some(0), "nested", true, 10),
            n(2, Some(1), "deep", true, 10),
            n(3, Some(2), "movie.mkv", false, 10),
        ];
        assert_eq!(folders(&[&nodes[1]], &nodes)[&1], Category::Video);
        let nodes = [
            n(0, None, "root", true, 10),
            n(1, Some(0), "mixed", true, 10),
            n(2, Some(1), "x.mkv", false, 5),
            n(3, Some(1), "x.png", false, 5),
        ];
        assert_eq!(folders(&[&nodes[1]], &nodes)[&1], Category::Other);
    }
    #[test]
    fn every_category_shade_and_selection_meets_wcag_aa() {
        for c in Category::ALL {
            for shade in 0..3 {
                let bg = c.color(shade);
                let fg = text(bg);
                let a = luminance(bg);
                let b = luminance(fg);
                assert!(
                    (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                    "{c:?} shade {shade}"
                );
            }
        }
        let bg = crate::theme::ACCENT;
        let a = luminance(bg);
        let b = luminance(text(bg));
        assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
    }
    #[test]
    fn same_category_cells_have_distinct_opaque_shades() {
        for c in Category::ALL {
            assert_ne!(c.color(0), c.color(1));
            assert_ne!(c.color(1), c.color(2));
            assert_eq!(c.color(0).a(), 255);
        }
    }
    fn saturation(c: Color32) -> f32 {
        let max = c.r().max(c.g()).max(c.b()) as f32;
        let min = c.r().min(c.g()).min(c.b()) as f32;
        (max - min) / max.max(1.0)
    }
    #[test]
    fn only_empty_entries_are_grey_and_every_group_has_its_own_hue() {
        for c in Category::ALL {
            assert!(saturation(c.color(0)) >= 0.35, "{c:?} reads as grey");
            assert_eq!(color_for(c, 0, 0), EMPTY);
            assert_eq!(color_for(c, 1, 0), c.color(0));
        }
        assert!(saturation(EMPTY) < 0.2);
        let mut seen = Category::ALL.map(|c| c.color(0)).to_vec();
        seen.sort_by_key(|c| c.to_array());
        seen.dedup();
        assert_eq!(seen.len(), Category::ALL.len());
    }
    #[test]
    fn groups_are_case_insensitive() {
        assert_eq!(file("movie.MKV"), Category::Video);
        assert_eq!(file("photo.PNG"), Category::Images);
        assert_eq!(file("driver.sys"), Category::System);
        assert_eq!(file("build.rlib"), Category::Code);
    }
}
