//! Reading lopdf objects without panicking: every lookup resolves references with lopdf's
//! bounded `dereference` and answers `None` for a missing key, a dangling reference or a value
//! of the wrong type.
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::test_hooks;

/// `object` with references followed, and the id of the last one.
pub fn resolve<'a>(
    doc: &'a Document,
    object: &'a Object,
) -> Option<(Option<ObjectId>, &'a Object)> {
    doc.dereference(object).ok()
}

pub fn resolved<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    resolve(doc, object).map(|(_, object)| object)
}

pub fn get<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    resolved(doc, dict.get(key).ok()?)
}

/// A dictionary value, or a stream's dictionary.
pub fn as_dict(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(dict) => Some(dict),
        Object::Stream(stream) => Some(&stream.dict),
        _ => None,
    }
}

pub fn get_dict<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    match get(doc, dict, key)? {
        Object::Dictionary(dict) => Some(dict),
        _ => None,
    }
}

pub fn get_stream<'a>(
    doc: &'a Document,
    dict: &'a Dictionary,
    key: &[u8],
) -> Option<(Option<ObjectId>, &'a Stream)> {
    match resolve(doc, dict.get(key).ok()?)? {
        (id, Object::Stream(stream)) => Some((id, stream)),
        _ => None,
    }
}

pub fn get_name<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    match get(doc, dict, key)? {
        Object::Name(name) => Some(name),
        _ => None,
    }
}

pub fn get_array<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [Object]> {
    match get(doc, dict, key)? {
        Object::Array(items) => Some(items),
        _ => None,
    }
}

/// A finite number.
pub fn number(object: &Object) -> Option<f64> {
    let value = match object {
        Object::Integer(value) => *value as f64,
        Object::Real(value) => f64::from(*value),
        _ => return None,
    };
    value.is_finite().then_some(value)
}

pub fn get_number(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<f64> {
    number(get(doc, dict, key)?)
}

pub fn get_int(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    match get(doc, dict, key)? {
        Object::Integer(value) => Some(*value),
        _ => None,
    }
}

pub fn get_bool(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<bool> {
    match get(doc, dict, key)? {
        Object::Boolean(value) => Some(*value),
        _ => None,
    }
}

/// Every item of `items` as a number, resolving references, or `None` if one is not.
pub fn numbers(doc: &Document, items: &[Object]) -> Option<Vec<f64>> {
    items
        .iter()
        .map(|item| resolved(doc, item).and_then(number))
        .collect()
}

pub fn get_numbers(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Vec<f64>> {
    numbers(doc, get_array(doc, dict, key)?)
}

/// A stream's bytes. This crate never decompresses: a stream that still has a `/Filter` here
/// is a caller's mistake (the caller decodes every stream first), so it is counted and is a
/// panic, never a silent decode.
pub fn get_contents(stream: &Stream) -> &[u8] {
    test_hooks::count_stream();
    if stream.dict.has(b"Filter") {
        test_hooks::count_filtered();
        panic!("pdf-extract: filtered stream reached get_contents");
    }
    &stream.content
}

/// A key for caches of parsed objects: the object id when there is one, else the address of the
/// direct object inside its (immutable, borrowed) document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Id(ObjectId),
    Direct(usize),
}

impl Key {
    pub fn of(id: Option<ObjectId>, object: &Object) -> Key {
        match id {
            Some(id) => Key::Id(id),
            None => Key::Direct(object as *const Object as usize),
        }
    }
}
