//! The content-stream walker. It runs a page's content and the Form XObjects it draws with the
//! graphics state of ISO 32000-1 §8.4 (colours, alphas, soft mask, blend mode, the clipping
//! path's bounding box, text state and render mode), marked content and optional content, and
//! tells the output device about every glyph shown and every area painted, in drawing order.
//!
//! It never decompresses a stream (`objects::get_contents` panics on a filtered one), never
//! recurses past `Options::max_form_depth`, never runs a Form that is already on its own path,
//! and asks the device before every operator, so the device can stop the walk.
use std::rc::Rc;

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::colour::{BlendMode, Colour, ColourCache, ColourSpace};
use crate::fonts::{Font, FontCache, FontFacts};
use crate::objects::{self, get_contents};
use crate::optional::{Optional, OptionalConfig};
use crate::{MediaBox, OutputDev, OutputError, Path, PathOp, Transform};

/// Saved graphics states (`q`) kept at once; deeper `q`s are counted and skipped with their `Q`s.
const MAX_STATE_DEPTH: usize = 256;
/// `/Parent` links followed for an inherited page attribute.
const MAX_INHERITANCE: usize = 64;
/// Tolerance, in points, of the axis-aligned rectangle test.
const RECT_TOLERANCE: f64 = 0.01;
/// A rectangle that holds no point: the clip of a path with no extent or a malformed /BBox.
const EMPTY_RECT: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: -1.0,
    y1: -1.0,
};
/// The page size used when a page names no media box (US Letter).
const DEFAULT_MEDIA_BOX: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: 612.0,
    y1: 792.0,
};

/// An axis-aligned rectangle in page space (default user space, y up). Empty when `x0 > x1` or
/// `y0 > y1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    /// The bounding box of `points`; `None` when there are none or one is not finite.
    pub fn bounding(points: impl IntoIterator<Item = (f64, f64)>) -> Option<Rect> {
        let mut rect: Option<Rect> = None;
        for (x, y) in points {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            rect = Some(match rect {
                None => Rect {
                    x0: x,
                    y0: y,
                    x1: x,
                    y1: y,
                },
                Some(rect) => Rect {
                    x0: rect.x0.min(x),
                    y0: rect.y0.min(y),
                    x1: rect.x1.max(x),
                    y1: rect.y1.max(y),
                },
            });
        }
        rect
    }

    /// `[llx lly urx ury]` in either corner order.
    pub fn from_array(values: &[f64]) -> Option<Rect> {
        match values {
            [a, b, c, d] => Rect::bounding([(*a, *b), (*c, *d)]),
            _ => None,
        }
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    pub fn is_empty(&self) -> bool {
        !(self.x0 <= self.x1 && self.y0 <= self.y1)
    }

    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        self.x0 <= x && x <= self.x1 && self.y0 <= y && y <= self.y1
    }

    pub fn contains(&self, other: &Rect) -> bool {
        self.x0 <= other.x0 && other.x1 <= self.x1 && self.y0 <= other.y0 && other.y1 <= self.y1
    }

    pub fn centre(&self) -> (f64, f64) {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }
}

/// The clipping path in force, as its bounding box in page space (`None`: the whole page, no
/// clip), and whether every path that built it was an axis-aligned rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub bbox: Option<Rect>,
    pub rect: bool,
}

impl Clip {
    pub const NONE: Clip = Clip {
        bbox: None,
        rect: true,
    };

    fn and(self, bbox: Rect, rect: bool) -> Clip {
        Clip {
            bbox: Some(match self.bbox {
                Some(clip) => clip.intersect(&bbox),
                None => bbox,
            }),
            rect: self.rect && rect,
        }
    }

    fn cut(&self, bbox: Rect) -> Rect {
        match self.bbox {
            Some(clip) => clip.intersect(&bbox),
            None => bbox,
        }
    }
}

/// An enclosing transparency group: a Form XObject with `/Group /S /Transparency`, with the
/// state it was painted with. `id` is unique within one walk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Group {
    pub id: u32,
    /// The nonstroking alpha (`ca`) in force at its `Do`.
    pub alpha: f64,
    pub soft_mask: bool,
    pub blend: BlendMode,
    pub knockout: bool,
    pub isolated: bool,
}

/// One shown glyph and its paint state. Alphas, the soft mask and the blend mode are folded
/// through the enclosing transparency groups (`groups`): the alphas multiply, and a soft mask
/// or a blend mode other than Normal at any level is reported as the glyph's own.
pub struct Glyph<'a> {
    /// The code's text: from `/ToUnicode`, else (simple fonts) the encoding. May be empty.
    pub text: &'a str,
    pub code: u32,
    /// The font's `/ToUnicode` map holds the code.
    pub mapped: bool,
    /// `w0`: the glyph's displacement in text space per unit of font size.
    pub width: f64,
    /// `Tc`, plus `Tw` for a single-byte code 32.
    pub spacing: f64,
    pub font_size: f64,
    /// `Tsm × Tm × CTM`, with `Tsm = [Tz/100 0 0 1 0 Ts]` (the font size is not in it).
    pub trm: Transform,
    pub tm: Transform,
    pub ctm: Transform,
    /// `Tz / 100`.
    pub horizontal_scaling: f64,
    pub render_mode: u8,
    pub fill: &'a Colour,
    pub stroke: &'a Colour,
    pub fill_alpha: f64,
    pub stroke_alpha: f64,
    pub soft_mask: bool,
    pub blend: BlendMode,
    pub clip: &'a Clip,
    pub optional: Optional,
    pub groups: &'a [Group],
    pub font: &'a FontFacts,
    /// The outermost marked-content section around the glyph with an `/ActualText`: an id unique
    /// within the walk, and its text.
    pub actual_text: Option<(u32, &'a str)>,
}

impl Glyph<'_> {
    /// The glyph's box in page space: in text space it spans the advance (`w0 × Tfs`) by −0.2
    /// to 0.8 of the font size, taken through `trm` and bounded axis-aligned. Fixed proportions,
    /// never the font's own metrics, so a hostile descriptor cannot move it.
    pub fn bbox(&self) -> Option<Rect> {
        let advance = self.width * self.font_size;
        let (low, high) = (-0.2 * self.font_size, 0.8 * self.font_size);
        Rect::bounding(
            [(0.0, low), (advance, low), (advance, high), (0.0, high)]
                .iter()
                .map(|&(x, y)| apply(&self.trm, x, y)),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaKind {
    /// A filled path (`f`, `F`, `f*`, `B`, `B*`, `b`, `b*`).
    Fill { even_odd: bool },
    /// An image XObject; `masked` when it has `/SMask`, `/Mask` or `/ImageMask true`.
    Image { masked: bool },
    /// A `BI` inline image; `parsed` is false when lopdf gave the operation no image (a filter
    /// or colour space it does not read), so its data was skipped to the next `EI`.
    InlineImage { masked: bool, parsed: bool },
    /// `sh`.
    Shading,
}

/// One painted area. Areas that optional content hides are never reported.
pub struct Area<'a> {
    pub kind: AreaKind,
    /// The painted shape's bounding box in page space, cut to the clipping path's.
    pub bbox: Rect,
    /// The shape is one axis-aligned rectangle in page space (a single `re`, or four straight
    /// segments alternating horizontal and vertical), under a clip built from such rectangles.
    pub rect: bool,
    /// The fill colour; `None` when it is not known (images, shadings, and areas inside an
    /// optional-content expression this crate does not evaluate).
    pub fill: Option<&'a Colour>,
    pub alpha: f64,
    pub soft_mask: bool,
    pub blend: BlendMode,
    pub groups: &'a [Group],
}

/// Limits of one walk.
#[derive(Clone, Debug)]
pub struct Options {
    /// Form XObjects nested deeper than this end the walk (`OutputError::Limit("xobject_depth")`).
    pub max_form_depth: u32,
    /// Distinct fonts one walk may load (`Limit("fonts")`).
    pub max_fonts: usize,
    /// Entries of one page's `/Contents` array (`Limit("contents")`).
    pub max_contents: usize,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_form_depth: 8,
            max_fonts: 4096,
            max_contents: 1024,
        }
    }
}

fn apply(m: &Transform, x: f64, y: f64) -> (f64, f64) {
    (x * m.m11 + y * m.m21 + m.m31, x * m.m12 + y * m.m22 + m.m32)
}

fn matrix(values: &[f64]) -> Option<Transform> {
    match values {
        [a, b, c, d, e, f] => Some(Transform::row_major(*a, *b, *c, *d, *e, *f)),
        _ => None,
    }
}

#[derive(Clone)]
struct State {
    ctm: Transform,
    fill: Colour,
    stroke: Colour,
    fill_alpha: f64,
    stroke_alpha: f64,
    soft_mask: bool,
    blend: BlendMode,
    clip: Clip,
    font: Option<Rc<Font>>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    horizontal_scaling: f64,
    leading: f64,
    rise: f64,
    render_mode: u8,
    line_width: f64,
}

impl State {
    fn initial() -> State {
        State {
            ctm: Transform::identity(),
            fill: Colour::black(),
            stroke: Colour::black(),
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            soft_mask: false,
            blend: BlendMode::Normal,
            clip: Clip::NONE,
            font: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
            line_width: 1.0,
        }
    }
}

/// One open marked-content section (or a Form's own `/OC`), with the state of the whole stack
/// at that depth.
struct Marked {
    optional: Optional,
    actual: Option<(u32, Rc<str>)>,
}

/// The fold of the enclosing transparency groups.
#[derive(Clone, Copy)]
struct Fold {
    alpha: f64,
    soft_mask: bool,
    blend: BlendMode,
}

const NO_FOLD: Fold = Fold {
    alpha: 1.0,
    soft_mask: false,
    blend: BlendMode::Normal,
};

pub(crate) struct Walker<'a, 'o> {
    doc: &'a Document,
    output: &'o mut dyn OutputDev,
    fonts: FontCache,
    colours: ColourCache,
    optional: OptionalConfig,
    options: Options,
    media_box: Rect,
    groups: Vec<Group>,
    folds: Vec<Fold>,
    next_group: u32,
    marked: Vec<Marked>,
    next_actual: u32,
    forms: Vec<ObjectId>,
}

/// What a subpath segment is, for the rectangle test.
enum Segment {
    Line((f64, f64), (f64, f64)),
    Curve,
}

/// The page-space bounding box of `path` under `ctm`, and whether it is one axis-aligned
/// rectangle.
fn shape(path: &Path, ctm: &Transform) -> (Option<Rect>, bool) {
    let mut points = Vec::new();
    let mut subpaths: Vec<Vec<Segment>> = Vec::new();
    let mut start: Option<(f64, f64)> = None;
    let mut current: Option<(f64, f64)> = None;
    let close = |subpaths: &mut Vec<Vec<Segment>>,
                 start: Option<(f64, f64)>,
                 current: Option<(f64, f64)>| {
        if let (Some(start), Some(current), Some(last)) = (start, current, subpaths.last_mut()) {
            last.push(Segment::Line(current, start));
        }
    };
    for op in &path.ops {
        match *op {
            PathOp::MoveTo(x, y) => {
                let point = apply(ctm, x, y);
                points.push(point);
                subpaths.push(Vec::new());
                start = Some(point);
                current = Some(point);
            }
            PathOp::LineTo(x, y) => {
                let point = apply(ctm, x, y);
                points.push(point);
                if let (Some(from), Some(last)) = (current, subpaths.last_mut()) {
                    last.push(Segment::Line(from, point));
                }
                current = Some(point);
            }
            PathOp::CurveTo(x1, y1, x2, y2, x3, y3) => {
                points.push(apply(ctm, x1, y1));
                points.push(apply(ctm, x2, y2));
                let point = apply(ctm, x3, y3);
                points.push(point);
                if let Some(last) = subpaths.last_mut() {
                    last.push(Segment::Curve);
                }
                current = Some(point);
            }
            PathOp::Rect(x, y, w, h) => {
                let corners =
                    [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|(x, y)| apply(ctm, x, y));
                points.extend(corners);
                subpaths.push(
                    (0..4)
                        .map(|index| Segment::Line(corners[index], corners[(index + 1) % 4]))
                        .collect(),
                );
                start = Some(corners[0]);
                current = Some(corners[0]);
            }
            PathOp::Close => {
                close(&mut subpaths, start, current);
                current = start;
            }
        }
    }
    // A fill closes every open subpath; closing an already closed one adds a zero-length line.
    close(&mut subpaths, start, current);
    let bbox = Rect::bounding(points);
    let rect = match subpaths.as_slice() {
        [segments] => is_rectangle(segments),
        _ => false,
    };
    (bbox, rect)
}

/// Four straight segments that alternate horizontal and vertical (zero-length ones ignored).
fn is_rectangle(segments: &[Segment]) -> bool {
    let mut kinds = Vec::new();
    for segment in segments {
        match segment {
            Segment::Curve => return false,
            Segment::Line(from, to) => {
                let (dx, dy) = ((to.0 - from.0).abs(), (to.1 - from.1).abs());
                if dx <= RECT_TOLERANCE && dy <= RECT_TOLERANCE {
                    continue;
                }
                if dy <= RECT_TOLERANCE {
                    kinds.push(true);
                } else if dx <= RECT_TOLERANCE {
                    kinds.push(false);
                } else {
                    return false;
                }
            }
        }
    }
    kinds.len() == 4 && (0..4).all(|index| kinds[index] != kinds[(index + 1) % 4])
}

/// The last `count` operands as numbers, or `None` when there are fewer or one is not a number.
fn numbers(operation: &Operation, count: usize) -> Option<Vec<f64>> {
    let operands = &operation.operands;
    let first = operands.len().checked_sub(count)?;
    operands[first..].iter().map(objects::number).collect()
}

fn last_name(operation: &Operation) -> Option<&[u8]> {
    match operation.operands.last()? {
        Object::Name(name) => Some(name),
        _ => None,
    }
}

/// Text of an `/ActualText` string: UTF-16BE with a byte order mark, else PDFDocEncoding.
fn text_string(bytes: &[u8]) -> String {
    match bytes {
        [0xFE, 0xFF, rest @ ..] => {
            let units: Vec<u16> = rest
                .chunks(2)
                .map(|pair| match pair {
                    [high, low] => u16::from_be_bytes([*high, *low]),
                    _ => 0xFFFD,
                })
                .collect();
            crate::cmap::utf16_lossy(&units)
        }
        _ => bytes
            .iter()
            .map(|byte| {
                char::from_u32(u32::from(crate::PDF_DOC_ENCODING[usize::from(*byte)]))
                    .unwrap_or('\u{FFFD}')
            })
            .collect(),
    }
}

/// A dictionary attribute of a page, inherited through `/Parent` (bounded, cycle-safe).
fn inherited<'a>(doc: &'a Document, page: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    let mut dict = page;
    let mut seen = Vec::new();
    for _ in 0..MAX_INHERITANCE {
        if let Some(value) = objects::get(doc, dict, key) {
            return Some(value);
        }
        let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
        if seen.contains(&parent) {
            return None;
        }
        seen.push(parent);
        dict = doc.get_dictionary(parent).ok()?;
    }
    None
}

/// A page's media box, inherited and normalised; `None` when it names none.
pub fn media_box(doc: &Document, page: &Dictionary) -> Option<Rect> {
    let Object::Array(items) = inherited(doc, page, b"MediaBox")? else {
        return None;
    };
    Rect::from_array(&objects::numbers(doc, items)?)
}

impl<'a, 'o> Walker<'a, 'o> {
    pub fn new(
        doc: &'a Document,
        output: &'o mut dyn OutputDev,
        options: &Options,
    ) -> Walker<'a, 'o> {
        Walker {
            doc,
            output,
            fonts: FontCache::new(options.max_fonts),
            colours: ColourCache::default(),
            optional: OptionalConfig::from_document(doc),
            options: options.clone(),
            media_box: DEFAULT_MEDIA_BOX,
            groups: Vec::new(),
            folds: Vec::new(),
            next_group: 0,
            marked: Vec::new(),
            next_actual: 0,
            forms: Vec::new(),
        }
    }

    pub fn page(&mut self, page_num: u32, id: ObjectId) -> Result<(), OutputError> {
        let doc = self.doc;
        let page = doc.get_dictionary(id).ok();
        self.media_box = page
            .and_then(|page| media_box(doc, page))
            .unwrap_or(DEFAULT_MEDIA_BOX);
        let art_box = page
            .and_then(|page| objects::get_numbers(doc, page, b"ArtBox"))
            .and_then(|values| Rect::from_array(&values))
            .map(|rect| (rect.x0, rect.y0, rect.x1, rect.y1));
        let media = MediaBox {
            llx: self.media_box.x0,
            lly: self.media_box.y0,
            urx: self.media_box.x1,
            ury: self.media_box.y1,
        };
        self.output.begin_page(page_num, &media, art_box)?;
        if let Some(page) = page {
            let resources = inherited(doc, page, b"Resources").and_then(objects::as_dict);
            let content = self.page_content(page)?;
            self.marked.clear();
            self.run(&content, resources, State::initial(), 0)?;
        }
        self.output.end_page()
    }

    /// The page's content streams joined with a line break between them.
    fn page_content(&self, page: &Dictionary) -> Result<Vec<u8>, OutputError> {
        let doc = self.doc;
        let mut streams: Vec<&Stream> = Vec::new();
        match objects::get(doc, page, b"Contents") {
            Some(Object::Stream(stream)) => streams.push(stream),
            Some(Object::Array(items)) => {
                if items.len() > self.options.max_contents {
                    return Err(OutputError::Limit("contents"));
                }
                for item in items {
                    if let Some(Object::Stream(stream)) = objects::resolved(doc, item) {
                        streams.push(stream);
                    }
                }
            }
            _ => {}
        }
        let mut content = Vec::new();
        for stream in streams {
            content.extend_from_slice(get_contents(stream));
            content.push(b'\n');
        }
        Ok(content)
    }

    fn fold(&self) -> Fold {
        self.folds.last().copied().unwrap_or(NO_FOLD)
    }

    fn visibility(&self) -> Optional {
        self.marked
            .last()
            .map_or(Optional::Visible, |marked| marked.optional)
    }

    fn push_marked(&mut self, optional: Optional, actual: Option<Rc<str>>) {
        let parent = self.marked.last();
        let optional = parent.map_or(optional, |parent| parent.optional.and(optional));
        // The outermost /ActualText applies to everything inside it.
        let actual = match parent.and_then(|parent| parent.actual.clone()) {
            Some(outer) => Some(outer),
            None => actual.map(|text| {
                self.next_actual = self.next_actual.wrapping_add(1);
                (self.next_actual, text)
            }),
        };
        self.marked.push(Marked { optional, actual });
    }

    fn area(
        &mut self,
        state: &State,
        kind: AreaKind,
        bbox: Option<Rect>,
        rect: bool,
        fill: Option<&Colour>,
    ) -> Result<(), OutputError> {
        let visibility = self.visibility();
        if visibility == Optional::Hidden {
            return Ok(());
        }
        let Some(bbox) = bbox
            .map(|bbox| state.clip.cut(bbox))
            .filter(|bbox| !bbox.is_empty())
        else {
            return Ok(());
        };
        let fold = self.fold();
        let area = Area {
            kind,
            bbox,
            rect: rect && state.clip.rect,
            fill: if visibility == Optional::Unknown {
                None
            } else {
                fill
            },
            alpha: state.fill_alpha * fold.alpha,
            soft_mask: state.soft_mask || fold.soft_mask,
            blend: if state.blend.is_normal() {
                fold.blend
            } else {
                state.blend
            },
            groups: &self.groups,
        };
        self.output.area(&area)
    }

    /// Shows `bytes` in the current font, moving the text matrix after each glyph.
    fn show(
        &mut self,
        state: &State,
        tm: &mut Transform,
        bytes: &[u8],
        text_clip: &mut Option<Rect>,
    ) -> Result<(), OutputError> {
        let Some(font) = state.font.clone() else {
            return Ok(());
        };
        self.output.begin_word()?;
        let fold = self.fold();
        let mut at = 0;
        while let Some((code, len)) = font.next_code(bytes, at) {
            if len == 0 {
                break;
            }
            at += len;
            self.output.work(1)?;
            let (text, mapped) = font.decode(code);
            let width = font.width(code);
            let mut spacing = state.char_spacing;
            if code == 32 && len == 1 {
                spacing += state.word_spacing;
            }
            let tsm =
                Transform::row_major(state.horizontal_scaling, 0.0, 0.0, 1.0, 0.0, state.rise);
            let trm = tsm.post_transform(&tm.post_transform(&state.ctm));
            let actual = self.marked.last().and_then(|marked| marked.actual.clone());
            let glyph = Glyph {
                text: &text,
                code,
                mapped,
                width,
                spacing,
                font_size: state.font_size,
                trm,
                tm: *tm,
                ctm: state.ctm,
                horizontal_scaling: state.horizontal_scaling,
                render_mode: state.render_mode,
                fill: &state.fill,
                stroke: &state.stroke,
                fill_alpha: state.fill_alpha * fold.alpha,
                stroke_alpha: state.stroke_alpha * fold.alpha,
                soft_mask: state.soft_mask || fold.soft_mask,
                blend: if state.blend.is_normal() {
                    fold.blend
                } else {
                    state.blend
                },
                clip: &state.clip,
                optional: self.visibility(),
                groups: &self.groups,
                font: &font.facts,
                actual_text: actual.as_ref().map(|(id, text)| (*id, &**text)),
            };
            self.output.glyph(&glyph)?;
            // Render modes 4 to 7 add the glyph outlines to the clip; a space has none.
            let outline = text.is_empty() || !text.chars().all(char::is_whitespace);
            if state.render_mode >= 4 && outline {
                if let Some(bbox) = glyph.bbox() {
                    *text_clip = Some(match text_clip {
                        Some(union) => Rect::bounding([
                            (union.x0, union.y0),
                            (union.x1, union.y1),
                            (bbox.x0, bbox.y0),
                            (bbox.x1, bbox.y1),
                        ])
                        .unwrap_or(bbox),
                        None => bbox,
                    });
                }
            }
            let advance = (width * state.font_size + spacing) * state.horizontal_scaling;
            *tm = tm.pre_transform(&Transform::create_translation(advance, 0.0));
        }
        self.output.end_word()
    }

    fn set_colour(&mut self, colour: &mut Colour, operation: &Operation) {
        let space = colour.space.clone();
        if matches!(*space, ColourSpace::Pattern) {
            colour.components = operation
                .operands
                .iter()
                .filter_map(objects::number)
                .collect();
            return;
        }
        if let Some(values) = numbers(operation, space.components()) {
            if operation.operands.len() == values.len() {
                colour.components = values;
            }
        }
    }

    fn apply_ext_gstate(&mut self, state: &mut State, resources: Option<&Dictionary>, name: &[u8]) {
        let doc = self.doc;
        let Some(dict) = resources
            .and_then(|resources| objects::get_dict(doc, resources, b"ExtGState"))
            .and_then(|states| objects::get(doc, states, name))
            .and_then(objects::as_dict)
        else {
            return;
        };
        if let Some(alpha) = objects::get_number(doc, dict, b"ca") {
            state.fill_alpha = alpha.clamp(0.0, 1.0);
        }
        if let Some(alpha) = objects::get_number(doc, dict, b"CA") {
            state.stroke_alpha = alpha.clamp(0.0, 1.0);
        }
        if let Some(blend) = dict
            .get(b"BM")
            .ok()
            .and_then(|blend| BlendMode::from_object(doc, blend))
        {
            state.blend = blend;
        }
        match objects::get(doc, dict, b"SMask") {
            Some(Object::Name(name)) => state.soft_mask = name != b"None",
            Some(Object::Dictionary(_)) | Some(Object::Stream(_)) => state.soft_mask = true,
            _ => {}
        }
        if let Some(width) = objects::get_number(doc, dict, b"LW") {
            state.line_width = width;
        }
    }

    /// Runs one content stream from `state`. Marked content the stream leaves open ends with it.
    fn run(
        &mut self,
        content: &[u8],
        resources: Option<&'a Dictionary>,
        mut state: State,
        depth: u32,
    ) -> Result<(), OutputError> {
        let doc = self.doc;
        let marked_base = self.marked.len();
        let mut saved: Vec<State> = Vec::new();
        let mut skipped_saves = 0usize;
        let mut path = Path { ops: Vec::new() };
        let mut has_point = false;
        let mut pending_clip: Option<bool> = None;
        let mut tm = Transform::identity();
        let mut tlm = Transform::identity();
        let mut text_clip: Option<Rect> = None;
        for operation in Content::decode_iter(content) {
            let Ok(operation) = operation else { break };
            self.output.operator()?;
            let op = operation.operator.as_str();
            match op {
                "q" => {
                    if saved.len() < MAX_STATE_DEPTH {
                        saved.push(state.clone());
                    } else {
                        skipped_saves += 1;
                    }
                }
                "Q" => {
                    if skipped_saves > 0 {
                        skipped_saves -= 1;
                    } else if let Some(previous) = saved.pop() {
                        state = previous;
                    }
                }
                "cm" => {
                    if let Some(m) = numbers(&operation, 6).as_deref().and_then(matrix) {
                        state.ctm = state.ctm.pre_transform(&m);
                    }
                }
                "w" => {
                    if let Some(values) = numbers(&operation, 1) {
                        state.line_width = values[0];
                    }
                }
                "gs" => {
                    if let Some(name) = last_name(&operation) {
                        self.apply_ext_gstate(&mut state, resources, name);
                    }
                }
                "CS" | "cs" => {
                    if let Some(name) = last_name(&operation) {
                        let space = match resources {
                            Some(resources) => self.colours.named(doc, resources, name),
                            None => self.colours.named(doc, &Dictionary::new(), name),
                        };
                        let colour = Colour::in_space(space);
                        if op == "CS" {
                            state.stroke = colour;
                        } else {
                            state.fill = colour;
                        }
                    }
                }
                "SC" | "SCN" => {
                    let mut colour = state.stroke.clone();
                    self.set_colour(&mut colour, &operation);
                    state.stroke = colour;
                }
                "sc" | "scn" => {
                    let mut colour = state.fill.clone();
                    self.set_colour(&mut colour, &operation);
                    state.fill = colour;
                }
                "G" | "g" | "RG" | "rg" | "K" | "k" => {
                    let (space, count) = match op {
                        "G" | "g" => (ColourSpace::DeviceGray, 1),
                        "RG" | "rg" => (ColourSpace::DeviceRGB, 3),
                        _ => (ColourSpace::DeviceCMYK, 4),
                    };
                    if let Some(components) = numbers(&operation, count) {
                        let colour = Colour {
                            space: Rc::new(space),
                            components,
                        };
                        if op.chars().all(char::is_uppercase) {
                            state.stroke = colour;
                        } else {
                            state.fill = colour;
                        }
                    }
                }
                "BT" => {
                    tm = Transform::identity();
                    tlm = Transform::identity();
                    text_clip = None;
                }
                "ET" => {
                    if let Some(bbox) = text_clip.take() {
                        state.clip = state.clip.and(bbox, false);
                    }
                }
                "Tc" | "Tw" | "Tz" | "TL" | "Ts" => {
                    if let Some(values) = numbers(&operation, 1) {
                        let value = values[0];
                        match op {
                            "Tc" => state.char_spacing = value,
                            "Tw" => state.word_spacing = value,
                            "Tz" => state.horizontal_scaling = value / 100.0,
                            "TL" => state.leading = value,
                            _ => state.rise = value,
                        }
                    }
                }
                "Tr" => {
                    if let Some(Object::Integer(mode)) = operation.operands.last() {
                        if let Ok(mode) = u8::try_from(*mode) {
                            if mode <= 7 {
                                state.render_mode = mode;
                            }
                        }
                    }
                }
                "Tf" => {
                    let operands = &operation.operands;
                    if operands.len() >= 2 {
                        if let (Object::Name(name), Some(size)) = (
                            &operands[operands.len() - 2],
                            objects::number(&operands[operands.len() - 1]),
                        ) {
                            let resource = resources
                                .and_then(|resources| objects::get_dict(doc, resources, b"Font"))
                                .and_then(|fonts| fonts.get(name).ok());
                            state.font = match resource {
                                Some(resource) => self
                                    .fonts
                                    .font(doc, resource)
                                    .map_err(|()| OutputError::Limit("fonts"))?,
                                None => None,
                            };
                            state.font_size = size;
                        }
                    }
                }
                "Td" | "TD" => {
                    if let Some(values) = numbers(&operation, 2) {
                        if op == "TD" {
                            state.leading = -values[1];
                        }
                        tlm =
                            tlm.pre_transform(&Transform::create_translation(values[0], values[1]));
                        tm = tlm;
                        self.output.end_line()?;
                    }
                }
                "Tm" => {
                    if let Some(m) = numbers(&operation, 6).as_deref().and_then(matrix) {
                        tlm = m;
                        tm = m;
                        self.output.end_line()?;
                    }
                }
                "T*" => {
                    tlm = tlm.pre_transform(&Transform::create_translation(0.0, -state.leading));
                    tm = tlm;
                    self.output.end_line()?;
                }
                "Tj" | "'" | "\"" => {
                    if op == "\"" {
                        let operands = &operation.operands;
                        if operands.len() >= 3 {
                            if let (Some(word), Some(character)) = (
                                objects::number(&operands[operands.len() - 3]),
                                objects::number(&operands[operands.len() - 2]),
                            ) {
                                state.word_spacing = word;
                                state.char_spacing = character;
                            }
                        }
                    }
                    if op != "Tj" {
                        tlm =
                            tlm.pre_transform(&Transform::create_translation(0.0, -state.leading));
                        tm = tlm;
                        self.output.end_line()?;
                    }
                    if let Some(Object::String(bytes, _)) = operation.operands.last() {
                        self.show(&state, &mut tm, bytes, &mut text_clip)?;
                    }
                }
                "TJ" => {
                    if let Some(Object::Array(items)) = operation.operands.last() {
                        for item in items {
                            self.output.work(1)?;
                            match item {
                                Object::String(bytes, _) => {
                                    self.show(&state, &mut tm, bytes, &mut text_clip)?
                                }
                                other => {
                                    if let Some(adjust) = objects::number(other) {
                                        let advance = -adjust / 1000.0
                                            * state.font_size
                                            * state.horizontal_scaling;
                                        tm = tm.pre_transform(&Transform::create_translation(
                                            advance, 0.0,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                "m" | "l" => {
                    if let Some(values) = numbers(&operation, 2) {
                        if op == "m" {
                            path.ops.push(PathOp::MoveTo(values[0], values[1]));
                            has_point = true;
                        } else if has_point {
                            path.ops.push(PathOp::LineTo(values[0], values[1]));
                        }
                    }
                }
                "c" => {
                    if let (Some(v), true) = (numbers(&operation, 6), has_point) {
                        path.ops
                            .push(PathOp::CurveTo(v[0], v[1], v[2], v[3], v[4], v[5]));
                    }
                }
                "v" => {
                    if let (Some(v), Some(current)) = (numbers(&operation, 4), current_point(&path))
                    {
                        path.ops.push(PathOp::CurveTo(
                            current.0, current.1, v[0], v[1], v[2], v[3],
                        ));
                    }
                }
                "y" => {
                    if let (Some(v), true) = (numbers(&operation, 4), has_point) {
                        path.ops
                            .push(PathOp::CurveTo(v[0], v[1], v[2], v[3], v[2], v[3]));
                    }
                }
                "h" => {
                    if has_point {
                        path.ops.push(PathOp::Close);
                    }
                }
                "re" => {
                    if let Some(v) = numbers(&operation, 4) {
                        path.ops.push(PathOp::Rect(v[0], v[1], v[2], v[3]));
                        has_point = true;
                    }
                }
                "W" | "W*" => pending_clip = Some(op == "W*"),
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    if matches!(op, "s" | "b" | "b*") && has_point {
                        path.ops.push(PathOp::Close);
                    }
                    let (bbox, rect) = shape(&path, &state.ctm);
                    if matches!(op, "S" | "s" | "B" | "B*" | "b" | "b*") {
                        self.output.stroke(
                            &state.ctm,
                            &state.stroke.space,
                            &state.stroke.components,
                            &path,
                        )?;
                    }
                    if !matches!(op, "S" | "s" | "n") {
                        let even_odd = matches!(op, "f*" | "B*" | "b*");
                        self.output.fill(
                            &state.ctm,
                            &state.fill.space,
                            &state.fill.components,
                            &path,
                        )?;
                        self.area(
                            &state,
                            AreaKind::Fill { even_odd },
                            bbox,
                            rect,
                            Some(&state.fill),
                        )?;
                    }
                    // W and W* take effect after the painting operator (ISO 32000-1 §8.5.4).
                    if pending_clip.take().is_some() {
                        state.clip = state.clip.and(bbox.unwrap_or(EMPTY_RECT), rect);
                    }
                    path.ops.clear();
                    has_point = false;
                }
                "sh" => {
                    let shading = last_name(&operation).and_then(|name| {
                        resources
                            .and_then(|resources| objects::get_dict(doc, resources, b"Shading"))
                            .and_then(|shadings| objects::get(doc, shadings, name))
                            .and_then(objects::as_dict)
                    });
                    if let Some(shading) = shading {
                        let bbox = objects::get_numbers(doc, shading, b"BBox")
                            .and_then(|values| Rect::from_array(&values))
                            .and_then(|own| {
                                Rect::bounding(
                                    [
                                        (own.x0, own.y0),
                                        (own.x1, own.y0),
                                        (own.x1, own.y1),
                                        (own.x0, own.y1),
                                    ]
                                    .iter()
                                    .map(|&(x, y)| apply(&state.ctm, x, y)),
                                )
                            })
                            .map_or(self.media_box, |own| own.intersect(&self.media_box));
                        self.area(&state, AreaKind::Shading, Some(bbox), false, None)?;
                    }
                }
                "BI" => {
                    let (masked, parsed) = match operation.operands.first() {
                        Some(Object::Stream(image)) => {
                            let flag = |key: &[u8]| {
                                matches!(image.dict.get(key), Ok(Object::Boolean(true)))
                            };
                            (flag(b"IM") || flag(b"ImageMask"), true)
                        }
                        _ => (false, false),
                    };
                    let bbox = unit_square(&state.ctm);
                    self.area(
                        &state,
                        AreaKind::InlineImage { masked, parsed },
                        bbox,
                        false,
                        None,
                    )?;
                }
                "Do" => {
                    if let Some(name) = last_name(&operation) {
                        self.draw_xobject(&state, resources, name, depth)?;
                    }
                }
                "BMC" => self.push_marked(Optional::Visible, None),
                "BDC" => {
                    let operands = &operation.operands;
                    let (tag, properties) = match operands.as_slice() {
                        [.., Object::Name(tag), properties] => (tag.as_slice(), Some(properties)),
                        _ => (&b""[..], None),
                    };
                    // A named property list comes from the resources' /Properties.
                    let named = match properties {
                        Some(Object::Name(name)) => resources
                            .and_then(|resources| objects::get_dict(doc, resources, b"Properties"))
                            .and_then(|list| list.get(name).ok()),
                        _ => None,
                    };
                    let optional = match (tag, named) {
                        (b"OC", Some(group)) => self.optional.evaluate(doc, group),
                        _ => Optional::Visible,
                    };
                    let dict = match properties {
                        Some(Object::Dictionary(dict)) => Some(dict),
                        _ => named
                            .and_then(|named| objects::resolved(doc, named))
                            .and_then(objects::as_dict),
                    };
                    let actual = match dict.and_then(|dict| objects::get(doc, dict, b"ActualText"))
                    {
                        Some(Object::String(bytes, _)) => Some(Rc::from(text_string(bytes))),
                        _ => None,
                    };
                    self.push_marked(optional, actual);
                }
                "EMC" => {
                    if self.marked.len() > marked_base {
                        self.marked.pop();
                    }
                }
                _ => {}
            }
        }
        self.marked.truncate(marked_base);
        Ok(())
    }

    fn draw_xobject(
        &mut self,
        state: &State,
        resources: Option<&'a Dictionary>,
        name: &[u8],
        depth: u32,
    ) -> Result<(), OutputError> {
        let doc = self.doc;
        let Some(object) = resources
            .and_then(|resources| objects::get_dict(doc, resources, b"XObject"))
            .and_then(|xobjects| xobjects.get(name).ok())
        else {
            return Ok(());
        };
        let Some((id, Object::Stream(stream))) = objects::resolve(doc, object) else {
            return Ok(());
        };
        let dict = &stream.dict;
        let optional = dict
            .get(b"OC")
            .ok()
            .map_or(Optional::Visible, |oc| self.optional.evaluate(doc, oc));
        match objects::get_name(doc, dict, b"Subtype") {
            Some(b"Image") => {
                if optional == Optional::Hidden {
                    return Ok(());
                }
                let flag = |key: &[u8]| dict.has(key);
                let masked = flag(b"SMask")
                    || flag(b"Mask")
                    || objects::get_bool(doc, dict, b"ImageMask") == Some(true);
                self.push_marked(optional, None);
                let result = self.area(
                    state,
                    AreaKind::Image { masked },
                    unit_square(&state.ctm),
                    false,
                    None,
                );
                self.marked.pop();
                result
            }
            Some(b"Form") => {
                let next = depth.saturating_add(1);
                if next > self.options.max_form_depth {
                    return Err(OutputError::Limit("xobject_depth"));
                }
                if let Some(id) = id {
                    if self.forms.contains(&id) {
                        return Err(OutputError::Malformed("xobject_cycle"));
                    }
                }
                let mut inner = state.clone();
                if let Some(m) = objects::get_numbers(doc, dict, b"Matrix")
                    .as_deref()
                    .and_then(matrix)
                {
                    inner.ctm = inner.ctm.pre_transform(&m);
                }
                // A /BBox that is not four numbers clips everything, as viewers skip such a
                // Form; with no /BBox at all the Form draws unclipped.
                let bbox = objects::get_numbers(doc, dict, b"BBox")
                    .and_then(|values| Rect::from_array(&values));
                if bbox.is_none() && dict.has(b"BBox") {
                    inner.clip = inner.clip.and(EMPTY_RECT, true);
                }
                if let Some(bbox) = bbox {
                    let corners = [
                        (bbox.x0, bbox.y0),
                        (bbox.x1, bbox.y0),
                        (bbox.x1, bbox.y1),
                        (bbox.x0, bbox.y1),
                    ];
                    let path = Path {
                        ops: vec![PathOp::Rect(
                            bbox.x0,
                            bbox.y0,
                            bbox.x1 - bbox.x0,
                            bbox.y1 - bbox.y0,
                        )],
                    };
                    let (page_bbox, rect) = shape(&path, &inner.ctm);
                    let page_bbox = page_bbox.or_else(|| {
                        Rect::bounding(corners.iter().map(|&(x, y)| apply(&inner.ctm, x, y)))
                    });
                    if let Some(page_bbox) = page_bbox {
                        inner.clip = inner.clip.and(page_bbox, rect);
                    }
                }
                let group = objects::get_dict(doc, dict, b"Group")
                    .filter(|group| objects::get_name(doc, group, b"S") == Some(b"Transparency"));
                if let Some(group) = group {
                    self.next_group = self.next_group.wrapping_add(1);
                    let entry = Group {
                        id: self.next_group,
                        alpha: state.fill_alpha,
                        soft_mask: state.soft_mask,
                        blend: state.blend,
                        knockout: objects::get_bool(doc, group, b"K") == Some(true),
                        isolated: objects::get_bool(doc, group, b"I") == Some(true),
                    };
                    let parent = self.fold();
                    self.folds.push(Fold {
                        alpha: parent.alpha * entry.alpha,
                        soft_mask: parent.soft_mask || entry.soft_mask,
                        blend: if entry.blend.is_normal() {
                            parent.blend
                        } else {
                            entry.blend
                        },
                    });
                    self.groups.push(entry);
                    // A transparency group starts its content from alpha 1, no soft mask and Normal.
                    inner.fill_alpha = 1.0;
                    inner.stroke_alpha = 1.0;
                    inner.soft_mask = false;
                    inner.blend = BlendMode::Normal;
                }
                let own_resources = objects::get(doc, dict, b"Resources")
                    .and_then(objects::as_dict)
                    .or(resources);
                if let Some(id) = id {
                    self.forms.push(id);
                }
                self.push_marked(optional, None);
                let content = get_contents(stream);
                // The Form's content is parsed again at every Do.
                self.output.work(content.len() as u64 / 1024 + 1)?;
                let result = self.run(content, own_resources, inner, next);
                self.marked.pop();
                if id.is_some() {
                    self.forms.pop();
                }
                if group.is_some() {
                    self.groups.pop();
                    self.folds.pop();
                }
                result
            }
            _ => Ok(()),
        }
    }
}

fn current_point(path: &Path) -> Option<(f64, f64)> {
    let mut start = None;
    let mut current = None;
    for op in &path.ops {
        match *op {
            PathOp::MoveTo(x, y) => {
                start = Some((x, y));
                current = start;
            }
            PathOp::LineTo(x, y) | PathOp::CurveTo(_, _, _, _, x, y) => current = Some((x, y)),
            PathOp::Rect(x, y, _, _) => {
                start = Some((x, y));
                current = start;
            }
            PathOp::Close => current = start,
        }
    }
    current
}

fn unit_square(ctm: &Transform) -> Option<Rect> {
    Rect::bounding(
        [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]
            .iter()
            .map(|&(x, y)| apply(ctm, x, y)),
    )
}

/// Walks `pages` (page objects, in reading order, numbered from 1) through `output`.
pub fn output_pages(
    doc: &Document,
    pages: &[ObjectId],
    options: &Options,
    output: &mut dyn OutputDev,
) -> Result<(), OutputError> {
    let mut walker = Walker::new(doc, output, options);
    for (index, page) in pages.iter().enumerate() {
        let number = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        walker.page(number, *page)?;
    }
    Ok(())
}
