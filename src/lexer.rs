//! A PostScript-style tokenizer for CMaps and Type 1 font programs. Every call consumes at
//! least one byte and nothing recurses, so any input is read in linear time: nested brackets,
//! parentheses and dictionaries are plain tokens here, never a stack.

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Hex(Vec<u8>),
    /// Built by the CMap parser from `[ <..> <..> ]`, never by the lexer.
    HexArray(Vec<Vec<u8>>),
    Name(Vec<u8>),
    Integer(i64),
    Real(f64),
    Keyword(Vec<u8>),
    /// A literal string; its bytes are not kept.
    Str,
    ArrayStart,
    ArrayEnd,
    DictStart,
    DictEnd,
    Other,
}

pub struct Lexer<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn is_white(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl<'a> Lexer<'a> {
    pub fn new(bytes: &'a [u8]) -> Lexer<'a> {
        Lexer { bytes, at: 0 }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.at.checked_add(offset)?).copied()
    }

    /// The bytes from the current offset while `keep` holds.
    fn take_while(&mut self, keep: impl Fn(u8) -> bool) -> &'a [u8] {
        let start = self.at;
        while let Some(byte) = self.peek(0) {
            if !keep(byte) {
                break;
            }
            self.at += 1;
        }
        &self.bytes[start..self.at]
    }

    pub fn next_token(&mut self) -> Option<Token> {
        loop {
            let byte = self.peek(0)?;
            if is_white(byte) {
                self.at += 1;
                continue;
            }
            if byte == b'%' {
                self.take_while(|byte| byte != b'\n' && byte != b'\r');
                continue;
            }
            break;
        }
        let byte = self.peek(0)?;
        self.at += 1;
        Some(match byte {
            b'<' if self.peek(0) == Some(b'<') => {
                self.at += 1;
                Token::DictStart
            }
            b'>' if self.peek(0) == Some(b'>') => {
                self.at += 1;
                Token::DictEnd
            }
            b'<' => {
                let digits = self.take_while(|byte| byte != b'>');
                if self.peek(0) == Some(b'>') {
                    self.at += 1;
                }
                let nibbles: Vec<u8> = digits.iter().filter_map(|byte| hex_value(*byte)).collect();
                Token::Hex(
                    nibbles
                        .chunks(2)
                        .map(|pair| (pair[0] << 4) | pair.get(1).copied().unwrap_or(0))
                        .collect(),
                )
            }
            b'(' => {
                let mut depth = 1usize;
                while let Some(byte) = self.peek(0) {
                    self.at += 1;
                    match byte {
                        b'\\' => self.at = self.at.saturating_add(1).min(self.bytes.len()),
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Token::Str
            }
            b'[' => Token::ArrayStart,
            b']' => Token::ArrayEnd,
            b'/' => {
                let raw = self.take_while(|byte| !is_white(byte) && !is_delimiter(byte));
                Token::Name(decode_name(raw))
            }
            b'{' | b'}' | b')' | b'>' => Token::Other,
            _ => {
                self.at -= 1;
                let word = self.take_while(|byte| !is_white(byte) && !is_delimiter(byte));
                if word.is_empty() {
                    self.at += 1;
                    return Some(Token::Other);
                }
                number(word).unwrap_or_else(|| Token::Keyword(word.to_vec()))
            }
        })
    }
}

/// A name's bytes with `#xx` escapes decoded.
pub fn decode_name(raw: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'#' {
            if let (Some(high), Some(low)) = (
                raw.get(index + 1).copied().and_then(hex_value),
                raw.get(index + 2).copied().and_then(hex_value),
            ) {
                name.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        name.push(raw[index]);
        index += 1;
    }
    name
}

fn number(word: &[u8]) -> Option<Token> {
    let text = std::str::from_utf8(word).ok()?;
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty()
        || !digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    if digits.contains('.') {
        let value: f64 = text.parse().ok()?;
        value.is_finite().then_some(Token::Real(value))
    } else {
        text.parse::<i64>().ok().map(Token::Integer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &[u8]) -> Vec<Token> {
        let mut lexer = Lexer::new(input);
        std::iter::from_fn(|| lexer.next_token()).collect()
    }

    #[test]
    fn reads_the_token_kinds() {
        assert_eq!(
            tokens(b"<00 1f> /A#42 12 -1.5 dup (a(b)c\\)) [ ] << >> % note\n{x}"),
            vec![
                Token::Hex(vec![0x00, 0x1f]),
                Token::Name(b"AB".to_vec()),
                Token::Integer(12),
                Token::Real(-1.5),
                Token::Keyword(b"dup".to_vec()),
                Token::Str,
                Token::ArrayStart,
                Token::ArrayEnd,
                Token::DictStart,
                Token::DictEnd,
                Token::Other,
                Token::Keyword(b"x".to_vec()),
                Token::Other,
            ]
        );
    }

    #[test]
    fn unterminated_input_ends() {
        assert_eq!(tokens(b"(((("), vec![Token::Str]);
        assert_eq!(tokens(b"<0"), vec![Token::Hex(vec![0])]);
        assert!(tokens(&vec![b'['; 100_000]).len() == 100_000);
    }
}
