//! The built-in encoding of an embedded Type 1 font program (`/FontFile`), read from its
//! clear-text part with the bounded lexer: `/Encoding StandardEncoding def`, or an
//! `/Encoding 256 array ... dup <code> /<glyph> put ... def` table. Nothing here recurses or
//! reads past `eexec`.
use crate::lexer::{Lexer, Token};

/// Tokens read before giving up on finding the encoding.
const MAX_TOKENS: usize = 1 << 20;

#[derive(Debug, PartialEq)]
pub enum BuiltinEncoding {
    Standard,
    /// Glyph names by code.
    Table(Vec<(u8, Vec<u8>)>),
}

pub fn builtin_encoding(program: &[u8]) -> Option<BuiltinEncoding> {
    let mut lexer = Lexer::new(program);
    let mut tokens = 0usize;
    let mut next = |lexer: &mut Lexer| {
        tokens += 1;
        if tokens > MAX_TOKENS {
            return None;
        }
        match lexer.next_token()? {
            Token::Keyword(word) if word == b"eexec" => None,
            token => Some(token),
        }
    };
    loop {
        if next(&mut lexer)? == Token::Name(b"Encoding".to_vec()) {
            break;
        }
    }
    let mut table = Vec::new();
    // The last three tokens, for the `dup <code> /<name> put` pattern.
    let mut window: Vec<Token> = Vec::with_capacity(4);
    loop {
        let token = next(&mut lexer);
        match &token {
            Some(Token::Keyword(word)) if word == b"StandardEncoding" && table.is_empty() => {
                return Some(BuiltinEncoding::Standard);
            }
            Some(Token::Keyword(word)) if word == b"put" => {
                if let [Token::Keyword(dup), Token::Integer(code), Token::Name(name)] =
                    window.as_slice()
                {
                    if dup == b"dup" {
                        if let Ok(code) = u8::try_from(*code) {
                            table.push((code, name.clone()));
                        }
                    }
                }
            }
            Some(Token::Keyword(word))
                if (word == b"def" || word == b"readonly") && !table.is_empty() =>
            {
                break;
            }
            None => break,
            _ => {}
        }
        let Some(token) = token else { break };
        window.push(token);
        if window.len() > 3 {
            window.remove(0);
        }
    }
    if table.is_empty() {
        None
    } else {
        Some(BuiltinEncoding::Table(table))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_forms() {
        assert_eq!(
            builtin_encoding(
                b"%!PS\n/FontName /X def /Encoding StandardEncoding def\neexec\x80\x81"
            ),
            Some(BuiltinEncoding::Standard)
        );
        assert_eq!(
            builtin_encoding(
                b"/Encoding 256 array 0 1 255 {1 index exch /.notdef put} for \
                  dup 65 /A put dup 300 /B put dup 66 /uni0E01 put readonly def"
            ),
            Some(BuiltinEncoding::Table(vec![
                (65, b"A".to_vec()),
                (66, b"uni0E01".to_vec())
            ]))
        );
    }

    #[test]
    fn nesting_and_garbage_end() {
        let mut program = b"/Encoding ".to_vec();
        program.extend(vec![b'{'; 200_000]);
        program.extend(vec![b'['; 200_000]);
        assert_eq!(builtin_encoding(&program), None);
        assert_eq!(builtin_encoding(b"eexec /Encoding StandardEncoding"), None);
    }
}
