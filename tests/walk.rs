//! The walker's reports on small documents built in memory: glyph paint state, areas, Form
//! XObjects, transparency groups, clipping, optional content, `/ActualText`, limits, and
//! malformed input that must not panic.
use std::rc::Rc;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use pdf_extract::{
    output_pages, test_hooks, Area, AreaKind, BlendMode, ColourSpace, Glyph, MediaBox, Optional,
    Options, OutputDev, OutputError, Rect, Transform,
};

#[derive(Debug, Clone)]
struct Seen {
    text: String,
    mapped: bool,
    render_mode: u8,
    fill: (ColourSpace, Vec<f64>),
    stroke: (ColourSpace, Vec<f64>),
    fill_alpha: f64,
    stroke_alpha: f64,
    soft_mask: bool,
    blend: BlendMode,
    clip: Option<Rect>,
    clip_rect: bool,
    optional: Optional,
    groups: Vec<(f64, bool)>,
    bbox: Option<Rect>,
    actual: Option<(u32, String)>,
    composite: bool,
}

#[derive(Debug, Clone)]
struct SeenArea {
    kind: AreaKind,
    bbox: Rect,
    rect: bool,
    fill: Option<(ColourSpace, Vec<f64>)>,
}

#[derive(Default)]
struct Recorder {
    glyphs: Vec<Seen>,
    areas: Vec<SeenArea>,
    order: Vec<char>,
    operators: u64,
    stop_after: Option<u64>,
}

impl OutputDev for Recorder {
    fn begin_page(
        &mut self,
        _: u32,
        _: &MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_page(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn output_character(
        &mut self,
        _: &Transform,
        _: f64,
        _: f64,
        _: f64,
        _: &str,
    ) -> Result<(), OutputError> {
        Ok(())
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn operator(&mut self) -> Result<(), OutputError> {
        self.operators += 1;
        if self.stop_after.is_some_and(|limit| self.operators > limit) {
            return Err(OutputError::Stopped);
        }
        Ok(())
    }
    fn glyph(&mut self, glyph: &Glyph) -> Result<(), OutputError> {
        self.order.push('g');
        self.glyphs.push(Seen {
            text: glyph.text.to_owned(),
            mapped: glyph.mapped,
            render_mode: glyph.render_mode,
            fill: ((*glyph.fill.space).clone(), glyph.fill.components.clone()),
            stroke: (
                (*glyph.stroke.space).clone(),
                glyph.stroke.components.clone(),
            ),
            fill_alpha: glyph.fill_alpha,
            stroke_alpha: glyph.stroke_alpha,
            soft_mask: glyph.soft_mask,
            blend: glyph.blend,
            clip: glyph.clip.bbox,
            clip_rect: glyph.clip.rect,
            optional: glyph.optional,
            groups: glyph
                .groups
                .iter()
                .map(|group| (group.alpha, group.knockout))
                .collect(),
            bbox: glyph.bbox(),
            actual: glyph.actual_text.map(|(id, text)| (id, text.to_owned())),
            composite: glyph.font.composite,
        });
        Ok(())
    }
    fn area(&mut self, area: &Area) -> Result<(), OutputError> {
        self.order.push('a');
        self.areas.push(SeenArea {
            kind: area.kind,
            bbox: area.bbox,
            rect: area.rect,
            fill: area
                .fill
                .map(|colour| ((*colour.space).clone(), colour.components.clone())),
        });
        Ok(())
    }
}

/// A one-page document whose page draws `content`, with Helvetica as `/F1` and the extra
/// resources given.
struct Builder {
    doc: Document,
    resources: Dictionary,
}

impl Builder {
    fn new() -> Builder {
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let resources = dictionary! { "Font" => dictionary! { "F1" => font } };
        Builder { doc, resources }
    }

    fn add(&mut self, object: impl Into<Object>) -> ObjectId {
        self.doc.add_object(object)
    }

    fn resource(&mut self, kind: &str, name: &str, value: impl Into<Object>) {
        let entry = self
            .resources
            .get_mut(kind.as_bytes())
            .ok()
            .and_then(|o| o.as_dict_mut().ok())
            .is_some();
        if !entry {
            self.resources.set(kind, Dictionary::new());
        }
        if let Ok(Object::Dictionary(dict)) = self.resources.get_mut(kind.as_bytes()) {
            dict.set(name, value.into());
        }
    }

    fn form(&mut self, extra: Dictionary, content: &str) -> ObjectId {
        let mut dict = dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 200.into(), 200.into()] };
        for (key, value) in extra.iter() {
            dict.set(key.clone(), value.clone());
        }
        self.add(Stream::new(dict, content.as_bytes().to_vec()))
    }

    fn page(mut self, content: &str) -> (Document, ObjectId) {
        let contents = self.add(Stream::new(Dictionary::new(), content.as_bytes().to_vec()));
        let pages = self.doc.new_object_id();
        let page = self.add(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => self.resources.clone(),
        });
        self.doc.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = self.add(dictionary! { "Type" => "Catalog", "Pages" => pages });
        self.doc.trailer.set("Root", catalog);
        (self.doc, page)
    }
}

fn walk(doc: &Document, page: ObjectId) -> Recorder {
    let mut recorder = Recorder::default();
    output_pages(doc, &[page], &Options::default(), &mut recorder).unwrap();
    recorder
}

fn text(recorder: &Recorder) -> String {
    recorder
        .glyphs
        .iter()
        .map(|glyph| glyph.text.as_str())
        .collect()
}

#[test]
fn glyphs_carry_colour_alpha_blend_and_mode() {
    let mut builder = Builder::new();
    let state = builder.add(dictionary! { "Type" => "ExtGState", "ca" => 0.5, "CA" => 0.25, "BM" => vec![Object::Name(b"Bogus".to_vec()), Object::Name(b"Multiply".to_vec())], "SMask" => dictionary! {} });
    builder.resource("ExtGState", "G1", state);
    let (doc, page) =
        builder.page("BT /F1 10 Tf 10 10 Td (A) Tj 1 0 0 rg 0 0 1 RG (B) Tj 2 Tr /G1 gs (C) Tj ET");
    let seen = walk(&doc, page);
    assert_eq!(text(&seen), "ABC");
    let [a, b, c] = &seen.glyphs[..] else {
        panic!("three glyphs")
    };
    assert_eq!(a.fill, (ColourSpace::DeviceGray, vec![0.0]));
    assert_eq!(b.fill, (ColourSpace::DeviceRGB, vec![1.0, 0.0, 0.0]));
    assert_eq!(b.stroke, (ColourSpace::DeviceRGB, vec![0.0, 0.0, 1.0]));
    assert_eq!(
        (
            c.render_mode,
            c.fill_alpha,
            c.stroke_alpha,
            c.soft_mask,
            c.blend
        ),
        (2, 0.5, 0.25, true, BlendMode::Multiply)
    );
    assert_eq!(
        (a.fill_alpha, a.soft_mask, a.blend),
        (1.0, false, BlendMode::Normal)
    );
    let bbox = a.bbox.unwrap();
    assert!(
        (bbox.x0 - 10.0).abs() < 1e-9
            && (bbox.y0 - 8.0).abs() < 1e-9
            && (bbox.y1 - 18.0).abs() < 1e-9
    );
}

#[test]
fn colour_space_operators_set_initial_values() {
    let mut builder = Builder::new();
    let separation = Object::Array(vec![
        "Separation".into(),
        "None".into(),
        "DeviceCMYK".into(),
        Object::Dictionary(
            dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()], "N" => 1 },
        ),
    ]);
    builder.resource("ColorSpace", "S", separation);
    builder.resource("ColorSpace", "C", Object::Array(vec!["DeviceCMYK".into()]));
    let (doc, page) = builder.page("BT /F1 10 Tf /S cs (a) Tj /C cs (b) Tj /Pattern cs (c) Tj ET");
    let seen = walk(&doc, page);
    assert!(
        matches!(&seen.glyphs[0].fill, (ColourSpace::Separation { colorant, .. }, components) if colorant == b"None" && components == &vec![1.0])
    );
    assert_eq!(
        seen.glyphs[1].fill,
        (ColourSpace::DeviceCMYK, vec![0.0, 0.0, 0.0, 1.0])
    );
    assert_eq!(seen.glyphs[2].fill.0, ColourSpace::Pattern);
}

#[test]
fn forms_inherit_the_invoking_state_and_end_at_their_return() {
    let mut builder = Builder::new();
    let font = builder.resources.clone();
    let inner = builder.form(dictionary! { "Resources" => font, "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 50.into(), 0.into()] }, "BT /F1 10 Tf 0 0 Td (x) Tj ET 0 g");
    builder.resource("XObject", "X", inner);
    let (doc, page) = builder.page("1 g /X Do BT /F1 10 Tf (y) Tj ET");
    let seen = walk(&doc, page);
    assert_eq!(text(&seen), "xy");
    assert_eq!(
        seen.glyphs[0].fill.1,
        vec![1.0],
        "the Form inherits the page's 1 g"
    );
    assert_eq!(
        seen.glyphs[1].fill.1,
        vec![1.0],
        "the Form's 0 g ends at its return"
    );
    let bbox = seen.glyphs[0].bbox.unwrap();
    assert!(
        (bbox.x0 - 50.0).abs() < 1e-9,
        "the Form's /Matrix moves its text"
    );
    assert_eq!(
        seen.glyphs[0].clip,
        Some(Rect {
            x0: 50.0,
            y0: 0.0,
            x1: 250.0,
            y1: 200.0
        }),
        "its /BBox clips"
    );
}

#[test]
fn transparency_groups_fold_and_reset() {
    let mut builder = Builder::new();
    let half = builder.add(dictionary! { "Type" => "ExtGState", "ca" => 0.3 });
    let one = builder.add(dictionary! { "Type" => "ExtGState", "ca" => 1 });
    builder.resource("ExtGState", "H", half);
    builder.resource("ExtGState", "One", one);
    let resources = builder.resources.clone();
    let group = dictionary! { "S" => "Transparency", "K" => true };
    let inner = builder.form(
        dictionary! { "Resources" => resources.clone(), "Group" => group.clone() },
        "/One gs BT /F1 10 Tf (i) Tj ET",
    );
    let mut with_inner = resources.clone();
    with_inner.set("XObject", dictionary! { "In" => inner });
    let outer = builder.form(
        dictionary! { "Resources" => with_inner, "Group" => group },
        "/H gs /In Do",
    );
    builder.resource("XObject", "Out", outer);
    let (doc, page) = builder.page("/H gs /Out Do");
    let seen = walk(&doc, page);
    let glyph = &seen.glyphs[0];
    assert!(
        (glyph.fill_alpha - 0.09).abs() < 1e-6,
        "{}",
        glyph.fill_alpha
    );
    assert_eq!(glyph.groups.len(), 2);
    assert!(glyph
        .groups
        .iter()
        .all(|(alpha, knockout)| (alpha - 0.3).abs() < 1e-6 && *knockout));
}

#[test]
fn areas_report_shape_rectangles_and_clip_timing() {
    let (doc, page) = Builder::new().page(
        "q 10 10 50 50 re W f \
         1 0 0 rg 0 0 200 200 re f Q \
         q 0.7071 0.7071 -0.7071 0.7071 100 0 cm 0 0 20 20 re f Q \
         20 20 m 180 180 l 180 20 l 20 180 l h f \
         0 0 m 0 10 l 10 10 l 10 0 l f \
         0 0 100 100 re 10 10 80 80 re f*",
    );
    let seen = walk(&doc, page);
    let areas = &seen.areas;
    assert_eq!(areas.len(), 6);
    assert_eq!(
        areas[0].bbox,
        Rect {
            x0: 10.0,
            y0: 10.0,
            x1: 60.0,
            y1: 60.0
        }
    );
    assert!(areas[0].rect, "re W f is not clipped by its own clip");
    assert_eq!(
        areas[1].bbox,
        Rect {
            x0: 10.0,
            y0: 10.0,
            x1: 60.0,
            y1: 60.0
        },
        "later areas are"
    );
    assert_eq!(
        areas[1].fill,
        Some((ColourSpace::DeviceRGB, vec![1.0, 0.0, 0.0]))
    );
    assert!(!areas[2].rect, "a re under a 45-degree CTM is no rectangle");
    assert!(!areas[3].rect, "a bow-tie is no rectangle");
    assert!(
        areas[4].rect,
        "four straight segments alternating H and V are one"
    );
    assert!(!areas[5].rect, "two subpaths are not one rectangle");
    assert_eq!(areas[5].kind, AreaKind::Fill { even_odd: true });
}

#[test]
fn images_inline_images_and_shadings_are_areas() {
    let mut builder = Builder::new();
    let image = builder.add(Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1, "SMask" => Object::Null }, vec![0]));
    builder.resource("XObject", "Im", image);
    builder.resource("Shading", "Sh", dictionary! { "ShadingType" => 2 });
    let (doc, page) = builder.page(
        "q 100 0 0 50 0 0 cm /Im Do Q \
         q 20 0 0 20 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /G ID \x00 EI Q \
         q 20 0 0 20 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /G /F /AHx ID ff> EI Q \
         /Sh sh",
    );
    let seen = walk(&doc, page);
    let kinds: Vec<AreaKind> = seen.areas.iter().map(|area| area.kind).collect();
    assert_eq!(
        kinds,
        vec![
            AreaKind::Image { masked: true },
            AreaKind::InlineImage {
                masked: false,
                parsed: true
            },
            AreaKind::InlineImage {
                masked: false,
                parsed: false
            },
            AreaKind::Shading,
        ]
    );
    assert_eq!(
        seen.areas[0].bbox,
        Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 50.0
        }
    );
    assert_eq!(
        seen.areas[3].bbox,
        Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 200.0,
            y1: 200.0
        }
    );
    assert!(seen.areas.iter().all(|area| area.fill.is_none()));
}

#[test]
fn optional_content_hides_glyphs_and_drops_areas() {
    let mut builder = Builder::new();
    let off = builder.add(dictionary! { "Type" => "OCG", "Name" => "off" });
    let on = builder.add(dictionary! { "Type" => "OCG", "Name" => "on" });
    let viewstate = builder.add(dictionary! { "Type" => "OCG", "Name" => "v", "Usage" => dictionary! { "View" => dictionary! { "ViewState" => "OFF" } } });
    let ve = builder.add(dictionary! { "Type" => "OCMD", "OCGs" => vec![on.into()], "VE" => vec![Object::Name(b"And".to_vec()), on.into()] });
    let all_on = builder.add(
        dictionary! { "Type" => "OCMD", "OCGs" => vec![off.into(), on.into()], "P" => "AllOn" },
    );
    let empty = builder.add(dictionary! { "Type" => "OCMD", "OCGs" => vec![Object::Null] });
    builder.resource("Properties", "Off", off);
    builder.resource("Properties", "On", on);
    builder.resource("Properties", "View", viewstate);
    builder.resource("Properties", "Ve", ve);
    builder.resource("Properties", "AllOn", all_on);
    builder.resource("Properties", "Empty", empty);
    let resources = builder.resources.clone();
    let form = builder.form(
        dictionary! { "Resources" => resources, "OC" => off },
        "BT /F1 10 Tf (f) Tj ET 0 0 5 5 re f",
    );
    builder.resource("XObject", "F", form);
    let (mut doc, page) = builder.page(
        "BT /F1 10 Tf /OC /Off BDC (a) Tj EMC /OC /On BDC (b) Tj /OC /Off BDC (c) Tj EMC EMC \
         /OC /View BDC (d) Tj EMC /OC /Ve BDC (e) Tj EMC /OC /AllOn BDC (g) Tj EMC /OC /Empty BDC (h) Tj EMC ET \
         /OC /Off BDC 0 0 9 9 re f EMC /OC /Ve BDC 0 0 9 9 re f EMC /F Do /Span << /MCID 0 >> BDC 0 0 1 1 re f EMC",
    );
    let properties = doc.add_object(dictionary! {
        "OCGs" => vec![off.into(), on.into(), viewstate.into()],
        "D" => dictionary! { "OFF" => vec![off.into()] },
    });
    let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    doc.get_dictionary_mut(catalog)
        .unwrap()
        .set("OCProperties", properties);
    let seen = walk(&doc, page);
    let states: Vec<(String, Optional)> = seen
        .glyphs
        .iter()
        .map(|glyph| (glyph.text.clone(), glyph.optional))
        .collect();
    assert_eq!(
        states,
        vec![
            ("a".into(), Optional::Hidden),
            ("b".into(), Optional::Visible),
            ("c".into(), Optional::Hidden),
            ("d".into(), Optional::Hidden),
            ("e".into(), Optional::Unknown),
            ("g".into(), Optional::Hidden),
            ("h".into(), Optional::Visible),
            ("f".into(), Optional::Hidden),
        ]
    );
    assert_eq!(
        seen.areas.len(),
        2,
        "the off areas are never reported: {:?}",
        seen.areas
    );
    assert_eq!(
        seen.areas[0].fill, None,
        "an area under /VE has an unknown colour"
    );
}

#[test]
fn actual_text_spans_are_reported_outermost_first() {
    let (doc, page) = Builder::new().page(
        "BT /F1 10 Tf /Span << /ActualText <FEFF0E010E33> >> BDC (ab) Tj /Span << /ActualText (x) >> BDC (c) Tj EMC EMC (d) Tj ET",
    );
    let seen = walk(&doc, page);
    let spans: Vec<Option<String>> = seen
        .glyphs
        .iter()
        .map(|glyph| glyph.actual.as_ref().map(|(_, text)| text.clone()))
        .collect();
    let thai = Some("\u{E01}\u{E33}".to_owned());
    assert_eq!(spans, vec![thai.clone(), thai.clone(), thai, None]);
    assert_eq!(
        seen.glyphs[0].actual.as_ref().unwrap().0,
        seen.glyphs[2].actual.as_ref().unwrap().0
    );
}

#[test]
fn quote_operators_show_text_and_text_clips_follow_et() {
    let (doc, page) = Builder::new().page(
        "BT /F1 10 Tf 12 TL 10 100 Td (a) Tj (b) ' 3 1 (c) \" ET \
         BT 7 Tr /F1 10 Tf 10 10 Td (Z) Tj ET BT 0 Tr /F1 10 Tf 150 150 Td (q) Tj ET",
    );
    let seen = walk(&doc, page);
    assert_eq!(text(&seen), "abcZq");
    let (a, b) = (seen.glyphs[0].bbox.unwrap(), seen.glyphs[1].bbox.unwrap());
    assert!(
        (a.y0 - b.y0 - 12.0).abs() < 1e-9,
        "' moves to the next line"
    );
    assert!(
        seen.glyphs[3].clip.is_none(),
        "the clip text itself is not clipped yet"
    );
    let clip = seen.glyphs[4]
        .clip
        .expect("text after a mode-7 object is clipped");
    assert!(!clip.contains_point(155.0, 152.0));
    assert!(!seen.glyphs[4].clip_rect);
}

#[test]
fn fonts_are_keyed_by_object_not_resource_name() {
    let mut builder = Builder::new();
    let other = builder.add(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => dictionary! { "Differences" => vec![65.into(), "Z".into()] },
    });
    let form = builder.form(
        dictionary! { "Resources" => dictionary! { "Font" => dictionary! { "F1" => other } } },
        "BT /F1 10 Tf (A) Tj ET",
    );
    builder.resource("XObject", "X", form);
    let (doc, page) = builder.page("BT /F1 10 Tf (A) Tj ET /X Do BT /F1 10 Tf (A) Tj ET");
    assert_eq!(text(&walk(&doc, page)), "AZA");
}

#[test]
fn cid_width_ranges_are_never_expanded() {
    let mut builder = Builder::new();
    let cid = builder.add(dictionary! {
        "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "X",
        "W" => vec![0.into(), 4294967295i64.into(), 500.into()],
    });
    let to_unicode = builder.add(Stream::new(
        Dictionary::new(),
        b"1 beginbfrange <0000> <FFFF> <0041> endbfrange".to_vec(),
    ));
    let font = builder.add(dictionary! {
        "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "X", "Encoding" => "Identity-H",
        "DescendantFonts" => vec![cid.into()], "ToUnicode" => to_unicode,
    });
    builder.resource("Font", "F2", font);
    let (doc, page) = builder.page("BT /F2 10 Tf 0 0 Td <00010002> Tj ET");
    let seen = walk(&doc, page);
    assert_eq!(text(&seen), "BC");
    assert!(seen.glyphs[0].composite && seen.glyphs[0].mapped);
    let (first, second) = (seen.glyphs[0].bbox.unwrap(), seen.glyphs[1].bbox.unwrap());
    assert!(
        (second.x0 - first.x0 - 5.0).abs() < 1e-9,
        "500/1000 × 10 pt"
    );
}

#[test]
fn type3_widths_follow_the_font_matrix() {
    let mut builder = Builder::new();
    let font = builder.add(dictionary! {
        "Type" => "Font", "Subtype" => "Type3", "FontMatrix" => vec![0.01.into(), 0.into(), 0.into(), 0.01.into(), 0.into(), 0.into()],
        "FirstChar" => 65, "LastChar" => 66, "Widths" => vec![50.into()],
        "Encoding" => dictionary! { "Differences" => vec![65.into(), "A".into(), "B".into()] },
    });
    builder.resource("Font", "T3", font);
    let (doc, page) = builder.page("BT /T3 10 Tf (AB) Tj ET");
    let seen = walk(&doc, page);
    assert_eq!(text(&seen), "AB");
    let (a, b) = (seen.glyphs[0].bbox.unwrap(), seen.glyphs[1].bbox.unwrap());
    assert!((b.x0 - a.x0 - 5.0).abs() < 1e-6, "50 × 0.01 × 10 pt");
}

#[test]
fn form_depth_and_cycles_end_the_walk() {
    let mut builder = Builder::new();
    let id = builder.doc.new_object_id();
    let mut dict = dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()] };
    dict.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "Self" => id } },
    );
    builder
        .doc
        .objects
        .insert(id, Object::Stream(Stream::new(dict, b"/Self Do".to_vec())));
    builder.resource("XObject", "Self", id);
    let (doc, page) = builder.page("/Self Do");
    let mut recorder = Recorder::default();
    let result = output_pages(&doc, &[page], &Options::default(), &mut recorder);
    assert!(
        matches!(result, Err(OutputError::Malformed("xobject_cycle"))),
        "{result:?}"
    );

    let mut builder = Builder::new();
    let mut previous: Option<ObjectId> = None;
    for _ in 0..20 {
        let resources = match previous {
            Some(inner) => dictionary! { "XObject" => dictionary! { "N" => inner } },
            None => Dictionary::new(),
        };
        previous = Some(builder.form(dictionary! { "Resources" => resources }, "/N Do"));
    }
    builder.resource("XObject", "N", previous.unwrap());
    let (doc, page) = builder.page("/N Do");
    let result = output_pages(&doc, &[page], &Options::default(), &mut Recorder::default());
    assert!(
        matches!(result, Err(OutputError::Limit("xobject_depth"))),
        "{result:?}"
    );
}

#[test]
fn the_device_can_stop_the_walk() {
    let (doc, page) = Builder::new().page("BT /F1 10 Tf (a) Tj (b) Tj (c) Tj ET");
    let mut recorder = Recorder {
        stop_after: Some(3),
        ..Recorder::default()
    };
    let result = output_pages(&doc, &[page], &Options::default(), &mut recorder);
    assert!(matches!(result, Err(OutputError::Stopped)));
    assert_eq!(text(&recorder), "a");
}

#[test]
fn a_filtered_stream_is_a_panic_and_is_counted() {
    let mut builder = Builder::new();
    let filtered = builder.add(Stream::new(
        dictionary! { "Filter" => "FlateDecode" },
        b"x".to_vec(),
    ));
    let mut doc_page = builder.page("");
    let contents = doc_page.0.get_dictionary_mut(doc_page.1).unwrap();
    contents.set("Contents", filtered);
    test_hooks::reset();
    let (doc, page) = doc_page;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk(&doc, page)));
    let message = result
        .err()
        .and_then(|payload| payload.downcast_ref::<&str>().map(|text| text.to_string()));
    assert_eq!(
        message.as_deref(),
        Some("pdf-extract: filtered stream reached get_contents")
    );
    assert_eq!(test_hooks::filtered_get_contents(), 1);
}

#[test]
fn common_malformations_do_not_panic() {
    let cases: &[(&str, Dictionary)] = &[
        ("BT /Missing 10 Tf (a) Tj ET", Dictionary::new()),
        ("BT (before Tf) Tj ET", Dictionary::new()),
        ("1 2 3 4 5 cm 10 20 v 30 40 l f", Dictionary::new()),
        (
            "BT /D 10 Tf (\u{1}) Tj ET",
            dictionary! { "Font" => dictionary! { "D" => dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "X", "FirstChar" => 0, "LastChar" => 300,
                "Widths" => vec![1.into()],
                "Encoding" => dictionary! { "BaseEncoding" => "StandardEncoding", "Differences" => vec![300.into(), "A".into(), Object::Null, (-5).into(), "B".into()] },
            } } },
        ),
        (
            "BT /E 10 Tf (x) Tj ET",
            dictionary! { "Font" => dictionary! { "E" => dictionary! {
                "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "X", "Encoding" => "Identity-H",
            } } },
        ),
        (
            "BT /G 10 Tf <0001> Tj ET",
            dictionary! { "Font" => dictionary! { "G" => dictionary! {
                "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "X", "Encoding" => "UniGB-UCS2-H",
                "DescendantFonts" => vec![Object::Reference((999, 0))], "ToUnicode" => "Identity-H",
            } } },
        ),
        (
            "/Nope gs /X cs 1 2 sc /Nope Do /Nope sh 0 Tr 9 Tr 12 Tz W n",
            Dictionary::new(),
        ),
    ];
    for (content, resources) in cases {
        let mut builder = Builder::new();
        for (key, value) in resources.iter() {
            builder.resources.set(key.clone(), value.clone());
        }
        let (mut doc, page) = builder.page(content);
        doc.get_dictionary_mut(page).unwrap().remove(b"MediaBox");
        let mut recorder = Recorder::default();
        output_pages(&doc, &[page], &Options::default(), &mut recorder).unwrap();
    }
}

#[test]
fn colour_spaces_are_reported_as_data() {
    let mut builder = Builder::new();
    let icc = builder.add(Stream::new(dictionary! { "N" => 3 }, Vec::new()));
    builder.resource(
        "ColorSpace",
        "Icc",
        Object::Array(vec!["ICCBased".into(), icc.into()]),
    );
    builder.resource(
        "ColorSpace",
        "Idx",
        Object::Array(vec![
            "Indexed".into(),
            "DeviceRGB".into(),
            1.into(),
            Object::String(
                vec![0, 0, 0, 255, 255, 255],
                lopdf::StringFormat::Hexadecimal,
            ),
        ]),
    );
    let (doc, page) = builder.page("BT /F1 10 Tf /Icc cs 1 1 1 sc (a) Tj /Idx cs 1 sc (b) Tj ET");
    let seen = walk(&doc, page);
    assert_eq!(
        seen.glyphs[0].fill,
        (ColourSpace::IccBased { n: 3 }, vec![1.0, 1.0, 1.0])
    );
    match &seen.glyphs[1].fill {
        (
            ColourSpace::Indexed {
                base,
                hival,
                lookup,
            },
            components,
        ) => {
            assert_eq!(
                (&**base, *hival, &**lookup, components.as_slice()),
                (
                    &ColourSpace::DeviceRGB,
                    1,
                    &[0u8, 0, 0, 255, 255, 255][..],
                    &[1.0][..]
                )
            );
        }
        other => panic!("{other:?}"),
    }
    let _: Rc<ColourSpace> = Rc::new(ColourSpace::Unknown);
}
