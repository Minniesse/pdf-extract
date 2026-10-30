//! Optional content (ISO 32000-1 §8.11): which optional content groups the default
//! configuration shows, and whether a marked-content section, a Form or an image tagged with an
//! OCG or OCMD is drawn. Whatever this crate does not evaluate fails closed: a `/VE` expression
//! is `Unknown`, and a group a usage application names for a category other than View is off.
use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId};

use crate::objects;

/// Whether optional content lets something be drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Optional {
    Visible,
    Hidden,
    /// Inside an OCMD whose `/VE` visibility expression this crate does not evaluate.
    Unknown,
}

impl Optional {
    /// Both apply (nested optional content): hidden wins, then unknown.
    pub fn and(self, other: Optional) -> Optional {
        match (self, other) {
            (Optional::Hidden, _) | (_, Optional::Hidden) => Optional::Hidden,
            (Optional::Unknown, _) | (_, Optional::Unknown) => Optional::Unknown,
            _ => Optional::Visible,
        }
    }
}

/// The default configuration's off groups.
#[derive(Debug, Default)]
pub struct OptionalConfig {
    groups: HashSet<ObjectId>,
    off: HashSet<ObjectId>,
}

/// Groups listed in a config array, by id.
fn ids(doc: &Document, object: Option<&Object>) -> Vec<ObjectId> {
    let Some(Object::Array(items)) = object.and_then(|object| objects::resolved(doc, object))
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.as_reference().ok())
        .collect()
}

impl OptionalConfig {
    pub fn from_document(doc: &Document) -> OptionalConfig {
        let mut config = OptionalConfig::default();
        let Some(properties) = doc
            .catalog()
            .ok()
            .and_then(|catalog| objects::get_dict(doc, catalog, b"OCProperties"))
        else {
            return config;
        };
        config.groups = ids(doc, properties.get(b"OCGs").ok()).into_iter().collect();
        let Some(default) = objects::get_dict(doc, properties, b"D") else {
            return config;
        };
        if objects::get_name(doc, default, b"BaseState") == Some(b"OFF") {
            config.off = config.groups.clone();
        }
        for id in ids(doc, default.get(b"ON").ok()) {
            config.off.remove(&id);
        }
        for id in ids(doc, default.get(b"OFF").ok()) {
            config.off.insert(id);
        }
        // A group whose usage says it is not viewed is off, with or without an /AS entry
        // applying it.
        for id in config.groups.clone() {
            let state = doc
                .get_dictionary(id)
                .ok()
                .and_then(|group| objects::get_dict(doc, group, b"Usage"))
                .and_then(|usage| objects::get_dict(doc, usage, b"View"))
                .and_then(|view| objects::get_name(doc, view, b"ViewState"));
            if state == Some(b"OFF") {
                config.off.insert(id);
            }
        }
        // Usage applications for viewing under categories this crate does not evaluate (zoom,
        // language, user and the rest) leave their groups off.
        if let Some(applications) = objects::get_array(doc, default, b"AS") {
            for application in applications {
                let Some(application) =
                    objects::resolved(doc, application).and_then(objects::as_dict)
                else {
                    continue;
                };
                if objects::get_name(doc, application, b"Event") != Some(b"View") {
                    continue;
                }
                let other = objects::get_array(doc, application, b"Category").is_some_and(|categories| {
                    categories
                        .iter()
                        .any(|category| !matches!(objects::resolved(doc, category), Some(Object::Name(name)) if name == b"View"))
                });
                if other {
                    config.off.extend(ids(doc, application.get(b"OCGs").ok()));
                }
            }
        }
        config
    }

    fn group(&self, id: ObjectId) -> Optional {
        if self.off.contains(&id) {
            Optional::Hidden
        } else {
            Optional::Visible
        }
    }

    /// An `/OC` value or a `/Properties` entry: an OCG, an OCMD, or anything else (no effect).
    pub fn evaluate(&self, doc: &Document, object: &Object) -> Optional {
        let Some((id, resolved)) = objects::resolve(doc, object) else {
            return Optional::Visible;
        };
        let Some(dict) = objects::as_dict(resolved) else {
            return Optional::Visible;
        };
        if let Some(id) = id {
            if self.off.contains(&id) {
                return Optional::Hidden;
            }
        }
        let kind = objects::get_name(doc, dict, b"Type");
        let membership = kind == Some(b"OCMD") || dict.has(b"OCGs") || dict.has(b"VE");
        if !membership {
            return Optional::Visible;
        }
        if dict.has(b"VE") {
            return Optional::Unknown;
        }
        let members: Vec<ObjectId> = match dict.get(b"OCGs").ok() {
            Some(Object::Reference(id)) => vec![*id],
            other => ids(doc, other),
        };
        let states: Vec<bool> = members
            .iter()
            .filter(|id| doc.get_dictionary(**id).is_ok())
            .map(|id| self.group(*id) == Optional::Visible)
            .collect();
        // An OCMD with no usable groups has no effect on visibility (Table 99).
        if states.is_empty() {
            return Optional::Visible;
        }
        let on = match objects::get_name(doc, dict, b"P") {
            Some(b"AllOn") => states.iter().all(|on| *on),
            Some(b"AnyOff") => states.iter().any(|on| !*on),
            Some(b"AllOff") => states.iter().all(|on| !*on),
            _ => states.iter().any(|on| *on),
        };
        if on {
            Optional::Visible
        } else {
            Optional::Hidden
        }
    }
}
