//! A bounded parser for the CMaps a PDF embeds: `/ToUnicode` maps and the `/Encoding` CMap of
//! a Type 0 font. It reads tokens one at a time with no recursion, keeps `bfrange` and
//! `cidrange` entries as ranges instead of expanding them, and stops adding mappings past
//! fixed caps, so a hostile CMap costs time and memory linear in its length.
use std::collections::BTreeMap;

use crate::glyphnames;
use crate::lexer::{Lexer, Token};

/// Explicit code mappings (`bfchar`, `cidchar` and array `bfrange` entries) one CMap may hold.
pub const MAX_MAPPINGS: usize = 262_144;
/// `bfrange`, `cidrange` and `codespacerange` entries one CMap may hold.
pub const MAX_RANGES: usize = 65_536;
/// UTF-16 code units one destination string may hold.
const MAX_DESTINATION_UNITS: usize = 256;

/// One codespace range: a code of `len` bytes whose every byte lies between the matching bytes
/// of `lo` and `hi`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSpace {
    pub len: u8,
    pub lo: [u8; 4],
    pub hi: [u8; 4],
}

impl CodeSpace {
    fn matches(&self, bytes: &[u8]) -> bool {
        let len = usize::from(self.len);
        bytes.len() >= len
            && (0..len)
                .all(|index| self.lo[index] <= bytes[index] && bytes[index] <= self.hi[index])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BfRange {
    lo: u32,
    hi: u32,
    /// The destination of `lo`; each later code adds one to its last unit.
    base: Vec<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CMap {
    pub codespace: Vec<CodeSpace>,
    bf_chars: BTreeMap<u32, String>,
    /// Sorted by `lo` once parsing ends.
    bf_ranges: Vec<BfRange>,
    cid_chars: BTreeMap<u32, u32>,
    /// `(lo, hi, cid of lo)`, sorted by `lo` once parsing ends.
    cid_ranges: Vec<(u32, u32, u32)>,
}

/// A code's bytes as a number, or `None` past four bytes.
fn code_of(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0u32, |code, byte| (code << 8) | u32::from(*byte)),
    )
}

fn units_of(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .take(MAX_DESTINATION_UNITS)
        .map(|pair| match pair {
            [high, low] => u16::from_be_bytes([*high, *low]),
            [single] => u16::from(*single),
            _ => 0xFFFD,
        })
        .collect()
}

/// UTF-16 to a string; a lone surrogate becomes U+FFFD.
pub fn utf16_lossy(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

impl CMap {
    pub fn parse(bytes: &[u8]) -> CMap {
        let mut cmap = CMap::default();
        let mut lexer = Lexer::new(bytes);
        let mut mappings = 0usize;
        // The section being read and the operands collected for its next entry.
        let mut section: Option<&'static str> = None;
        let mut operands: Vec<Token> = Vec::new();
        let mut array: Option<Vec<Vec<u8>>> = None;
        while let Some(token) = lexer.next_token() {
            if let Some(items) = array.as_mut() {
                match token {
                    Token::ArrayEnd => {
                        let items = array.take().unwrap_or_default();
                        operands.push(Token::HexArray(items));
                    }
                    Token::Hex(bytes) if items.len() < MAX_MAPPINGS => items.push(bytes),
                    _ => {}
                }
            } else {
                match token {
                    Token::Keyword(word) => {
                        match word.as_slice() {
                            b"begincodespacerange" => section = Some("codespace"),
                            b"beginbfchar" => section = Some("bfchar"),
                            b"beginbfrange" => section = Some("bfrange"),
                            b"begincidchar" => section = Some("cidchar"),
                            b"begincidrange" => section = Some("cidrange"),
                            b"endcodespacerange" | b"endbfchar" | b"endbfrange" | b"endcidchar"
                            | b"endcidrange" => section = None,
                            _ => {}
                        }
                        operands.clear();
                    }
                    Token::ArrayStart => array = Some(Vec::new()),
                    Token::Hex(_) | Token::Name(_) | Token::Integer(_) => operands.push(token),
                    _ => operands.clear(),
                }
            }
            let Some(kind) = section else {
                if operands.len() > 3 {
                    operands.remove(0);
                }
                continue;
            };
            let want = match kind {
                "codespace" | "bfchar" | "cidchar" => 2,
                _ => 3,
            };
            if operands.len() < want {
                continue;
            }
            let entry: Vec<Token> = std::mem::take(&mut operands);
            cmap.add(kind, entry, &mut mappings);
        }
        cmap.bf_ranges.sort_by_key(|range| range.lo);
        cmap.cid_ranges.sort_by_key(|range| range.0);
        cmap
    }

    fn add(&mut self, kind: &str, entry: Vec<Token>, mappings: &mut usize) {
        let ranges = self.codespace.len() + self.bf_ranges.len() + self.cid_ranges.len();
        match (kind, entry.as_slice()) {
            ("codespace", [Token::Hex(lo), Token::Hex(hi)]) => {
                if ranges >= MAX_RANGES || lo.len() != hi.len() || lo.is_empty() || lo.len() > 4 {
                    return;
                }
                let mut space = CodeSpace {
                    len: lo.len() as u8,
                    lo: [0; 4],
                    hi: [0; 4],
                };
                space.lo[..lo.len()].copy_from_slice(lo);
                space.hi[..hi.len()].copy_from_slice(hi);
                self.codespace.push(space);
            }
            ("bfchar", [Token::Hex(src), destination]) => {
                let Some(code) = code_of(src) else { return };
                let text = match destination {
                    Token::Hex(bytes) => utf16_lossy(&units_of(bytes)),
                    Token::Name(name) => match glyph_name_text(name) {
                        Some(text) => text,
                        None => return,
                    },
                    _ => return,
                };
                if *mappings < MAX_MAPPINGS {
                    *mappings += 1;
                    self.bf_chars.insert(code, text);
                }
            }
            ("bfrange", [Token::Hex(lo), Token::Hex(hi), Token::Hex(base)]) => {
                let (Some(lo), Some(hi)) = (code_of(lo), code_of(hi)) else {
                    return;
                };
                if lo > hi || ranges >= MAX_RANGES {
                    return;
                }
                let base = units_of(base);
                if base.is_empty() {
                    return;
                }
                self.bf_ranges.push(BfRange { lo, hi, base });
            }
            ("bfrange", [Token::Hex(lo), Token::Hex(hi), Token::HexArray(items)]) => {
                let (Some(lo), Some(hi)) = (code_of(lo), code_of(hi)) else {
                    return;
                };
                if lo > hi {
                    return;
                }
                for (offset, bytes) in items.iter().enumerate() {
                    let Some(code) = u32::try_from(offset)
                        .ok()
                        .and_then(|offset| lo.checked_add(offset))
                    else {
                        break;
                    };
                    if code > hi || *mappings >= MAX_MAPPINGS {
                        break;
                    }
                    *mappings += 1;
                    self.bf_chars.insert(code, utf16_lossy(&units_of(bytes)));
                }
            }
            ("cidchar", [Token::Hex(src), Token::Integer(cid)]) => {
                let (Some(code), Ok(cid)) = (code_of(src), u32::try_from(*cid)) else {
                    return;
                };
                if *mappings < MAX_MAPPINGS {
                    *mappings += 1;
                    self.cid_chars.insert(code, cid);
                }
            }
            ("cidrange", [Token::Hex(lo), Token::Hex(hi), Token::Integer(cid)]) => {
                let (Some(lo), Some(hi), Ok(cid)) = (code_of(lo), code_of(hi), u32::try_from(*cid))
                else {
                    return;
                };
                if lo > hi || ranges >= MAX_RANGES {
                    return;
                }
                self.cid_ranges.push((lo, hi, cid));
            }
            _ => {}
        }
    }

    /// The text a `/ToUnicode` map gives `code`, or `None` when it holds no entry for it.
    pub fn unicode(&self, code: u32) -> Option<String> {
        if let Some(text) = self.bf_chars.get(&code) {
            return Some(text.clone());
        }
        let index = self.bf_ranges.partition_point(|range| range.lo <= code);
        let range = self.bf_ranges[..index]
            .iter()
            .rev()
            .find(|range| code <= range.hi)?;
        let offset = code - range.lo;
        let mut units = range.base.clone();
        let last = units.last_mut()?;
        let shifted = u32::from(*last).checked_add(offset)?;
        *last = u16::try_from(shifted).ok()?;
        Some(utf16_lossy(&units))
    }

    /// The CID an encoding CMap gives `code`.
    pub fn cid(&self, code: u32) -> Option<u32> {
        if let Some(cid) = self.cid_chars.get(&code) {
            return Some(*cid);
        }
        let index = self.cid_ranges.partition_point(|range| range.0 <= code);
        let (lo, _, cid) = self.cid_ranges[..index]
            .iter()
            .rev()
            .find(|range| code <= range.1)?;
        cid.checked_add(code - lo)
    }

    /// The next code in `bytes` from `at`, and its length: the shortest codespace range that
    /// matches, else the length of the first range whose first byte matches, else one byte. A
    /// CMap with no codespace reads two-byte codes.
    pub fn next_code(&self, bytes: &[u8], at: usize) -> Option<(u32, usize)> {
        let rest = bytes.get(at..)?;
        if rest.is_empty() {
            return None;
        }
        if self.codespace.is_empty() {
            let len = rest.len().min(2);
            return code_of(&rest[..len]).map(|code| (code, len));
        }
        for len in 1..=4u8 {
            if self
                .codespace
                .iter()
                .any(|space| space.len == len && space.matches(rest))
            {
                let len = usize::from(len);
                return code_of(&rest[..len]).map(|code| (code, len));
            }
        }
        let len = self
            .codespace
            .iter()
            .find(|space| space.lo[0] <= rest[0] && rest[0] <= space.hi[0])
            .map_or(1, |space| usize::from(space.len))
            .min(rest.len());
        code_of(&rest[..len]).map(|code| (code, len))
    }

    pub fn is_empty(&self) -> bool {
        self.bf_chars.is_empty() && self.bf_ranges.is_empty()
    }
}

/// The text of a glyph name: the Adobe glyph list, `uniXXXX` or `uXXXX` to `uXXXXXX`.
pub fn glyph_name_text(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    if let Some(unit) = glyphnames::name_to_unicode(name) {
        return Some(utf16_lossy(&[unit]));
    }
    let hex = |digits: &str| -> Option<u32> {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok()
    };
    if let Some(digits) = name.strip_prefix("uni") {
        if digits.len() == 4 {
            return hex(digits).and_then(char::from_u32).map(String::from);
        }
    }
    if let Some(digits) = name.strip_prefix('u') {
        if (4..=6).contains(&digits.len()) {
            return hex(digits).and_then(char::from_u32).map(String::from);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bfchar_bfrange_and_arrays() {
        let cmap = CMap::parse(
            b"1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
              2 beginbfchar <0001> <0E010E33> <0002> <> endbfchar\n\
              1 beginbfrange <0010> <0012> <0041> endbfrange\n\
              1 beginbfrange <0020> <0021> [<0061> <0062>] endbfrange",
        );
        assert_eq!(cmap.unicode(1).as_deref(), Some("\u{E01}\u{E33}"));
        assert_eq!(cmap.unicode(2).as_deref(), Some(""));
        assert_eq!(cmap.unicode(0x11).as_deref(), Some("B"));
        assert_eq!(cmap.unicode(0x21).as_deref(), Some("b"));
        assert_eq!(cmap.unicode(0x13), None);
        assert_eq!(cmap.next_code(&[0, 1, 0, 2], 2), Some((2, 2)));
    }

    #[test]
    fn a_range_bomb_is_one_range() {
        let cmap = CMap::parse(b"1 beginbfrange <00000000> <FFFFFFFF> <0041> endbfrange");
        assert_eq!(cmap.bf_ranges.len(), 1);
        assert_eq!(cmap.unicode(1).as_deref(), Some("B"));
        assert_eq!(cmap.unicode(0xFFFF_FFFF), None);
    }

    #[test]
    fn nesting_does_not_recurse() {
        let mut bomb = vec![b'['; 200_000];
        bomb.extend(vec![b'('; 200_000]);
        bomb.extend(vec![b'<'; 200_000]);
        let cmap = CMap::parse(&bomb);
        assert!(cmap.is_empty());
    }

    #[test]
    fn cid_ranges_and_mixed_codespaces() {
        let cmap = CMap::parse(
            b"2 begincodespacerange <00> <80> <8140> <9FFC> endcodespacerange\n\
              1 begincidrange <8140> <817E> 633 endcidrange 1 begincidchar <20> 1 endcidchar",
        );
        assert_eq!(cmap.next_code(&[0x20, 0x81, 0x41], 0), Some((0x20, 1)));
        assert_eq!(cmap.next_code(&[0x20, 0x81, 0x41], 1), Some((0x8141, 2)));
        assert_eq!(cmap.cid(0x8141), Some(634));
        assert_eq!(cmap.cid(0x20), Some(1));
    }
}
