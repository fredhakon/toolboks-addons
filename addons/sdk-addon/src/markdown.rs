//! A Markdown document as plain data: [`parse`] turns pulldown-cmark's events into [`Block`]s,
//! which the reader then draws. Nothing here touches egui, the disk or the network, so the model
//! is tested on its own. Images become a text placeholder and raw HTML stays text.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;

/// How deep block quotes and lists nest before deeper ones are drawn flat, so a hostile file of
/// `>>>>>>…` cannot run the drawing out of stack.
pub const MAX_DEPTH: usize = 24;

/// How a run of text looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub code: bool,
    /// The placeholder for an image: `[image: <alt text>]`.
    pub image: bool,
}

/// A run of text with one style, and the link it belongs to, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

/// The text of a paragraph, heading or table cell.
pub type Inline = Vec<Span>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// `level` is 1 to 6; `anchor` is the `#anchor` that links to it.
    Heading {
        level: u8,
        anchor: String,
        text: Inline,
    },
    Paragraph(Inline),
    /// `start` is the first number of an ordered list, `None` for bullets.
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Quote(Vec<Block>),
    /// A fenced or indented code block, without its last line break.
    Code(String),
    Table {
        aligns: Vec<Align>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Rule,
    /// A raw HTML block, shown as the text it is.
    Html(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// `Some(done)` for a task list item (`- [ ]` or `- [x]`).
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// A parsed document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

impl Doc {
    /// Whether some heading answers `#anchor`.
    pub fn has_anchor(&self, anchor: &str) -> bool {
        fn walk(blocks: &[Block], anchor: &str) -> bool {
            blocks.iter().any(|block| match block {
                Block::Heading { anchor: a, .. } => a == anchor,
                Block::Quote(inner) => walk(inner, anchor),
                Block::List { items, .. } => items.iter().any(|item| walk(&item.blocks, anchor)),
                _ => false,
            })
        }
        walk(&self.blocks, anchor)
    }
}

/// The text of `spans` without styles.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// The anchor GitHub gives a heading: lower case, letters, digits, `-` and `_` kept, spaces
/// become `-`, everything else dropped.
pub fn slug(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// Parses `text` (CommonMark with tables, strikethrough and task lists).
pub fn parse(text: &str) -> Doc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    builder.finish()
}

enum Container {
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Item(Item),
}

#[derive(Default)]
struct TableBuild {
    aligns: Vec<Align>,
    header: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    row: Vec<Inline>,
}

#[derive(Default)]
struct Builder {
    root: Vec<Block>,
    stack: Vec<Container>,
    /// One entry per open quote, list or item: whether it got a container (false past
    /// [`MAX_DEPTH`]).
    pushed: Vec<bool>,
    /// The text being collected for a paragraph, heading or table cell.
    inline: Option<Inline>,
    /// `inline` was opened by text outside a paragraph (a tight list item).
    implicit: bool,
    heading: Option<(u8, Option<String>)>,
    code: Option<String>,
    html: Option<String>,
    table: Option<TableBuild>,
    strong: u32,
    emphasis: u32,
    strike: u32,
    links: Vec<String>,
    image_depth: u32,
    image_alt: String,
    anchors: HashMap<String, usize>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, Style::default()),
            Event::Code(text) => self.text(
                &text,
                Style {
                    code: true,
                    ..Style::default()
                },
            ),
            Event::Html(text) => match &mut self.html {
                Some(html) => html.push_str(&text),
                None => self.text(&text, Style::default()),
            },
            Event::InlineHtml(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.text(&text, Style::default())
            }
            Event::FootnoteReference(name) => self.text(&format!("[{name}]"), Style::default()),
            Event::SoftBreak => self.text(" ", Style::default()),
            Event::HardBreak => self.text("\n", Style::default()),
            Event::Rule => self.push_block(Block::Rule),
            Event::TaskListMarker(done) => {
                let item = self.stack.iter_mut().rev().find_map(|c| match c {
                    Container::Item(item) => Some(item),
                    _ => None,
                });
                if let Some(item) = item {
                    item.task = Some(done);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush_implicit();
                self.inline = Some(Vec::new());
            }
            Tag::Heading { level, id, .. } => {
                self.flush_implicit();
                self.heading = Some((heading_level(level), id.map(|id| id.to_string())));
                self.inline = Some(Vec::new());
            }
            Tag::BlockQuote(_) => self.open(Container::Quote(Vec::new())),
            Tag::List(start) => self.open(Container::List {
                start,
                items: Vec::new(),
            }),
            Tag::Item => {
                self.flush_implicit();
                let in_list = matches!(self.stack.last(), Some(Container::List { .. }));
                if in_list {
                    self.stack.push(Container::Item(Item::default()));
                }
                self.pushed.push(in_list);
            }
            Tag::CodeBlock(_) => {
                self.flush_implicit();
                self.code = Some(String::new());
            }
            Tag::HtmlBlock => {
                self.flush_implicit();
                self.html = Some(String::new());
            }
            Tag::Table(aligns) => {
                self.flush_implicit();
                self.table = Some(TableBuild {
                    aligns: aligns.into_iter().map(align).collect(),
                    ..TableBuild::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => self.inline = Some(Vec::new()),
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { .. } => {
                if self.image_depth == 0 {
                    self.image_alt.clear();
                }
                self.image_depth += 1;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some(text) = self.inline.take() {
                    if !text.is_empty() {
                        self.push_block(Block::Paragraph(text));
                    }
                }
            }
            TagEnd::Heading(_) => {
                let text = self.inline.take().unwrap_or_default();
                let (level, id) = self.heading.take().unwrap_or((1, None));
                let anchor = self.anchor(id.unwrap_or_else(|| slug(&plain(&text))));
                self.push_block(Block::Heading {
                    level,
                    anchor,
                    text,
                });
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item => self.close(),
            TagEnd::CodeBlock => {
                let mut code = self.code.take().unwrap_or_default();
                if code.ends_with('\n') {
                    code.pop();
                }
                self.push_block(Block::Code(code));
            }
            TagEnd::HtmlBlock => {
                let html = self.html.take().unwrap_or_default();
                self.push_block(Block::Html(html.trim_end().to_owned()));
            }
            TagEnd::TableCell => {
                let cell = self.inline.take().unwrap_or_default();
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Block::Table {
                        aligns: table.aligns,
                        header: table.header,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                self.image_depth = self.image_depth.saturating_sub(1);
                if self.image_depth == 0 {
                    let alt = std::mem::take(&mut self.image_alt);
                    let alt = alt.trim();
                    let text = if alt.is_empty() {
                        "[image]".to_owned()
                    } else {
                        format!("[image: {alt}]")
                    };
                    self.text(
                        &text,
                        Style {
                            image: true,
                            ..Style::default()
                        },
                    );
                }
            }
            _ => {}
        }
    }

    /// A unique anchor: the second "Usage" heading answers `#usage-1`, as on GitHub.
    fn anchor(&mut self, base: String) -> String {
        let seen = self.anchors.entry(base.clone()).or_insert(0);
        let anchor = if *seen == 0 {
            base
        } else {
            format!("{base}-{seen}")
        };
        *seen += 1;
        anchor
    }

    fn text(&mut self, text: &str, mut style: Style) {
        if let Some(code) = &mut self.code {
            code.push_str(text);
            return;
        }
        if let Some(html) = &mut self.html {
            html.push_str(text);
            return;
        }
        if self.image_depth > 0 {
            self.image_alt.push_str(text);
            return;
        }
        style.strong = self.strong > 0;
        style.emphasis |= self.emphasis > 0;
        style.strike = self.strike > 0;
        let link = self.links.last().cloned();
        let spans = match &mut self.inline {
            Some(spans) => spans,
            None => {
                self.implicit = true;
                self.inline.insert(Vec::new())
            }
        };
        match spans.last_mut() {
            Some(last) if last.style == style && last.link == link => last.text.push_str(text),
            _ => spans.push(Span {
                text: text.to_owned(),
                style,
                link,
            }),
        }
    }

    /// Ends text that a tight list item had outside any paragraph.
    fn flush_implicit(&mut self) {
        if !self.implicit {
            return;
        }
        self.implicit = false;
        if let Some(text) = self.inline.take() {
            if !text.is_empty() {
                self.push_block(Block::Paragraph(text));
            }
        }
    }

    fn push_block(&mut self, block: Block) {
        self.flush_implicit();
        match self.stack.last_mut() {
            None => self.root.push(block),
            Some(Container::Quote(blocks)) => blocks.push(block),
            Some(Container::Item(item)) => item.blocks.push(block),
            Some(Container::List { items, .. }) => items.push(Item {
                task: None,
                blocks: vec![block],
            }),
        }
    }

    fn open(&mut self, container: Container) {
        self.flush_implicit();
        let room = self.stack.len() < MAX_DEPTH;
        if room {
            self.stack.push(container);
        }
        self.pushed.push(room);
    }

    fn close(&mut self) {
        self.flush_implicit();
        if self.pushed.pop() != Some(true) {
            return;
        }
        match self.stack.pop() {
            Some(Container::Item(item)) => match self.stack.last_mut() {
                Some(Container::List { items, .. }) => items.push(item),
                _ => {
                    for block in item.blocks {
                        self.push_block(block);
                    }
                }
            },
            Some(Container::Quote(blocks)) => self.push_block(Block::Quote(blocks)),
            Some(Container::List { start, items }) => self.push_block(Block::List { start, items }),
            None => {}
        }
    }

    fn finish(mut self) -> Doc {
        self.flush_implicit();
        while !self.pushed.is_empty() {
            self.close();
        }
        Doc { blocks: self.root }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn align(alignment: Alignment) -> Align {
    match alignment {
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
        Alignment::None | Alignment::Left => Align::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str) -> Span {
        Span {
            text: text.into(),
            style: Style::default(),
            link: None,
        }
    }

    /// Every supported element in one document becomes the block it should.
    #[test]
    fn every_supported_element_becomes_its_block() {
        let text = "# Title\n\
            \n\
            ## Second\n\
            ### Third\n\
            #### Fourth\n\
            ##### Fifth\n\
            ###### Sixth\n\
            \n\
            Plain **bold** *italic* ~~gone~~ `code` [web](https://example.com) and a  \n\
            hard break, a\n\
            soft break.\n\
            \n\
            - one\n\
            - two\n\
            \x20 1. nested\n\
            \x20 2. again\n\
            \n\
            3. three\n\
            4. four\n\
            \n\
            - [ ] open task\n\
            - [x] done task\n\
            \n\
            > quoted *text*\n\
            > > deeper\n\
            \n\
            ```rust\n\
            fn main() {}\n\
            ```\n\
            \n\
            \x20   indented code\n\
            \n\
            | Left | Middle | Right |\n\
            |:-----|:------:|------:|\n\
            | a | b | c |\n\
            | d | `e` | f |\n\
            \n\
            ---\n\
            \n\
            ![a cat](https://example.com/cat.png)\n\
            \n\
            <div onclick=\"x()\">raw</div>\n\
            \n\
            Inline <b>html</b> stays text.\n";
        let doc = parse(text);
        let kinds: Vec<&str> = doc
            .blocks
            .iter()
            .map(|block| match block {
                Block::Heading { .. } => "heading",
                Block::Paragraph(_) => "paragraph",
                Block::List { .. } => "list",
                Block::Quote(_) => "quote",
                Block::Code(_) => "code",
                Block::Table { .. } => "table",
                Block::Rule => "rule",
                Block::Html(_) => "html",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "heading",
                "heading",
                "heading",
                "heading",
                "heading",
                "heading",
                "paragraph",
                "list",
                "list",
                "list",
                "quote",
                "code",
                "code",
                "table",
                "rule",
                "paragraph",
                "html",
                "paragraph"
            ]
        );
        let levels: Vec<u8> = doc
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Heading { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, [1, 2, 3, 4, 5, 6]);
        assert!(doc.has_anchor("title") && doc.has_anchor("sixth"));

        // The paragraph's styles, the link and both kinds of break.
        let Block::Paragraph(spans) = &doc.blocks[6] else {
            unreachable!()
        };
        assert!(spans.iter().any(|s| s.text == "bold" && s.style.strong));
        assert!(spans.iter().any(|s| s.text == "italic" && s.style.emphasis));
        assert!(spans.iter().any(|s| s.text == "gone" && s.style.strike));
        assert!(spans.iter().any(|s| s.text == "code" && s.style.code));
        assert!(spans
            .iter()
            .any(|s| s.text == "web" && s.link.as_deref() == Some("https://example.com")));
        let all = plain(spans);
        assert!(all.contains("a\nhard break, a soft break."), "{all:?}");

        // Bullets with a nested ordered list, then an ordered list from 3.
        let Block::List { start: None, items } = &doc.blocks[7] else {
            unreachable!()
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].blocks, [Block::Paragraph(vec![span("one")])]);
        assert!(matches!(
            &items[1].blocks[1],
            Block::List { start: Some(1), items } if items.len() == 2
        ));
        assert!(matches!(&doc.blocks[8], Block::List { start: Some(3), .. }));
        let Block::List { items, .. } = &doc.blocks[9] else {
            unreachable!()
        };
        assert_eq!(
            items.iter().map(|i| i.task).collect::<Vec<_>>(),
            [Some(false), Some(true)]
        );

        let Block::Quote(inner) = &doc.blocks[10] else {
            unreachable!()
        };
        assert!(matches!(inner[1], Block::Quote(_)));
        assert_eq!(doc.blocks[11], Block::Code("fn main() {}".into()));
        assert_eq!(doc.blocks[12], Block::Code("indented code".into()));

        let Block::Table {
            aligns,
            header,
            rows,
        } = &doc.blocks[13]
        else {
            unreachable!()
        };
        assert_eq!(aligns, &[Align::Left, Align::Center, Align::Right]);
        assert_eq!(plain(&header[1]), "Middle");
        assert_eq!(rows.len(), 2);
        assert!(rows[1][1][0].style.code);

        // The image is a placeholder; nothing points at its address.
        let Block::Paragraph(image) = &doc.blocks[15] else {
            unreachable!()
        };
        assert_eq!(plain(image), "[image: a cat]");
        assert!(image[0].style.image && image[0].link.is_none());
        // Raw HTML is kept as text, never read as markup.
        assert_eq!(
            doc.blocks[16],
            Block::Html("<div onclick=\"x()\">raw</div>".into())
        );
        let Block::Paragraph(inline_html) = &doc.blocks[17] else {
            unreachable!()
        };
        assert_eq!(plain(inline_html), "Inline <b>html</b> stays text.");
    }

    #[test]
    fn anchors_follow_github() {
        assert_eq!(slug("The five edits, in order"), "the-five-edits-in-order");
        assert_eq!(
            slug("  `cargo test` & more_stuff "),
            "cargo-test--more_stuff"
        );
        let doc = parse("# Usage\n\n## Usage\n\n### Ünïcode!\n");
        assert!(doc.has_anchor("usage") && doc.has_anchor("usage-1"));
        assert!(doc.has_anchor("ünïcode"));
        assert!(!doc.has_anchor("missing"));
    }

    /// Thousands of nested quotes and lists parse, and nest no deeper than `MAX_DEPTH`.
    #[test]
    fn deep_nesting_is_flattened() {
        fn depth(blocks: &[Block]) -> usize {
            blocks
                .iter()
                .map(|block| match block {
                    Block::Quote(inner) => 1 + depth(inner),
                    Block::List { items, .. } => {
                        1 + items.iter().map(|i| depth(&i.blocks)).max().unwrap_or(0)
                    }
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        }
        let quotes = format!("{} deep\n", ">".repeat(5000));
        let doc = parse(&quotes);
        assert!(depth(&doc.blocks) <= MAX_DEPTH);
        assert!(plain_of(&doc).contains("deep"));
        let lists: String = (0..300)
            .map(|n| format!("{}- level {n}\n", "  ".repeat(n)))
            .collect();
        let doc = parse(&lists);
        assert!(depth(&doc.blocks) <= MAX_DEPTH);
        assert!(plain_of(&doc).contains("level 299"));
    }

    fn plain_of(doc: &Doc) -> String {
        fn walk(blocks: &[Block], out: &mut String) {
            for block in blocks {
                match block {
                    Block::Heading { text, .. } | Block::Paragraph(text) => {
                        out.push_str(&plain(text))
                    }
                    Block::Quote(inner) => walk(inner, out),
                    Block::List { items, .. } => items.iter().for_each(|i| walk(&i.blocks, out)),
                    Block::Code(text) | Block::Html(text) => out.push_str(text),
                    _ => {}
                }
                out.push('\n');
            }
        }
        let mut out = String::new();
        walk(&doc.blocks, &mut out);
        out
    }

    #[test]
    fn empty_and_odd_input_does_not_panic() {
        assert_eq!(parse(""), Doc::default());
        for text in [
            "|",
            "- [ ]",
            "> ",
            "```",
            "![]()",
            "[x](",
            "<",
            "\u{feff}# x",
            "\0",
        ] {
            parse(text);
        }
        assert_eq!(
            parse("![](a.png)").blocks,
            [Block::Paragraph(vec![Span {
                text: "[image]".into(),
                style: Style {
                    image: true,
                    ..Style::default()
                },
                link: None,
            }])]
        );
    }
}
