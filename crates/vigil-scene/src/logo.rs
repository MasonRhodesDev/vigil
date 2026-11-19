use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use resvg::usvg::{self, roxmltree};

use crate::Error;

pub const MAX_LOGO_BYTES: u64 = 512 * 1024;
pub const MAX_EDGE: u32 = 2048;
pub const MAX_RASTER_BYTES: u64 = 16 << 20;
const MAX_RASTER_SIDE: u32 = 8192;
const MAX_NODES: u32 = 10_000;
const MAX_DEPTH: usize = 64;
const MAX_REF_DEPTH: usize = 128;
const MAX_EXPANDED: usize = 20_000;
const REACH: f32 = 16.0;
const MAX_NUMBER: f64 = 1e6;
const SVG_NS: &str = "http://www.w3.org/2000/svg";
const DRAWN: &[&str] = &[
    "svg",
    "g",
    "defs",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "use",
    "symbol",
    "linearGradient",
    "radialGradient",
    "stop",
    "clipPath",
    "mask",
    "title",
    "desc",
    "metadata",
    "style",
];

pub enum Logo {
    Svg(Box<usvg::Tree>),
    Raster(image::RgbaImage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    edge: u32,
    alpha: Vec<u8>,
}

impl Mask {
    pub fn edge(&self) -> u32 {
        self.edge
    }

    pub fn alpha(&self) -> &[u8] {
        &self.alpha
    }

    pub fn sprite(&self, rgb: [u8; 3]) -> Vec<u8> {
        self.alpha
            .iter()
            .flat_map(|&a| [rgb[0], rgb[1], rgb[2], a])
            .collect()
    }
}

pub fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, Error> {
    let file = std::fs::File::open(path).map_err(|e| Error::Io(e.to_string()))?;
    let meta = file.metadata().map_err(|e| Error::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(Error::Refused("not a regular file".into()));
    }
    let mut data = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut data)
        .map_err(|e| Error::Io(e.to_string()))?;
    if data.len() as u64 > cap {
        return Err(Error::TooLarge(format!("over {cap} bytes")));
    }
    Ok(data)
}

impl Logo {
    pub fn load(path: &Path) -> Result<Logo, Error> {
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        {
            Logo::svg(&read_bounded(path, MAX_LOGO_BYTES)?)
        } else {
            Logo::raster(&read_bounded(path, MAX_RASTER_BYTES)?)
        }
    }

    pub fn svg(data: &[u8]) -> Result<Logo, Error> {
        if data.len() as u64 > MAX_LOGO_BYTES {
            return Err(Error::TooLarge(format!(
                "logo is over {MAX_LOGO_BYTES} bytes"
            )));
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| Error::Invalid("the SVG is not UTF-8 text (svgz is refused)".into()))?;
        if text.contains("<!ENTITY") {
            return Err(Error::Refused("the SVG declares XML entities".into()));
        }
        if nesting(text.as_bytes()) > MAX_DEPTH {
            return Err(Error::Refused(format!(
                "the SVG nests elements deeper than {MAX_DEPTH}"
            )));
        }
        let xml = roxmltree::ParsingOptions {
            allow_dtd: true,
            nodes_limit: MAX_NODES,
            ..roxmltree::ParsingOptions::default()
        };
        let doc = roxmltree::Document::parse_with_options(text, xml)
            .map_err(|e| Error::Invalid(format!("SVG: {e}")))?;
        screen(&doc)?;
        let options = usvg::Options {
            resources_dir: None,
            image_href_resolver: usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..usvg::Options::default()
        };
        let tree = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            usvg::Tree::from_xmltree(&doc, &options)
        }))
        .map_err(|_| Error::Invalid("the SVG parser could not read this SVG".into()))?
        .map_err(|e| Error::Invalid(format!("SVG: {e}")))?;
        let size = tree.size();
        screen_tree(tree.root(), REACH * size.width().max(size.height()))?;
        Ok(Logo::Svg(Box::new(tree)))
    }

    pub fn raster(data: &[u8]) -> Result<Logo, Error> {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_RASTER_SIDE);
        limits.max_image_height = Some(MAX_RASTER_SIDE);
        limits.max_alloc = Some(256 << 20);
        let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        reader.limits(limits);
        let img = reader.decode().map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Logo::Raster(img.into_rgba8()))
    }

    pub fn mask(&self, edge: u32) -> Result<Mask, Error> {
        if edge == 0 || edge > MAX_EDGE {
            return Err(Error::TooLarge(format!(
                "a logo {edge} px wide is outside 1..={MAX_EDGE}"
            )));
        }
        let mut alpha = match self {
            Logo::Svg(tree) => {
                let size = tree.size();
                let s = edge as f32 / size.width().max(size.height());
                let ox = (edge as f32 - size.width() * s) / 2.0;
                let oy = (edge as f32 - size.height() * s) / 2.0;
                let mut pixmap = resvg::tiny_skia::Pixmap::new(edge, edge)
                    .ok_or_else(|| Error::Invalid("empty logo box".into()))?;
                let fit = resvg::tiny_skia::Transform::from_row(s, 0.0, 0.0, s, ox, oy);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    resvg::render(tree, fit, &mut pixmap.as_mut());
                }))
                .map_err(|_| Error::Invalid("the renderer could not draw this SVG".into()))?;
                let (pixels, _partial) = pixmap.data().as_chunks::<4>();
                pixels.iter().map(|p| p[3]).collect::<Vec<u8>>()
            }
            Logo::Raster(img) => {
                let ratio =
                    (f64::from(edge) / f64::from(img.width().max(img.height()))).min(f64::MAX);
                let (w, h) = (
                    ((f64::from(img.width()) * ratio).round() as u32).max(1),
                    ((f64::from(img.height()) * ratio).round() as u32).max(1),
                );
                let resized;
                let img = if (w, h) == img.dimensions() {
                    img
                } else {
                    resized =
                        image::imageops::resize(img, w, h, image::imageops::FilterType::Lanczos3);
                    &resized
                };
                let mut out = vec![0u8; (edge * edge) as usize];
                let (ox, oy) = ((edge - w.min(edge)) / 2, (edge - h.min(edge)) / 2);
                for (x, y, p) in img.enumerate_pixels() {
                    if x < edge && y < edge && ox + x < edge && oy + y < edge {
                        out[((oy + y) * edge + ox + x) as usize] = p[3];
                    }
                }
                out
            }
        };
        drop_specks(&mut alpha, edge as usize, edge as usize, 0.02);
        Ok(Mask { edge, alpha })
    }
}

fn screen(doc: &roxmltree::Document) -> Result<(), Error> {
    for n in doc.descendants().filter(roxmltree::Node::is_element) {
        let values = n
            .attributes()
            .filter(|a| a.namespace().is_none() && !matches!(a.name(), "id" | "class" | "href"))
            .map(|a| a.value())
            .chain(
                n.has_tag_name((SVG_NS, "style"))
                    .then(|| n.text())
                    .flatten(),
            );
        for value in values {
            if largest_number(value) > MAX_NUMBER {
                return Err(Error::Refused(format!(
                    "a number beyond {MAX_NUMBER} has no place in a logo"
                )));
            }
        }
        let name = n.tag_name();
        if name.namespace() == Some(SVG_NS) && !DRAWN.contains(&name.name()) {
            return Err(Error::Refused(format!(
                "<{}> has no place in a logo silhouette",
                name.name()
            )));
        }
    }
    let mut expansion = Expansion::new(doc)?;
    let mut total = expansion.size(doc.root_element(), 0)?;
    let elements = doc
        .descendants()
        .filter(roxmltree::Node::is_element)
        .count();
    let sheets = doc
        .descendants()
        .filter(|n| n.has_tag_name((SVG_NS, "style")))
        .filter_map(|n| n.text());
    for sheet in sheets {
        for target in expansion.targets(sheet) {
            total += expansion.size(target, 0)?.saturating_mul(elements);
            expansion.check(total)?;
        }
    }
    Ok(())
}

struct Expansion<'a, 'input> {
    ids: HashMap<&'a str, roxmltree::Node<'a, 'input>>,
    sizes: HashMap<roxmltree::NodeId, usize>,
    open: HashSet<roxmltree::NodeId>,
}

impl<'a, 'input> Expansion<'a, 'input> {
    fn new(doc: &'a roxmltree::Document<'input>) -> Result<Self, Error> {
        let mut ids = HashMap::new();
        for n in doc.descendants() {
            if let Some(id) = n.attribute("id")
                && ids.insert(id, n).is_some()
            {
                return Err(Error::Refused(format!("the SVG uses the id {id:?} twice")));
            }
        }
        Ok(Self {
            ids,
            sizes: HashMap::new(),
            open: HashSet::new(),
        })
    }

    fn targets(&self, value: &str) -> Vec<roxmltree::Node<'a, 'input>> {
        value
            .split('#')
            .skip(1)
            .filter_map(|rest| {
                let end = rest
                    .find(|c: char| c.is_whitespace() || "()'\",;".contains(c))
                    .unwrap_or(rest.len());
                self.ids.get(&rest[..end]).copied()
            })
            .collect()
    }

    fn check(&self, total: usize) -> Result<(), Error> {
        if total > MAX_EXPANDED {
            return Err(Error::Refused(format!(
                "the SVG expands to more than {MAX_EXPANDED} elements"
            )));
        }
        Ok(())
    }

    fn size(&mut self, node: roxmltree::Node<'a, 'input>, depth: usize) -> Result<usize, Error> {
        if let Some(&n) = self.sizes.get(&node.id()) {
            return Ok(n);
        }
        if depth > MAX_REF_DEPTH || !self.open.insert(node.id()) {
            return Err(Error::Refused(
                "the SVG references form a loop or a chain too deep to follow".into(),
            ));
        }
        let mut total = 1usize;
        let targets: Vec<_> = node
            .attributes()
            .flat_map(|a| self.targets(a.value()))
            .collect();
        for next in node
            .children()
            .filter(roxmltree::Node::is_element)
            .chain(targets)
        {
            total += self.size(next, depth + 1)?;
            self.check(total)?;
        }
        self.open.remove(&node.id());
        self.sizes.insert(node.id(), total);
        Ok(total)
    }
}

fn largest_number(text: &str) -> f64 {
    let b = text.as_bytes();
    let mut largest = 0.0f64;
    let mut i = 0;
    while i < b.len() {
        let starts = (b[i].is_ascii_digit() || b[i] == b'.')
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b"_#".contains(&b[i - 1])));
        if !starts {
            i += 1;
            continue;
        }
        let from = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            let mut j = i + 1;
            if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                j += 1;
            }
            if j < b.len() && b[j].is_ascii_digit() {
                i = j;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        if !text[from..i].bytes().any(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(v) = text[from..i].parse::<f64>() {
            largest = largest.max(v.abs());
        }
    }
    largest
}

// roxmltree recurses once per nesting level while it parses, so depth is
// bounded on the raw text first. The scan never counts fewer levels than
// the parser reaches before its first error.
fn nesting(text: &[u8]) -> usize {
    let skip_to = |from: usize, end: &[u8]| {
        text[from..]
            .windows(end.len())
            .position(|w| w == end)
            .map_or(text.len(), |p| from + p + end.len())
    };
    let (mut i, mut depth, mut deepest) = (0, 0usize, 0);
    while i < text.len() {
        if text[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        if rest.starts_with(b"<!--") {
            i = skip_to(i, b"-->");
        } else if rest.starts_with(b"<![CDATA[") {
            i = skip_to(i, b"]]>");
        } else if rest.starts_with(b"<?") {
            i = skip_to(i, b"?>");
        } else if rest.starts_with(b"<!") {
            i = skip_to(i, b">");
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            i = skip_to(i, b">");
        } else {
            let mut quote = None;
            let mut j = i + 1;
            while j < text.len() {
                match (quote, text[j]) {
                    (None, q @ (b'"' | b'\'')) => quote = Some(q),
                    (Some(q), c) if c == q => quote = None,
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            if text.get(j.wrapping_sub(1)) != Some(&b'/') {
                depth += 1;
                deepest = deepest.max(depth);
            }
            i = j + 1;
        }
    }
    deepest
}

fn screen_tree(group: &usvg::Group, reach: f32) -> Result<(), Error> {
    let nested = group
        .clip_path()
        .map(|c| c.root())
        .into_iter()
        .chain(group.mask().map(|m| m.root()));
    for g in nested {
        screen_tree(g, reach)?;
    }
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => screen_tree(g, reach)?,
            usvg::Node::Path(p) => {
                if p.stroke().is_some_and(|s| s.dasharray().is_some()) {
                    return Err(Error::Refused("a dashed stroke".into()));
                }
                let b = p.abs_stroke_bounding_box();
                if [b.left(), b.top(), b.right(), b.bottom()]
                    .iter()
                    .any(|v| !v.is_finite() || v.abs() > reach)
                {
                    return Err(Error::Refused(
                        "a shape reaches far outside the logo's box".into(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn drop_specks(alpha: &mut [u8], w: usize, h: usize, keep: f64) {
    let mut seen = vec![false; w * h];
    let mut shapes: Vec<Vec<usize>> = Vec::new();
    for start in 0..w * h {
        if alpha[start] == 0 || seen[start] {
            continue;
        }
        let mut pts = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(i) = stack.pop() {
            pts.push(i);
            let (x, y) = (i % w, i / w);
            let mut visit = |n: usize| {
                if alpha[n] != 0 && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            };
            if x + 1 < w {
                visit(i + 1);
            }
            if x > 0 {
                visit(i - 1);
            }
            if y + 1 < h {
                visit(i + w);
            }
            if y > 0 {
                visit(i - w);
            }
        }
        shapes.push(pts);
    }
    let biggest = shapes.iter().map(Vec::len).max().unwrap_or(0) as f64;
    for shape in shapes.iter().filter(|s| (s.len() as f64) < keep * biggest) {
        for &i in shape {
            alpha[i] = 0;
        }
    }
}
