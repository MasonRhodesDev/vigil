use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use resvg::usvg::{self, roxmltree};

use crate::Error;

pub const MAX_LOGO_BYTES: u64 = 512 * 1024;
pub const MAX_EDGE: u32 = 2048;
pub const MAX_RASTER_BYTES: u64 = 16 << 20;
const MAX_RASTER_SIDE: u32 = 8192;
const MAX_RASTER_PIXELS: u64 = MAX_EDGE as u64 * MAX_EDGE as u64;
const MAX_NODES: u32 = 10_000;
const MAX_DEPTH: usize = 64;
const MAX_REF_DEPTH: usize = 128;
const MAX_EXPANDED: usize = 20_000;
const MAX_EXPANDED_DATA: usize = 4 << 20;
const MAX_STYLE_RULES: usize = 64;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Svg,
    Raster,
}

impl Format {
    pub fn of(path: &Path) -> Format {
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        {
            Format::Svg
        } else {
            Format::Raster
        }
    }

    pub fn cap(self) -> u64 {
        match self {
            Format::Svg => MAX_LOGO_BYTES,
            Format::Raster => MAX_RASTER_BYTES,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Svg => "svg",
            Format::Raster => "raster",
        }
    }

    pub fn parse(name: &str) -> Option<Format> {
        [Format::Svg, Format::Raster]
            .into_iter()
            .find(|f| f.name() == name)
    }
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

    pub(crate) fn from_alpha(edge: u32, alpha: Vec<u8>) -> Mask {
        Mask { edge, alpha }
    }

    pub fn into_alpha(self) -> Vec<u8> {
        self.alpha
    }

    pub fn sprite(&self, rgb: [u8; 3]) -> Vec<u8> {
        self.alpha
            .iter()
            .flat_map(|&a| [rgb[0], rgb[1], rgb[2], a])
            .collect()
    }
}

// Opened without blocking, so a FIFO in place of a file is refused rather
// than waited on.
pub fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .map_err(|e| Error::Io(e.to_string()))?;
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

pub fn read_logo(path: &Path) -> Result<(Vec<u8>, Format), Error> {
    let format = Format::of(path);
    Ok((read_bounded(path, format.cap())?, format))
}

impl Logo {
    pub fn load(path: &Path) -> Result<Logo, Error> {
        let (data, format) = read_logo(path)?;
        Logo::parse(&data, format)
    }

    pub fn parse(data: &[u8], format: Format) -> Result<Logo, Error> {
        match format {
            Format::Svg => Logo::svg(data),
            Format::Raster => Logo::raster(data),
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
        crate::cost::check(&tree, fit(&tree, MAX_EDGE), MAX_EDGE)?;
        Ok(Logo::Svg(Box::new(tree)))
    }

    pub fn raster(data: &[u8]) -> Result<Logo, Error> {
        let reader = || {
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_RASTER_SIDE);
            limits.max_image_height = Some(MAX_RASTER_SIDE);
            limits.max_alloc = Some(64 << 20);
            let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
                .with_guessed_format()
                .map_err(|e| Error::Invalid(e.to_string()))?;
            reader.limits(limits);
            Ok::<_, Error>(reader)
        };
        let (w, h) = reader()?
            .into_dimensions()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if u64::from(w) * u64::from(h) > MAX_RASTER_PIXELS {
            return Err(Error::TooLarge(format!(
                "a {w}x{h} raster is over {MAX_RASTER_PIXELS} pixels"
            )));
        }
        let img = reader()?
            .decode()
            .map_err(|e| Error::Invalid(e.to_string()))?;
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
                let mut pixmap = resvg::tiny_skia::Pixmap::new(edge, edge)
                    .ok_or_else(|| Error::Invalid("empty logo box".into()))?;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    resvg::render(tree, fit(tree, edge), &mut pixmap.as_mut());
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
        if name.name() == "style" {
            let sheet: String = n
                .descendants()
                .filter(roxmltree::Node::is_text)
                .filter_map(|t| t.text())
                .collect();
            simple_sheet(&sheet)?;
        }
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
            total = total.plus(expansion.size(target, 0)?.times(elements));
            total.check()?;
        }
    }
    Ok(())
}

// usvg matches every rule against every element, and its matcher backtracks
// through combinators without bound, so a sheet may only hold rules on one
// type, class or id. Each `}` ends a rule here, which can only split a sheet
// into more selectors to check than the CSS parser sees.
fn simple_sheet(sheet: &str) -> Result<(), Error> {
    let refuse = |why: String| {
        Err(Error::Refused(format!(
            "a style sheet {why}; a logo's rules each name one type, class or id"
        )))
    };
    let mut text = String::new();
    let mut rest = sheet;
    while let Some(at) = rest.find("/*") {
        text.push_str(&rest[..at]);
        rest = rest[at + 2..]
            .split_once("*/")
            .map_or("", |(_, after)| after);
    }
    text.push_str(rest);
    if text.contains('@') {
        return refuse("has an at-rule".into());
    }
    let mut rules = 0;
    for rule in text.split('}') {
        let Some((selectors, body)) = rule.split_once('{') else {
            if rule.trim().is_empty() {
                continue;
            }
            return refuse(format!("has text outside a rule: {:?}", rule.trim()));
        };
        if body.contains('{') {
            return refuse("nests a block".into());
        }
        rules += 1;
        if rules > MAX_STYLE_RULES {
            return refuse(format!("has more than {MAX_STYLE_RULES} rules"));
        }
        for selector in selectors.split(',').map(str::trim) {
            let name = selector.strip_prefix(['.', '#']).unwrap_or(selector);
            let ident = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !(ident || selector == "*") {
                return refuse(format!("uses the selector {selector:?}"));
            }
        }
    }
    Ok(())
}

// What a reference costs once usvg has copied it in: elements, and the path
// data and point lists it parses again for every copy.
#[derive(Debug, Clone, Copy)]
struct Weight {
    elements: usize,
    data: usize,
}

impl Weight {
    fn plus(self, o: Weight) -> Weight {
        Weight {
            elements: self.elements.saturating_add(o.elements),
            data: self.data.saturating_add(o.data),
        }
    }

    fn times(self, n: usize) -> Weight {
        Weight {
            elements: self.elements.saturating_mul(n),
            data: self.data.saturating_mul(n),
        }
    }

    fn check(self) -> Result<(), Error> {
        if self.elements > MAX_EXPANDED {
            return Err(Error::Refused(format!(
                "the SVG expands to more than {MAX_EXPANDED} elements"
            )));
        }
        if self.data > MAX_EXPANDED_DATA {
            return Err(Error::Refused(format!(
                "the SVG expands to more than {} MiB of path data",
                MAX_EXPANDED_DATA >> 20
            )));
        }
        Ok(())
    }
}

struct Expansion<'a, 'input> {
    ids: HashMap<&'a str, roxmltree::Node<'a, 'input>>,
    sizes: HashMap<roxmltree::NodeId, Weight>,
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

    fn size(&mut self, node: roxmltree::Node<'a, 'input>, depth: usize) -> Result<Weight, Error> {
        if let Some(&n) = self.sizes.get(&node.id()) {
            return Ok(n);
        }
        if depth > MAX_REF_DEPTH || !self.open.insert(node.id()) {
            return Err(Error::Refused(
                "the SVG references form a loop or a chain too deep to follow".into(),
            ));
        }
        let data = node
            .attributes()
            .filter(|a| a.namespace().is_none() && matches!(a.name(), "d" | "points"))
            .map(|a| a.value().len())
            .sum();
        let mut total = Weight { elements: 1, data };
        let targets: Vec<_> = node
            .attributes()
            .flat_map(|a| self.targets(a.value()))
            .collect();
        for next in node
            .children()
            .filter(roxmltree::Node::is_element)
            .chain(targets)
        {
            total = total.plus(self.size(next, depth + 1)?);
            total.check()?;
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

fn fit(tree: &usvg::Tree, edge: u32) -> usvg::Transform {
    let size = tree.size();
    let s = edge as f32 / size.width().max(size.height());
    let ox = (edge as f32 - size.width() * s) / 2.0;
    let oy = (edge as f32 - size.height() * s) / 2.0;
    usvg::Transform::from_row(s, 0.0, 0.0, s, ox, oy)
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
