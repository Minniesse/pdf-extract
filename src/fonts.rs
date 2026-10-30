//! Fonts as the text walker needs them: how a string splits into codes, each code's width and
//! the text it maps to. A font whose data is missing or malformed still loads, with whatever
//! mappings could be read; nothing here panics or decodes a stream.
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::cmap::{self, CMap};
use crate::objects::{self, get_contents, Key};
use crate::type1::{self, BuiltinEncoding};
use crate::{core_fonts, encodings, glyphnames, zapfglyphnames, PDF_DOC_ENCODING};

/// What a caller may want to know about the font a glyph was shown in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFacts {
    /// The font dictionary's object id, when it is an indirect object.
    pub id: Option<ObjectId>,
    /// A Type 0 (composite) font.
    pub composite: bool,
    pub type3: bool,
    /// The font has a `/ToUnicode` map that holds at least one mapping.
    pub has_to_unicode: bool,
}

pub struct Font {
    pub facts: FontFacts,
    kind: Kind,
}

enum Kind {
    Simple(Simple),
    Composite(Composite),
}

struct Simple {
    /// The UTF-16 unit each code maps to by encoding; 0 when the encoding defines none.
    encoding: [u16; 256],
    to_unicode: Option<Rc<CMap>>,
    widths: [Option<f64>; 256],
    missing_width: f64,
    /// Glyph-space units to text space: 1/1000, or a Type 3 font's `/FontMatrix` scale.
    scale: f64,
}

struct Composite {
    /// `None` for Identity-H and Identity-V (and for predefined CMaps this crate does not carry,
    /// which are read as two-byte codes).
    encoding: Option<Rc<CMap>>,
    to_unicode: Option<Rc<CMap>>,
    widths: CidWidths,
    default_width: f64,
}

#[derive(Default)]
struct CidWidths {
    single: BTreeMap<u32, f64>,
    /// `(first, last, width)`, sorted by `first`.
    ranges: Vec<(u32, u32, f64)>,
}

impl CidWidths {
    fn get(&self, cid: u32) -> Option<f64> {
        if let Some(width) = self.single.get(&cid) {
            return Some(*width);
        }
        let index = self.ranges.partition_point(|range| range.0 <= cid);
        self.ranges[..index]
            .iter()
            .rev()
            .find(|range| cid <= range.1)
            .map(|range| range.2)
    }
}

impl Font {
    /// The next code in a shown string and its length in bytes.
    pub fn next_code(&self, bytes: &[u8], at: usize) -> Option<(u32, usize)> {
        match &self.kind {
            Kind::Simple(_) => bytes.get(at).map(|byte| (u32::from(*byte), 1)),
            Kind::Composite(font) => match &font.encoding {
                Some(cmap) => cmap.next_code(bytes, at),
                None => {
                    let rest = bytes.get(at..).filter(|rest| !rest.is_empty())?;
                    match rest {
                        [high, low, ..] => Some((u32::from(*high) << 8 | u32::from(*low), 2)),
                        [single] => Some((u32::from(*single), 1)),
                        [] => None,
                    }
                }
            },
        }
    }

    /// The code's horizontal displacement in text space per unit of font size (`w0`).
    pub fn width(&self, code: u32) -> f64 {
        match &self.kind {
            Kind::Simple(font) => {
                let width = usize::try_from(code)
                    .ok()
                    .and_then(|code| font.widths.get(code).copied().flatten())
                    .unwrap_or(font.missing_width);
                width * font.scale
            }
            Kind::Composite(font) => {
                let cid = match &font.encoding {
                    Some(cmap) => cmap.cid(code).unwrap_or(0),
                    None => code,
                };
                font.widths.get(cid).unwrap_or(font.default_width) / 1000.0
            }
        }
    }

    /// The code's text, and whether a `/ToUnicode` map holds the code. A composite font gives
    /// "" for a code its map lacks; a simple font falls back to its encoding.
    pub fn decode(&self, code: u32) -> (String, bool) {
        match &self.kind {
            Kind::Simple(font) => {
                if let Some(text) = font.to_unicode.as_ref().and_then(|map| map.unicode(code)) {
                    return (text, true);
                }
                let unit = usize::try_from(code)
                    .ok()
                    .and_then(|code| font.encoding.get(code))
                    .copied()
                    .unwrap_or(0);
                let text = if unit < 0x20 {
                    char::REPLACEMENT_CHARACTER.to_string()
                } else {
                    cmap::utf16_lossy(&[unit])
                };
                (text, false)
            }
            Kind::Composite(font) => {
                match font.to_unicode.as_ref().and_then(|map| map.unicode(code)) {
                    Some(text) => (text, true),
                    None => (String::new(), false),
                }
            }
        }
    }
}

/// Fonts and CMaps parsed during one document walk, keyed by their objects, so a font that many
/// resource dictionaries name is parsed once and two fonts that share a resource name are never
/// confused.
pub struct FontCache {
    fonts: HashMap<Key, Rc<Font>>,
    cmaps: HashMap<Key, Rc<CMap>>,
    max_fonts: usize,
}

impl FontCache {
    pub fn new(max_fonts: usize) -> FontCache {
        FontCache {
            fonts: HashMap::new(),
            cmaps: HashMap::new(),
            max_fonts,
        }
    }

    /// The font a `/Font` resource value names, or `None` when it is not a dictionary. Past
    /// `max_fonts` distinct fonts the walk stops (`Err`).
    pub fn font(&mut self, doc: &Document, object: &Object) -> Result<Option<Rc<Font>>, ()> {
        let Some((id, resolved)) = objects::resolve(doc, object) else {
            return Ok(None);
        };
        let Object::Dictionary(dict) = resolved else {
            return Ok(None);
        };
        let key = Key::of(id, if id.is_some() { resolved } else { object });
        if let Some(font) = self.fonts.get(&key) {
            return Ok(Some(font.clone()));
        }
        if self.fonts.len() >= self.max_fonts {
            return Err(());
        }
        let font = Rc::new(self.load(doc, dict, id));
        self.fonts.insert(key, font.clone());
        Ok(Some(font))
    }

    fn cmap(&mut self, doc: &Document, object: &Object) -> Option<Rc<CMap>> {
        let (id, resolved) = objects::resolve(doc, object)?;
        let Object::Stream(stream) = resolved else {
            return None;
        };
        let key = Key::of(id, if id.is_some() { resolved } else { object });
        if let Some(cmap) = self.cmaps.get(&key) {
            return Some(cmap.clone());
        }
        let cmap = Rc::new(CMap::parse(get_contents(stream)));
        self.cmaps.insert(key, cmap.clone());
        Some(cmap)
    }

    fn to_unicode(&mut self, doc: &Document, font: &Dictionary) -> Option<Rc<CMap>> {
        let cmap = self.cmap(doc, font.get(b"ToUnicode").ok()?)?;
        (!cmap.is_empty()).then_some(cmap)
    }

    fn load(&mut self, doc: &Document, dict: &Dictionary, id: Option<ObjectId>) -> Font {
        let subtype = objects::get_name(doc, dict, b"Subtype").unwrap_or(b"Type1");
        let to_unicode = self.to_unicode(doc, dict);
        let mut facts = FontFacts {
            id,
            composite: subtype == b"Type0",
            type3: subtype == b"Type3",
            has_to_unicode: to_unicode.is_some(),
        };
        let kind = match subtype {
            b"Type0" => Kind::Composite(self.composite(doc, dict, to_unicode)),
            b"Type3" => Kind::Simple(simple_type3(doc, dict, to_unicode)),
            _ => Kind::Simple(simple(doc, dict, subtype, to_unicode)),
        };
        if let Kind::Composite(font) = &kind {
            facts.has_to_unicode = font.to_unicode.is_some();
        }
        Font { facts, kind }
    }

    fn composite(
        &mut self,
        doc: &Document,
        dict: &Dictionary,
        to_unicode: Option<Rc<CMap>>,
    ) -> Composite {
        let encoding = match dict
            .get(b"Encoding")
            .ok()
            .and_then(|encoding| objects::resolved(doc, encoding))
        {
            Some(Object::Stream(_)) => dict
                .get(b"Encoding")
                .ok()
                .and_then(|encoding| self.cmap(doc, encoding)),
            _ => None,
        };
        let descendant = objects::get_array(doc, dict, b"DescendantFonts")
            .and_then(|fonts| fonts.first())
            .and_then(|font| objects::resolved(doc, font))
            .and_then(objects::as_dict);
        let mut widths = CidWidths::default();
        let mut default_width = 1000.0;
        if let Some(descendant) = descendant {
            if let Some(width) = objects::get_number(doc, descendant, b"DW") {
                default_width = width;
            }
            if let Some(items) = objects::get_array(doc, descendant, b"W") {
                widths = cid_widths(doc, items);
            }
        }
        Composite {
            encoding,
            to_unicode,
            widths,
            default_width,
        }
    }
}

/// A `/W` array: `c [w1 w2 …]` and `c_first c_last w` entries, ranges kept as ranges.
fn cid_widths(doc: &Document, items: &[Object]) -> CidWidths {
    let mut widths = CidWidths::default();
    let cid = |object: &Object| match objects::resolved(doc, object) {
        Some(Object::Integer(value)) => u32::try_from(*value).ok(),
        _ => None,
    };
    let mut index = 0;
    while index < items.len() {
        let Some(first) = cid(&items[index]) else {
            index += 1;
            continue;
        };
        match items
            .get(index + 1)
            .and_then(|next| objects::resolved(doc, next))
        {
            Some(Object::Array(list)) => {
                for (offset, width) in list.iter().enumerate() {
                    let Some(code) = u32::try_from(offset)
                        .ok()
                        .and_then(|offset| first.checked_add(offset))
                    else {
                        break;
                    };
                    if let Some(width) = objects::resolved(doc, width).and_then(objects::number) {
                        widths.single.insert(code, width);
                    }
                }
                index += 2;
            }
            Some(next) => {
                let last = match next {
                    Object::Integer(value) => u32::try_from(*value).ok(),
                    _ => None,
                };
                let width = items
                    .get(index + 2)
                    .and_then(|width| objects::resolved(doc, width))
                    .and_then(objects::number);
                if let (Some(last), Some(width)) = (last, width) {
                    if first <= last {
                        widths.ranges.push((first, last, width));
                    }
                }
                index += 3;
            }
            None => break,
        }
    }
    widths.ranges.sort_by_key(|range| range.0);
    widths
}

/// A named base encoding as UTF-16 units, or `None` for a name this crate does not know.
fn named_encoding(name: &[u8]) -> Option<[u16; 256]> {
    let table = match name {
        b"StandardEncoding" => &encodings::STANDARD_ENCODING,
        b"MacRomanEncoding" => &encodings::MAC_ROMAN_ENCODING,
        b"MacExpertEncoding" => &encodings::MAC_EXPERT_ENCODING,
        b"WinAnsiEncoding" => &encodings::WIN_ANSI_ENCODING,
        _ => return None,
    };
    Some(names_to_units(table))
}

fn names_to_units(table: &[Option<&str>; 256]) -> [u16; 256] {
    let mut units = [0u16; 256];
    for (unit, name) in units.iter_mut().zip(table.iter()) {
        *unit = name.and_then(glyphnames::name_to_unicode).unwrap_or(0);
    }
    units
}

/// A glyph name's single UTF-16 unit, from the Adobe glyph list or a `uniXXXX`/`uXXXX` name.
fn glyph_unit(name: &[u8]) -> Option<u16> {
    let text = cmap::glyph_name_text(name)?;
    let mut units = text.encode_utf16();
    let unit = units.next()?;
    units.next().is_none().then_some(unit)
}

/// `/Differences` over `table`: an integer sets the code, each name assigns the next one.
fn apply_differences(doc: &Document, table: &mut [u16; 256], differences: &[Object]) {
    let mut code: Option<usize> = None;
    for item in differences {
        match objects::resolved(doc, item) {
            Some(Object::Integer(value)) => {
                code = usize::try_from(*value).ok().filter(|code| *code < 256)
            }
            Some(Object::Name(name)) => {
                if let Some(at) = code {
                    if let Some(unit) = glyph_unit(name) {
                        table[at] = unit;
                    }
                    code = (at + 1 < 256).then_some(at + 1);
                }
            }
            _ => {}
        }
    }
}

/// The encoding a font program carries: a Type 1 `/FontFile`'s clear text, or a CFF
/// (`/FontFile3 /Type1C`) encoding and charset.
fn builtin_encoding(
    doc: &Document,
    descriptor: Option<&Dictionary>,
    subtype: &[u8],
) -> Option<[u16; 256]> {
    let descriptor = descriptor?;
    if subtype == b"Type1" || subtype == b"MMType1" {
        if let Some((_, stream)) = objects::get_stream(doc, descriptor, b"FontFile") {
            match type1::builtin_encoding(get_contents(stream))? {
                BuiltinEncoding::Standard => return named_encoding(b"StandardEncoding"),
                BuiltinEncoding::Table(entries) => {
                    let mut table = [0u16; 256];
                    for (code, name) in entries {
                        if let Some(unit) = glyph_unit(&name) {
                            table[usize::from(code)] = unit;
                        }
                    }
                    return Some(table);
                }
            }
        }
    }
    let (_, stream) = objects::get_stream(doc, descriptor, b"FontFile3")?;
    if objects::get_name(doc, &stream.dict, b"Subtype") != Some(b"Type1C") {
        return None;
    }
    let cff = cff_parser::Table::parse(get_contents(stream))?;
    Some(cff_encoding(&cff))
}

/// A CFF font's code-to-glyph-name table as UTF-16 units. cff-parser's own
/// `get_code_to_sid_table` indexes its charset without a bound check, so this walks the
/// encoding with the charset's checked `gid_to_sid`.
fn cff_encoding(cff: &cff_parser::Table) -> [u16; 256] {
    use cff_parser::{EncodingKind, GlyphId, StringId, STANDARD_ENCODING};
    let mut table = [0u16; 256];
    let mut set = |code: u8, sid: Option<StringId>| {
        let Some(name) = sid.and_then(|sid| cff_parser::string_by_id(cff, sid)) else {
            return;
        };
        if name == ".notdef" {
            return;
        }
        if let Some(unit) = glyphnames::name_to_unicode(name)
            .or_else(|| zapfglyphnames::zapfdigbats_names_to_unicode(name))
        {
            table[usize::from(code)] = unit;
        }
    };
    match cff.encoding.kind {
        EncodingKind::Standard => {
            for (code, sid) in STANDARD_ENCODING.iter().enumerate() {
                if let Ok(code) = u8::try_from(code) {
                    set(code, Some(StringId(u16::from(*sid))));
                }
            }
        }
        EncodingKind::Expert => {}
        EncodingKind::Format0(codes) => {
            for (index, code) in codes.into_iter().enumerate() {
                let Some(gid) = u16::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                else {
                    break;
                };
                set(code, cff.charset.gid_to_sid(GlyphId(gid)));
            }
        }
        EncodingKind::Format1(ranges) => {
            let mut gid: u16 = 1;
            for range in ranges {
                for offset in 0..=range.left {
                    let Some(code) = range.first.checked_add(offset) else {
                        break;
                    };
                    set(code, cff.charset.gid_to_sid(GlyphId(gid)));
                    let Some(next) = gid.checked_add(1) else {
                        return table;
                    };
                    gid = next;
                }
            }
        }
    }
    table
}

fn simple(
    doc: &Document,
    dict: &Dictionary,
    subtype: &[u8],
    to_unicode: Option<Rc<CMap>>,
) -> Simple {
    let base_font = objects::get_name(doc, dict, b"BaseFont").unwrap_or(b"");
    let descriptor = objects::get_dict(doc, dict, b"FontDescriptor");
    let builtin = builtin_encoding(doc, descriptor, subtype);
    let mut encoding: Option<[u16; 256]> = match objects::get(doc, dict, b"Encoding") {
        Some(Object::Name(name)) => named_encoding(name).or(builtin),
        Some(Object::Dictionary(encoding)) => {
            let mut table = objects::get_name(doc, encoding, b"BaseEncoding")
                .and_then(named_encoding)
                .or(builtin)
                .unwrap_or_else(pdf_doc_encoding);
            if let Some(differences) = objects::get_array(doc, encoding, b"Differences") {
                apply_differences(doc, &mut table, differences);
            }
            Some(table)
        }
        _ => builtin.or_else(|| {
            (subtype == b"TrueType").then(|| names_to_units(&encodings::WIN_ANSI_ENCODING))
        }),
    };
    let mut widths = [None; 256];
    let first = objects::get_int(doc, dict, b"FirstChar");
    let listed = objects::get_array(doc, dict, b"Widths");
    if let (Some(first), Some(listed)) = (first, listed) {
        for (offset, width) in listed.iter().enumerate() {
            let code = i64::try_from(offset)
                .ok()
                .and_then(|offset| first.checked_add(offset));
            let Some(code) = code
                .and_then(|code| usize::try_from(code).ok())
                .filter(|code| *code < 256)
            else {
                continue;
            };
            widths[code] = objects::resolved(doc, width).and_then(objects::number);
        }
    } else {
        // A standard 14 font may omit its widths; any other font without them gets
        // Helvetica's, as pdf-extract always did.
        let name = std::str::from_utf8(base_font).unwrap_or("");
        let name = if core_fonts::metrics()
            .iter()
            .any(|metrics| metrics.0 == name)
        {
            name
        } else {
            "Helvetica"
        };
        if let Some(metrics) = core_fonts::metrics()
            .iter()
            .find(|metrics| metrics.0 == name)
        {
            let table = encoding.get_or_insert_with(|| {
                let mut table = [0u16; 256];
                for (code, _, glyph) in metrics.2.iter() {
                    if let Ok(code) = usize::try_from(*code) {
                        if code < 256 {
                            table[code] = glyphnames::name_to_unicode(glyph)
                                .or_else(|| zapfglyphnames::zapfdigbats_names_to_unicode(glyph))
                                .unwrap_or(0);
                        }
                    }
                }
                table
            });
            for (_, width, glyph) in metrics.2.iter() {
                let Some(unit) = glyphnames::name_to_unicode(glyph)
                    .or_else(|| zapfglyphnames::zapfdigbats_names_to_unicode(glyph))
                else {
                    continue;
                };
                for (code, entry) in table.iter().enumerate() {
                    if *entry == unit {
                        widths[code] = Some(*width);
                    }
                }
            }
        }
    }
    let missing_width = descriptor
        .and_then(|descriptor| objects::get_number(doc, descriptor, b"MissingWidth"))
        .unwrap_or(0.0);
    Simple {
        encoding: encoding.unwrap_or_else(pdf_doc_encoding),
        to_unicode,
        widths,
        missing_width,
        scale: 0.001,
    }
}

fn simple_type3(doc: &Document, dict: &Dictionary, to_unicode: Option<Rc<CMap>>) -> Simple {
    let mut encoding = pdf_doc_encoding();
    if let Some(Object::Dictionary(table)) = objects::get(doc, dict, b"Encoding") {
        if let Some(base) = objects::get_name(doc, table, b"BaseEncoding").and_then(named_encoding)
        {
            encoding = base;
        }
        if let Some(differences) = objects::get_array(doc, table, b"Differences") {
            apply_differences(doc, &mut encoding, differences);
        }
    }
    let mut widths = [None; 256];
    if let (Some(first), Some(listed)) = (
        objects::get_int(doc, dict, b"FirstChar"),
        objects::get_array(doc, dict, b"Widths"),
    ) {
        for (offset, width) in listed.iter().enumerate() {
            let code = i64::try_from(offset)
                .ok()
                .and_then(|offset| first.checked_add(offset));
            if let Some(code) = code
                .and_then(|code| usize::try_from(code).ok())
                .filter(|code| *code < 256)
            {
                widths[code] = objects::resolved(doc, width).and_then(objects::number);
            }
        }
    }
    // Widths are in glyph space, which the font matrix maps to text space.
    let scale = match objects::get_numbers(doc, dict, b"FontMatrix").as_deref() {
        Some([a, ..]) => *a,
        _ => 0.001,
    };
    Simple {
        encoding,
        to_unicode,
        widths,
        missing_width: 0.0,
        scale,
    }
}

fn pdf_doc_encoding() -> [u16; 256] {
    PDF_DOC_ENCODING
}
