//! Colours as data: the colour space a content stream selected and the operands it set, for the
//! output device to resolve. Nothing here converts a colour; a space this crate cannot describe
//! is `Unknown`, never a guess.
use std::collections::HashMap;
use std::rc::Rc;

use lopdf::{Dictionary, Document, Object};

use crate::objects::{self, get_contents, Key};

/// How deep colour spaces may nest (`Indexed` over `Separation` over `ICCBased`, say).
const MAX_SPACE_DEPTH: usize = 4;
/// The most components a `DeviceN` space may name.
const MAX_COMPONENTS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub enum ColourSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
    CalGray,
    CalRGB,
    Lab,
    /// `/N` from the profile stream's dictionary (1, 3 or 4), or 0 when it is missing or
    /// another value; the profile itself is never read.
    IccBased {
        n: u8,
    },
    /// `hival` and the lookup table's bytes as the file holds them.
    Indexed {
        base: Rc<ColourSpace>,
        hival: u32,
        lookup: Rc<[u8]>,
    },
    Separation {
        colorant: Vec<u8>,
        alternate: Rc<ColourSpace>,
        tint: Rc<Function>,
    },
    DeviceN {
        colorants: Vec<Vec<u8>>,
        alternate: Rc<ColourSpace>,
        tint: Rc<Function>,
    },
    Pattern,
    Unknown,
}

impl ColourSpace {
    /// The number of operands a colour in this space takes.
    pub fn components(&self) -> usize {
        match self {
            ColourSpace::DeviceGray | ColourSpace::CalGray => 1,
            ColourSpace::DeviceRGB | ColourSpace::CalRGB | ColourSpace::Lab => 3,
            ColourSpace::DeviceCMYK => 4,
            ColourSpace::IccBased { n } => usize::from(*n),
            ColourSpace::Indexed { .. } | ColourSpace::Separation { .. } => 1,
            ColourSpace::DeviceN { colorants, .. } => colorants.len(),
            ColourSpace::Pattern | ColourSpace::Unknown => 0,
        }
    }

    /// The colour `cs` and `CS` select (ISO 32000-1 Table 74): 0 for each component (ICCBased
    /// and Lab included), black (0 0 0 1) in DeviceCMYK, the full tint 1.0 in Separation and
    /// DeviceN, and no operands in a Pattern space.
    pub fn initial(&self) -> Vec<f64> {
        match self {
            ColourSpace::DeviceCMYK => vec![0.0, 0.0, 0.0, 1.0],
            ColourSpace::Separation { .. } | ColourSpace::DeviceN { .. } => {
                vec![1.0; self.components()]
            }
            other => vec![0.0; other.components()],
        }
    }
}

/// A tint transform as data. KatWork evaluates the Type 2 and one-input Type 0 forms itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Function {
    /// Type 2: `y = C0 + x^N × (C1 − C0)` for `x` clipped to `domain`.
    Exponential {
        domain: [f64; 2],
        c0: Vec<f64>,
        c1: Vec<f64>,
        n: f64,
        range: Option<Vec<f64>>,
    },
    /// Type 0, with its sample bytes as the (decoded) stream holds them.
    Sampled {
        domain: Vec<f64>,
        range: Vec<f64>,
        size: Vec<u32>,
        bits_per_sample: u32,
        order: u32,
        encode: Vec<f64>,
        decode: Vec<f64>,
        samples: Rc<[u8]>,
    },
    /// Type 3, Type 4, an array of functions, or anything malformed.
    Unsupported,
}

/// A colour: the space in force and the operands the stream set.
#[derive(Clone, Debug, PartialEq)]
pub struct Colour {
    pub space: Rc<ColourSpace>,
    pub components: Vec<f64>,
}

impl Colour {
    pub fn in_space(space: Rc<ColourSpace>) -> Colour {
        let components = space.initial();
        Colour { space, components }
    }

    pub fn black() -> Colour {
        Colour {
            space: Rc::new(ColourSpace::DeviceGray),
            components: vec![0.0],
        }
    }
}

/// The blend mode in force (ISO 32000-1 Tables 136 and 137, with `Compatible`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlendMode {
    Normal,
    Compatible,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
    /// A single name that is none of the above.
    Unknown,
}

impl BlendMode {
    /// `Normal` or `Compatible`, its synonym.
    pub fn is_normal(self) -> bool {
        matches!(self, BlendMode::Normal | BlendMode::Compatible)
    }

    fn from_name(name: &[u8]) -> Option<BlendMode> {
        Some(match name {
            b"Normal" => BlendMode::Normal,
            b"Compatible" => BlendMode::Compatible,
            b"Multiply" => BlendMode::Multiply,
            b"Screen" => BlendMode::Screen,
            b"Overlay" => BlendMode::Overlay,
            b"Darken" => BlendMode::Darken,
            b"Lighten" => BlendMode::Lighten,
            b"ColorDodge" => BlendMode::ColorDodge,
            b"ColorBurn" => BlendMode::ColorBurn,
            b"HardLight" => BlendMode::HardLight,
            b"SoftLight" => BlendMode::SoftLight,
            b"Difference" => BlendMode::Difference,
            b"Exclusion" => BlendMode::Exclusion,
            b"Hue" => BlendMode::Hue,
            b"Saturation" => BlendMode::Saturation,
            b"Color" => BlendMode::Color,
            b"Luminosity" => BlendMode::Luminosity,
            _ => return None,
        })
    }

    /// `/BM`: a name, or an array whose first recognised name applies (`Normal` when it names
    /// none, ISO 32000-1 §11.6.3). A single unrecognised name is `Unknown`.
    pub fn from_object(doc: &Document, object: &Object) -> Option<BlendMode> {
        match objects::resolved(doc, object)? {
            Object::Name(name) => Some(BlendMode::from_name(name).unwrap_or(BlendMode::Unknown)),
            Object::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|item| match objects::resolved(doc, item) {
                        Some(Object::Name(name)) => BlendMode::from_name(name),
                        _ => None,
                    })
                    .next()
                    .unwrap_or(BlendMode::Normal),
            ),
            _ => None,
        }
    }
}

/// Parsed colour spaces and functions, one per object, for one document walk.
#[derive(Default)]
pub struct ColourCache {
    spaces: HashMap<Key, Rc<ColourSpace>>,
    functions: HashMap<Key, Rc<Function>>,
}

impl ColourCache {
    /// The space a `cs`/`CS` operand names: a device family or Pattern by name, else the
    /// `/ColorSpace` resource of that name.
    pub fn named(
        &mut self,
        doc: &Document,
        resources: &Dictionary,
        name: &[u8],
    ) -> Rc<ColourSpace> {
        match name {
            b"DeviceGray" => return Rc::new(ColourSpace::DeviceGray),
            b"DeviceRGB" => return Rc::new(ColourSpace::DeviceRGB),
            b"DeviceCMYK" => return Rc::new(ColourSpace::DeviceCMYK),
            b"Pattern" => return Rc::new(ColourSpace::Pattern),
            _ => {}
        }
        let resource = objects::get_dict(doc, resources, b"ColorSpace")
            .and_then(|spaces| spaces.get(name).ok());
        match resource {
            Some(object) => self.space(doc, object, 0),
            None => Rc::new(ColourSpace::Unknown),
        }
    }

    /// A colour space object: a name or an array.
    pub fn space(&mut self, doc: &Document, object: &Object, depth: usize) -> Rc<ColourSpace> {
        let Some((id, resolved)) = objects::resolve(doc, object) else {
            return Rc::new(ColourSpace::Unknown);
        };
        let key = Key::of(id, if id.is_some() { resolved } else { object });
        if let Some(space) = self.spaces.get(&key) {
            return space.clone();
        }
        let space = Rc::new(self.parse_space(doc, resolved, depth));
        self.spaces.insert(key, space.clone());
        space
    }

    fn parse_space(&mut self, doc: &Document, object: &Object, depth: usize) -> ColourSpace {
        if depth > MAX_SPACE_DEPTH {
            return ColourSpace::Unknown;
        }
        let (family, items) = match object {
            Object::Name(name) => (name.as_slice(), &[][..]),
            Object::Array(items) => match items
                .first()
                .and_then(|first| objects::resolved(doc, first))
            {
                Some(Object::Name(name)) => (name.as_slice(), &items[1..]),
                _ => return ColourSpace::Unknown,
            },
            _ => return ColourSpace::Unknown,
        };
        let item = |index: usize| items.get(index);
        match family {
            b"DeviceGray" | b"G" => ColourSpace::DeviceGray,
            b"DeviceRGB" | b"RGB" => ColourSpace::DeviceRGB,
            b"DeviceCMYK" | b"CMYK" => ColourSpace::DeviceCMYK,
            b"CalGray" => ColourSpace::CalGray,
            b"CalRGB" => ColourSpace::CalRGB,
            b"Lab" => ColourSpace::Lab,
            b"Pattern" => ColourSpace::Pattern,
            b"ICCBased" => {
                let n = item(0)
                    .and_then(|stream| objects::resolved(doc, stream))
                    .and_then(objects::as_dict)
                    .and_then(|dict| objects::get_int(doc, dict, b"N"));
                match n {
                    Some(n @ (1 | 3 | 4)) => ColourSpace::IccBased { n: n as u8 },
                    _ => ColourSpace::IccBased { n: 0 },
                }
            }
            b"Indexed" | b"I" => {
                let base = match item(0) {
                    Some(base) => self.space(doc, base, depth + 1),
                    None => return ColourSpace::Unknown,
                };
                let hival = match item(1).and_then(|hival| objects::resolved(doc, hival)) {
                    Some(Object::Integer(hival)) => u32::try_from(*hival).ok(),
                    _ => None,
                };
                let lookup: Option<Rc<[u8]>> =
                    match item(2).and_then(|lookup| objects::resolved(doc, lookup)) {
                        Some(Object::String(bytes, _)) => Some(bytes.as_slice().into()),
                        Some(Object::Stream(stream)) => Some(get_contents(stream).into()),
                        _ => None,
                    };
                match (hival, lookup) {
                    (Some(hival), Some(lookup)) => ColourSpace::Indexed {
                        base,
                        hival,
                        lookup,
                    },
                    _ => ColourSpace::Unknown,
                }
            }
            b"Separation" => {
                let colorant = match item(0).and_then(|name| objects::resolved(doc, name)) {
                    Some(Object::Name(name)) => name.clone(),
                    _ => return ColourSpace::Unknown,
                };
                let (Some(alternate), Some(tint)) = (item(1), item(2)) else {
                    return ColourSpace::Unknown;
                };
                ColourSpace::Separation {
                    colorant,
                    alternate: self.space(doc, alternate, depth + 1),
                    tint: self.function(doc, tint),
                }
            }
            b"DeviceN" => {
                let colorants: Option<Vec<Vec<u8>>> =
                    match item(0).and_then(|names| objects::resolved(doc, names)) {
                        Some(Object::Array(names))
                            if !names.is_empty() && names.len() <= MAX_COMPONENTS =>
                        {
                            names
                                .iter()
                                .map(|name| match objects::resolved(doc, name) {
                                    Some(Object::Name(name)) => Some(name.clone()),
                                    _ => None,
                                })
                                .collect()
                        }
                        _ => None,
                    };
                let (Some(colorants), Some(alternate), Some(tint)) = (colorants, item(1), item(2))
                else {
                    return ColourSpace::Unknown;
                };
                ColourSpace::DeviceN {
                    colorants,
                    alternate: self.space(doc, alternate, depth + 1),
                    tint: self.function(doc, tint),
                }
            }
            _ => ColourSpace::Unknown,
        }
    }

    pub fn function(&mut self, doc: &Document, object: &Object) -> Rc<Function> {
        let Some((id, resolved)) = objects::resolve(doc, object) else {
            return Rc::new(Function::Unsupported);
        };
        let key = Key::of(id, if id.is_some() { resolved } else { object });
        if let Some(function) = self.functions.get(&key) {
            return function.clone();
        }
        let function = Rc::new(parse_function(doc, resolved));
        self.functions.insert(key, function.clone());
        function
    }
}

fn parse_function(doc: &Document, object: &Object) -> Function {
    let Some(dict) = objects::as_dict(object) else {
        return Function::Unsupported;
    };
    let numbers = |key: &[u8]| objects::get_numbers(doc, dict, key);
    match objects::get_int(doc, dict, b"FunctionType") {
        Some(2) => {
            let domain = match numbers(b"Domain").as_deref() {
                Some([lo, hi, ..]) => [*lo, *hi],
                _ => return Function::Unsupported,
            };
            let Some(n) = objects::get_number(doc, dict, b"N") else {
                return Function::Unsupported;
            };
            Function::Exponential {
                domain,
                c0: numbers(b"C0").unwrap_or_else(|| vec![0.0]),
                c1: numbers(b"C1").unwrap_or_else(|| vec![1.0]),
                n,
                range: numbers(b"Range"),
            }
        }
        Some(0) => {
            let Object::Stream(stream) = object else {
                return Function::Unsupported;
            };
            let size: Option<Vec<u32>> = objects::get_array(doc, dict, b"Size").and_then(|items| {
                items
                    .iter()
                    .map(|item| match objects::resolved(doc, item) {
                        Some(Object::Integer(size)) => u32::try_from(*size).ok(),
                        _ => None,
                    })
                    .collect()
            });
            let (Some(domain), Some(range), Some(size), Some(bits)) = (
                numbers(b"Domain"),
                numbers(b"Range"),
                size,
                objects::get_int(doc, dict, b"BitsPerSample")
                    .and_then(|bits| u32::try_from(bits).ok()),
            ) else {
                return Function::Unsupported;
            };
            let encode = numbers(b"Encode").unwrap_or_else(|| {
                size.iter()
                    .flat_map(|size| [0.0, f64::from(size.saturating_sub(1))])
                    .collect()
            });
            let decode = numbers(b"Decode").unwrap_or_else(|| range.clone());
            Function::Sampled {
                order: objects::get_int(doc, dict, b"Order")
                    .and_then(|order| u32::try_from(order).ok())
                    .unwrap_or(1),
                domain,
                range,
                size,
                bits_per_sample: bits,
                encode,
                decode,
                samples: get_contents(stream).into(),
            }
        }
        _ => Function::Unsupported,
    }
}
