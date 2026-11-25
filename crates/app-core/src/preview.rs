//! The preview pane's pure side: file kinds, text lines and tab rules.

use protocol::message::SentFile;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::ops::Range;
use std::path::Path;

/// Largest image the pane decodes, in pixels; a larger one shows "Too large to preview".
pub const MAX_IMAGE_PIXELS: u64 = 50_000_000;
/// Largest file with an unknown extension that is shown as text, in bytes.
pub const MAX_UNKNOWN_TEXT: usize = 1024 * 1024;
/// Most characters of one text line that are drawn; the rest is replaced by `…`.
pub const MAX_LINE_CHARS: usize = 10_000;

/// The image formats the pane draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageType {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
    /// SVG.
    Svg,
}

/// How the pane treats a file, decided by its name only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// An image the pane draws.
    Image(ImageType),
    /// Text with a known extension.
    Text,
    /// An HTML file: a card that opens the browser, never read.
    Html,
    /// A type the pane never reads.
    NotPreviewed,
    /// Any other extension: text only when the bytes are valid UTF-8 and small enough.
    Unknown,
}

const TEXT_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "log", "json", "jsonl", "yaml", "yml", "toml", "ini", "cfg", "conf", "xml", "rs", "py",
    "js", "mjs", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp", "cs", "rb", "php", "sh", "bash",
    "zsh", "fish", "sql", "css", "scss", "lua", "swift", "diff", "patch", "env",
];

const NOT_PREVIEWED_EXTENSIONS: &[&str] = &[
    "pdf", "csv", "tsv", "mp4", "mov", "webm", "mkv", "avi", "mp3", "wav", "ogg", "flac", "zip", "gz", "tgz", "tar",
    "xz", "7z", "rar", "docx", "xlsx", "pptx", "odt", "ods", "odp", "exe", "dll", "so", "dylib", "bin", "iso", "dmg",
];

/// Classify a file by its lower-cased extension (or, for `Dockerfile` and `Makefile`, its name).
pub fn kind_of(path: &Path) -> FileKind {
    if matches!(path.file_name().and_then(|n| n.to_str()), Some("Dockerfile" | "Makefile")) {
        return FileKind::Text;
    }
    let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_lowercase()) else {
        return FileKind::Unknown;
    };
    match ext.as_str() {
        "png" => FileKind::Image(ImageType::Png),
        "jpg" | "jpeg" => FileKind::Image(ImageType::Jpeg),
        "gif" => FileKind::Image(ImageType::Gif),
        "webp" => FileKind::Image(ImageType::Webp),
        "svg" => FileKind::Image(ImageType::Svg),
        "html" | "htm" => FileKind::Html,
        e if TEXT_EXTENSIONS.contains(&e) => FileKind::Text,
        e if NOT_PREVIEWED_EXTENSIONS.contains(&e) => FileKind::NotPreviewed,
        _ => FileKind::Unknown,
    }
}

/// Whether the pane reads the file's bytes at all (false for HTML and the never-read types).
pub fn is_read(kind: FileKind) -> bool {
    !matches!(kind, FileKind::Html | FileKind::NotPreviewed)
}

/// The pixel size of an image: the header for raster types, the root element for SVG; `None` for garbage.
pub fn image_size(bytes: &[u8], kind: ImageType) -> Option<(u32, u32)> {
    if kind != ImageType::Svg {
        let size = imagesize::blob_size(bytes).ok()?;
        return Some((u32::try_from(size.width).ok()?, u32::try_from(size.height).ok()?));
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]);
    let start = head.find("<svg")?;
    let tag = &head[start..];
    let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
    if let Some(view_box) = svg_attr(tag, "viewBox") {
        let nums: Vec<f64> = view_box
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if let [_, _, w, h] = nums[..]
            && w > 0.0
            && h > 0.0
        {
            return Some((w.round() as u32, h.round() as u32));
        }
    }
    let length = |name: &str| -> Option<u32> {
        let v = svg_attr(tag, name)?;
        let n: f64 = v.trim().trim_end_matches("px").trim().parse().ok()?;
        (n > 0.0).then(|| n.round() as u32)
    };
    Some(match (length("width"), length("height")) {
        (Some(w), Some(h)) => (w, h),
        _ => (300, 150),
    })
}

/// The value of attribute `name` in an element's text, matching whole attribute names only.
fn svg_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        if !tag[..at].ends_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let rest = tag[from..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &rest[1..];
        return rest.find(quote).map(|end| &rest[..end]);
    }
    None
}

/// A text file split into lines, for the pane's line list.
#[derive(Debug)]
pub struct TextDoc {
    text: String,
    lines: Vec<Range<usize>>,
    widest: usize,
}

impl TextDoc {
    /// Parse UTF-8 bytes (a leading BOM is dropped); `None` when they are not valid UTF-8.
    pub fn parse(bytes: &[u8]) -> Option<TextDoc> {
        let text = std::str::from_utf8(bytes).ok()?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text).to_string();
        let mut lines = Vec::new();
        let mut start = 0;
        for piece in text.split_inclusive('\n') {
            let end = start + piece.len();
            let body = piece.strip_suffix('\n').unwrap_or(piece);
            let body = body.strip_suffix('\r').unwrap_or(body);
            lines.push(start..start + body.len());
            start = end;
        }
        let mut doc = TextDoc { text, lines, widest: 0 };
        doc.widest = (0..doc.line_count()).map(|i| doc.display(i).chars().count()).max().unwrap_or(0);
        Some(doc)
    }

    /// The number of lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// One line as drawn: tabs as 4 spaces, cut at `MAX_LINE_CHARS` characters plus `…`.
    pub fn display(&self, line: usize) -> Cow<'_, str> {
        let raw = &self.text[self.lines[line].clone()];
        // Work is bounded by `MAX_LINE_CHARS`, however long the line is.
        if raw.len() <= MAX_LINE_CHARS && !raw.contains('\t') {
            return Cow::Borrowed(raw);
        }
        let mut out = String::new();
        let mut count = 0;
        for c in raw.chars() {
            let (shown, times) = if c == '\t' { (' ', 4) } else { (c, 1) };
            for _ in 0..times {
                if count == MAX_LINE_CHARS {
                    out.push('…');
                    return Cow::Owned(out);
                }
                out.push(shown);
                count += 1;
            }
        }
        Cow::Owned(out)
    }

    /// The character count of the widest displayed line.
    pub fn widest(&self) -> usize {
        self.widest
    }

    /// The full lines in `lines`, joined by `\n`.
    pub fn copy(&self, lines: Range<usize>) -> String {
        let end = lines.end.min(self.lines.len());
        let start = lines.start.min(end);
        self.lines[start..end].iter().map(|r| &self.text[r.clone()]).collect::<Vec<_>>().join("\n")
    }

    /// The whole text (BOM dropped, line endings as in the file).
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The files shown as tabs: oldest first, the removed ids left out.
pub fn listed<'a>(files: &'a [SentFile], removed: Option<&BTreeSet<String>>) -> Vec<&'a SentFile> {
    let mut out: Vec<&SentFile> = files.iter().filter(|f| removed.is_none_or(|r| !r.contains(&f.id))).collect();
    out.sort_by_key(|f| f.sent_ms);
    out
}

/// The active tab's file id: `saved` while it is listed, else the newest.
pub fn active_id<'a>(listed: &[&'a SentFile], saved: Option<&str>) -> Option<&'a str> {
    saved.and_then(|s| listed.iter().find(|f| f.id == s)).or(listed.last()).map(|f| f.id.as_str())
}

/// The file to activate after `removed` is hidden: the newest other one.
pub fn after_remove(listed: &[&SentFile], removed: &str) -> Option<String> {
    listed.iter().rev().find(|f| f.id != removed).map(|f| f.id.clone())
}

/// The file to jump to after a new list: the newest one that is not backlog and not in `known`.
/// `None` when `known` is `None`, since the first list a pane sees only fills.
pub fn newly_sent(known: Option<&HashSet<String>>, files: &[SentFile]) -> Option<String> {
    let known = known?;
    files.iter().rev().find(|f| !f.backlog && !known.contains(&f.id)).map(|f| f.id.clone())
}

/// Which file tabs are drawn and which go under the `+N` button.
#[derive(Debug, PartialEq)]
pub struct TabFit {
    /// Indexes of the drawn tabs, ascending.
    pub shown: Vec<usize>,
    /// Indexes under `+N`, ascending.
    pub hidden: Vec<usize>,
}

/// Fit `count` tabs of width `tab` into `width`: all when they fit, else the newest that fit beside the `plus`
/// button, with the active one replacing the oldest shown when it would be hidden.
pub fn fit_tabs(count: usize, active: Option<usize>, width: f32, tab: f32, plus: f32) -> TabFit {
    if count as f32 * tab <= width {
        return TabFit { shown: (0..count).collect(), hidden: Vec::new() };
    }
    let room = (((width - plus) / tab).floor().max(0.0) as usize).max(1).min(count);
    let mut shown: Vec<usize> = (count - room..count).collect();
    if let Some(a) = active.filter(|a| *a < count && !shown.contains(a)) {
        shown[0] = a;
        shown.sort_unstable();
    }
    let hidden = (0..count).filter(|i| !shown.contains(i)).collect();
    TabFit { shown, hidden }
}

/// A path's last component.
pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The preview row's title: `Preview · <name>`, or `Preview` with no files.
pub fn row_title(active: Option<&SentFile>) -> String {
    match active {
        Some(f) => format!("Preview · {}", file_name(&f.path)),
        None => "Preview".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::SentFile;
    use std::collections::{BTreeSet, HashSet};
    use std::path::{Path, PathBuf};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    fn file(id: &str, backlog: bool) -> SentFile {
        SentFile { id: id.into(), path: PathBuf::from(format!("/t/{id}.md")), sent_ms: 0, caption: None, backlog }
    }

    #[test]
    fn kinds_follow_the_extension() {
        assert_eq!(kind_of(Path::new("a.PNG")), FileKind::Image(ImageType::Png));
        assert_eq!(kind_of(Path::new("x.jpeg")), FileKind::Image(ImageType::Jpeg));
        assert_eq!(kind_of(Path::new("d.svg")), FileKind::Image(ImageType::Svg));
        assert_eq!(kind_of(Path::new("n.md")), FileKind::Text);
        assert_eq!(kind_of(Path::new("Makefile")), FileKind::Text);
        assert_eq!(kind_of(Path::new("p.html")), FileKind::Html);
        assert_eq!(kind_of(Path::new("r.pdf")), FileKind::NotPreviewed);
        assert_eq!(kind_of(Path::new("data.csv")), FileKind::NotPreviewed);
        assert_eq!(kind_of(Path::new("blob.xyz")), FileKind::Unknown);
        assert_eq!(kind_of(Path::new("noext")), FileKind::Unknown);
        assert!(!is_read(FileKind::Html));
        assert!(!is_read(FileKind::NotPreviewed));
        assert!(is_read(FileKind::Text));
    }

    #[test]
    fn image_size_reads_png_header_and_svg_viewbox() {
        assert_eq!(image_size(&png(1, 1), ImageType::Png), Some((1, 1)));
        assert_eq!(image_size(br#"<svg viewBox="0 0 640 480">"#, ImageType::Svg), Some((640, 480)));
        assert_eq!(image_size(br#"<svg width="32px" height="16">"#, ImageType::Svg), Some((32, 16)));
        assert_eq!(image_size(b"<svg>", ImageType::Svg), Some((300, 150)));
        assert_eq!(image_size(b"nope", ImageType::Png), None);
    }

    #[test]
    fn huge_images_are_over_the_pixel_cap() {
        let (w, h) = image_size(&png(20000, 20000), ImageType::Png).unwrap();
        assert_eq!((w, h), (20000, 20000));
        assert!(u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS);
    }

    #[test]
    fn text_lines_split_on_both_endings() {
        let d = TextDoc::parse(b"a\r\nb\nc\n").unwrap();
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.display(1), "b");
        assert_eq!(TextDoc::parse(b"\xef\xbb\xbfhi").unwrap().display(0), "hi");
        assert!(TextDoc::parse(&[0xff, 0xfe, 0x00]).is_none());
    }

    #[test]
    fn long_lines_are_cut_for_display_but_copied_whole() {
        let d = TextDoc::parse("x".repeat(25_000).as_bytes()).unwrap();
        let shown = d.display(0);
        assert_eq!(shown.chars().count(), 10_001);
        assert!(shown.ends_with('…'));
        assert_eq!(d.copy(0..1).chars().count(), 25_000);
        assert_eq!(d.widest(), 10_001);
    }

    #[test]
    fn tabs_draw_as_four_spaces() {
        assert_eq!(TextDoc::parse(b"\tx").unwrap().display(0), "    x");
    }

    #[test]
    fn listed_leaves_out_removed_ids_oldest_first() {
        let files = [file("a", false), file("b", false), file("c", false)];
        let removed: BTreeSet<String> = ["b".to_string()].into();
        let ids: Vec<_> = listed(&files, Some(&removed)).iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
        assert_eq!(listed(&files, None).len(), 3);
    }

    #[test]
    fn active_falls_back_to_the_newest() {
        let files = [file("a", false), file("b", false), file("c", false)];
        let l = listed(&files, None);
        assert_eq!(active_id(&l, Some("b")), Some("b"));
        assert_eq!(active_id(&l, Some("gone")), Some("c"));
        assert_eq!(active_id(&l, None), Some("c"));
        assert_eq!(active_id(&[], None), None);
    }

    #[test]
    fn removing_activates_the_newest_left() {
        let files = [file("a", false), file("b", false), file("c", false)];
        let l = listed(&files, None);
        assert_eq!(after_remove(&l, "c"), Some("b".to_string()));
        assert_eq!(after_remove(&l, "a"), Some("c".to_string()));
        assert_eq!(after_remove(&l[..1], "a"), None);
    }

    #[test]
    fn the_first_list_only_fills() {
        let files = [file("a", false), file("b", true), file("c", false)];
        assert_eq!(newly_sent(None, &files), None);
        let known: HashSet<String> = ["a".to_string()].into();
        assert_eq!(newly_sent(Some(&known), &files), Some("c".to_string()));
        let known: HashSet<String> = ["a".to_string(), "c".to_string()].into();
        assert_eq!(newly_sent(Some(&known), &files), None);
    }

    #[test]
    fn overflow_hides_the_oldest_and_keeps_the_active_tab() {
        let t = fit_tabs(9, Some(8), 500., 160., 44.);
        assert_eq!(t.shown, [7, 8]);
        assert_eq!(t.hidden, (0..=6).collect::<Vec<_>>());
        let t = fit_tabs(9, Some(0), 500., 160., 44.);
        assert_eq!(t.shown, [0, 8]);
        assert_eq!(t.hidden, (1..=7).collect::<Vec<_>>());
        let t = fit_tabs(3, None, 500., 160., 44.);
        assert_eq!(t.shown, [0, 1, 2]);
        assert!(t.hidden.is_empty());
    }

    #[test]
    fn row_title_names_the_active_file() {
        assert_eq!(row_title(None), "Preview");
        assert_eq!(row_title(Some(&file("a", false))), "Preview · a.md");
    }

    #[test]
    fn huge_lines_take_bounded_work() {
        let start = std::time::Instant::now();
        let plain = TextDoc::parse("x".repeat(25_000_000).as_bytes()).unwrap();
        assert_eq!(plain.display(0).chars().count(), MAX_LINE_CHARS + 1);
        let tabs = TextDoc::parse("\t".repeat(25_000_000).as_bytes()).unwrap();
        let shown = tabs.display(0);
        assert_eq!(shown.chars().count(), MAX_LINE_CHARS + 1);
        assert!(shown.starts_with("    ") && shown.ends_with('…'));
        assert_eq!(tabs.widest(), MAX_LINE_CHARS + 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "took {:?}", start.elapsed());
    }
}
