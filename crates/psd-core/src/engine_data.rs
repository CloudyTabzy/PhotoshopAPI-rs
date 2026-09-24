//! EngineData parser (`Core/Struct/EngineDataStructure.{h,cpp}`).
//!
//! EngineData is the PostScript-flavoured text format inside text-layer
//! (`TySh`) descriptors: `<< /Key value … >>` dictionaries, `[ … ]` arrays,
//! `/Names`, `(literal strings)`, `<hex>`, numbers, booleans, bare
//! identifiers, `%` line comments.
//!
//! Every parsed [`EngineValue`] records the byte span it occupied in the
//! payload it was parsed from, so edits can **splice raw payload bytes**
//! without a full re-serialization. Use
//! [`apply_patches`]/[`insert_dict_entry_bytes`]/[`insert_array_item_bytes`]
//! for that; offsets are always relative to the slice passed to [`parse`].
//!
//! Number handling keeps Photoshop's integer/float distinction (`16` vs
//! `16.0`) exactly like upstream: [`EngineNumber::integer`] is `Some` when the
//! token was an integer literal, and [`EngineValue::set_number`] preserves the
//! original representation.
//!
//! One deliberate deviation: upstream parses numbers with `strtod`, which
//! accepts C hex-float tokens like `0x10`; this port uses Rust's `str::parse`,
//! so such tokens classify as identifiers (upstream quirk, no real payload
//! contains hex floats).

use std::ops::Range;

use crate::error::{PsdError, Result};

/// A number with its literal representation preserved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineNumber {
    /// The numeric value.
    pub value: f64,
    /// `Some` when the token was an integer literal (`16`, not `16.0`).
    pub integer: Option<i64>,
}

impl EngineNumber {
    /// Parse a bare token; `None` when it is not a finite decimal number.
    fn parse_token(token: &str) -> Option<Self> {
        if token.is_empty() || !token.bytes().any(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: f64 = token.parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        // Integer literals carry no fraction or exponent marker.
        let integer = if token.contains(['.', 'e', 'E']) {
            None
        } else {
            token.parse::<i64>().ok()
        };
        Some(Self { value, integer })
    }
}

/// The typed payload of an [`EngineValue`].
#[derive(Debug, Clone, PartialEq)]
pub enum EngineValueKind {
    /// `<< /Key value … >>` (insertion-ordered).
    Dictionary(Vec<(String, EngineValue)>),
    Array(Vec<EngineValue>),
    /// `/Name`.
    Name(String),
    Number(EngineNumber),
    Boolean(bool),
    /// `(…)` with paren nesting and backslash escapes understood.
    LiteralString(String),
    /// A string value created from logical UTF-8 text (not pre-escaped
    /// PostScript source). Formatting escapes its syntax characters.
    LiteralStringValue(String),
    /// A literal string whose source bytes are not UTF-8. Photoshop stores
    /// UTF-16BE text directly inside EngineData literal strings, so these
    /// bytes must not pass through lossy UTF-8 conversion.
    LiteralStringBytes(Vec<u8>),
    /// `<…>`.
    HexString(String),
    /// A bare word that is none of the above (e.g. an enum-like token).
    Identifier(String),
}

/// One EngineData value plus its byte span in the source payload.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineValue {
    /// Byte range `[start, end)` of this value in the parsed payload.
    pub span: Range<usize>,
    pub kind: EngineValueKind,
}

impl EngineValue {
    // ------------------------------------------------------------------
    // Factories
    // ------------------------------------------------------------------

    pub fn number(value: f64) -> Self {
        let integer = (value.is_finite()
            && (value.round() - value).abs() <= 1e-6
            && value.round() >= i64::MIN as f64
            && value.round() <= i64::MAX as f64)
            .then(|| value.round() as i64);
        Self::raw(EngineValueKind::Number(EngineNumber { value, integer }))
    }

    /// A number that always serializes with a decimal point (`1.0`, not `1`),
    /// the spelling Photoshop uses for colors, spacing arrays, and metrics.
    pub fn float(value: f64) -> Self {
        Self::raw(EngineValueKind::Number(EngineNumber {
            value,
            integer: None,
        }))
    }

    pub fn integer(value: i64) -> Self {
        Self::raw(EngineValueKind::Number(EngineNumber {
            value: value as f64,
            integer: Some(value),
        }))
    }

    pub fn boolean(value: bool) -> Self {
        Self::raw(EngineValueKind::Boolean(value))
    }

    pub fn name(value: impl Into<String>) -> Self {
        Self::raw(EngineValueKind::Name(value.into()))
    }

    pub fn string(value: impl Into<String>) -> Self {
        Self::raw(EngineValueKind::LiteralStringValue(value.into()))
    }

    /// Construct a literal string from its raw PostScript source contents
    /// (without the surrounding parentheses).
    pub fn literal_string_bytes(value: Vec<u8>) -> Self {
        Self::raw(EngineValueKind::LiteralStringBytes(value))
    }

    pub fn dict() -> Self {
        Self::raw(EngineValueKind::Dictionary(Vec::new()))
    }

    pub fn array() -> Self {
        Self::raw(EngineValueKind::Array(Vec::new()))
    }

    fn raw(kind: EngineValueKind) -> Self {
        Self { span: 0..0, kind }
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    /// First dictionary entry under `key`.
    pub fn get(&self, key: &str) -> Option<&EngineValue> {
        match &self.kind {
            EngineValueKind::Dictionary(items) => {
                items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut EngineValue> {
        match &mut self.kind {
            EngineValueKind::Dictionary(items) => {
                items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    /// Walk a path of dictionary keys (`get_path(&["EngineDict", "StyleRun"])`).
    pub fn get_path<'a>(&self, path: impl IntoIterator<Item = &'a str>) -> Option<&EngineValue> {
        let mut cursor = self;
        for key in path {
            cursor = cursor.get(key)?;
        }
        Some(cursor)
    }

    pub fn as_dictionary(&self) -> Option<&[(String, EngineValue)]> {
        match &self.kind {
            EngineValueKind::Dictionary(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[EngineValue]> {
        match &self.kind {
            EngineValueKind::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<EngineNumber> {
        match &self.kind {
            EngineValueKind::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub fn as_double(&self) -> Option<f64> {
        self.as_number().map(|number| number.value)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match &self.kind {
            EngineValueKind::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// Text of a literal string.
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            EngineValueKind::LiteralString(value) => Some(value),
            EngineValueKind::LiteralStringValue(value) => Some(value),
            _ => None,
        }
    }

    /// Raw PostScript source contents of a literal string, excluding its
    /// parentheses. This preserves non-UTF-8 payloads such as UTF-16BE text.
    pub fn as_literal_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            EngineValueKind::LiteralString(value) => Some(value.as_bytes()),
            EngineValueKind::LiteralStringValue(value) => Some(value.as_bytes()),
            EngineValueKind::LiteralStringBytes(value) => Some(value),
            _ => None,
        }
    }

    /// Text of a name (`/Name` without the slash).
    pub fn as_name(&self) -> Option<&str> {
        match &self.kind {
            EngineValueKind::Name(value) => Some(value),
            _ => None,
        }
    }

    /// All array items as `i32`, if this is an array of whole numbers.
    pub fn as_int32_vector(&self) -> Option<Vec<i32>> {
        let items = self.as_array()?;
        items
            .iter()
            .map(|item| {
                let number = item.as_number()?;
                let rounded = number.value.round();
                if !rounded.is_finite()
                    || (rounded - number.value).abs() > 1e-6
                    || rounded < i32::MIN as f64
                    || rounded > i32::MAX as f64
                {
                    return None;
                }
                Some(rounded as i32)
            })
            .collect()
    }

    /// All array items as `f64`, if this is an array of numbers.
    pub fn as_double_vector(&self) -> Option<Vec<f64>> {
        self.as_array()?
            .iter()
            .map(|item| item.as_double())
            .collect()
    }

    // ------------------------------------------------------------------
    // Mutation
    // ------------------------------------------------------------------

    /// Replace the numeric value, preserving the integer/float kind. Returns
    /// `false` for non-numbers or non-finite input.
    pub fn set_number(&mut self, number: f64) -> bool {
        if !number.is_finite() {
            return false;
        }
        let EngineValueKind::Number(target) = &mut self.kind else {
            return false;
        };
        let was_integer = target.integer;
        target.value = number;
        let rounded = number.round();
        target.integer = if was_integer.is_some()
            && (rounded - number).abs() <= 1e-6
            && rounded >= i64::MIN as f64
            && rounded <= i64::MAX as f64
        {
            Some(rounded as i64)
        } else {
            None
        };
        true
    }

    pub fn set_bool(&mut self, value: bool) -> bool {
        let EngineValueKind::Boolean(target) = &mut self.kind else {
            return false;
        };
        *target = value;
        true
    }

    pub fn set_name(&mut self, value: &str) -> bool {
        let EngineValueKind::Name(target) = &mut self.kind else {
            return false;
        };
        *target = value.to_string();
        true
    }

    pub fn set_string(&mut self, value: &str) -> bool {
        if !matches!(
            &self.kind,
            EngineValueKind::LiteralString(_)
                | EngineValueKind::LiteralStringValue(_)
                | EngineValueKind::LiteralStringBytes(_)
        ) {
            return false;
        }
        self.kind = EngineValueKind::LiteralStringValue(value.to_string());
        true
    }

    pub fn set_int32_array(&mut self, values: &[i32]) -> bool {
        let EngineValueKind::Array(target) = &mut self.kind else {
            return false;
        };
        *target = values
            .iter()
            .map(|&v| EngineValue::integer(i64::from(v)))
            .collect();
        true
    }

    pub fn set_double_array(&mut self, values: &[f64]) -> bool {
        if !values.iter().all(|v| v.is_finite()) {
            return false;
        }
        let EngineValueKind::Array(target) = &mut self.kind else {
            return false;
        };
        *target = values.iter().map(|&v| EngineValue::number(v)).collect();
        true
    }

    /// Insert or replace a dictionary entry. New keys must be safe ASCII
    /// identifiers so they serialize as one unambiguous EngineData Name token.
    pub fn insert(&mut self, key: &str, value: EngineValue) -> bool {
        let EngineValueKind::Dictionary(items) = &mut self.kind else {
            return false;
        };
        match items.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) => *slot = value,
            None if is_safe_name_token(key) => items.push((key.to_string(), value)),
            None => return false,
        }
        true
    }

    /// Remove a dictionary entry; `true` when the key existed.
    pub fn remove(&mut self, key: &str) -> bool {
        let EngineValueKind::Dictionary(items) = &mut self.kind else {
            return false;
        };
        match items.iter().position(|(k, _)| k == key) {
            Some(index) => {
                items.remove(index);
                true
            }
            None => false,
        }
    }

    /// Append an array item.
    pub fn push(&mut self, item: EngineValue) -> bool {
        let EngineValueKind::Array(items) = &mut self.kind else {
            return false;
        };
        items.push(item);
        true
    }
}

fn invalid(offset: usize, message: &'static str) -> PsdError {
    PsdError::InvalidData {
        offset: offset as u64,
        message,
    }
}

fn is_value_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(byte, b'[' | b']' | b'(' | b')' | b'<' | b'>' | b'/' | b'%')
}

fn is_safe_name_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn decode_name_token(source: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < source.len() {
        if source[cursor] == b'#' && cursor + 2 < source.len() {
            if let (Some(high), Some(low)) =
                (hex_digit(source[cursor + 1]), hex_digit(source[cursor + 2]))
            {
                decoded.push((high << 4) | low);
                cursor += 3;
                continue;
            }
        }
        decoded.push(source[cursor]);
        cursor += 1;
    }
    decoded
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn serialize_name_token(name: &str, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(b'/');
    for &byte in name.as_bytes() {
        if byte.is_ascii_graphic() && !is_value_delimiter(byte) && byte != b'#' {
            out.push(byte);
        } else {
            out.extend_from_slice(&[b'#', HEX[(byte >> 4) as usize], HEX[(byte & 0x0F) as usize]]);
        }
    }
}

struct Parser<'a> {
    data: &'a [u8],
    cursor: usize,
    error_offset: usize,
    error_message: Option<&'static str>,
}

/// Safety cap on EngineData container nesting. Real Photoshop text payloads
/// stay under ~30 levels; a hostile `<<<<<…` payload would otherwise recurse
/// until the stack overflows (a hardening fix).
const MAX_NESTING_DEPTH: usize = 256;

impl<'a> Parser<'a> {
    fn set_error(&mut self, message: &'static str) {
        if self.error_message.is_none() {
            self.error_offset = self.cursor;
            self.error_message = Some(message);
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        while self.cursor < self.data.len() {
            let byte = self.data[self.cursor];
            if byte.is_ascii_whitespace() {
                self.cursor += 1;
                continue;
            }
            if byte == b'%' {
                self.cursor += 1;
                while self.cursor < self.data.len() {
                    let c = self.data[self.cursor];
                    self.cursor += 1;
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                }
                continue;
            }
            break;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<EngineValue> {
        self.skip_whitespace_and_comments();
        let Some(&byte) = self.data.get(self.cursor) else {
            self.set_error("Unexpected end of EngineData payload");
            return Err(self.fail("Unexpected end of EngineData payload"));
        };
        match byte {
            b'<' if self.data.get(self.cursor + 1) == Some(&b'<') => self.parse_dictionary(depth),
            b'<' => self.parse_hex_string(),
            b'[' => self.parse_array(depth),
            b'(' => self.parse_literal_string(),
            b'/' => self.parse_name(),
            _ => self.parse_number_or_identifier(),
        }
    }

    fn check_nesting_depth(&self, depth: usize) -> Result<()> {
        if depth > MAX_NESTING_DEPTH {
            return Err(self.fail("EngineData nesting exceeds the safety limit"));
        }
        Ok(())
    }

    fn fail(&self, message: &'static str) -> PsdError {
        invalid(self.error_offset, self.error_message.unwrap_or(message))
    }

    fn parse_dictionary(&mut self, depth: usize) -> Result<EngineValue> {
        self.check_nesting_depth(depth)?;
        let start = self.cursor;
        if self.data.get(self.cursor) != Some(&b'<')
            || self.data.get(self.cursor + 1) != Some(&b'<')
        {
            self.set_error("Expected dictionary start token '<<'");
            return Err(self.fail("Expected dictionary start token '<<'"));
        }
        self.cursor += 2;

        let mut items = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.cursor >= self.data.len() {
                self.set_error("Unterminated dictionary in EngineData payload");
                return Err(self.fail("Unterminated dictionary in EngineData payload"));
            }
            if self.data.get(self.cursor) == Some(&b'>')
                && self.data.get(self.cursor + 1) == Some(&b'>')
            {
                self.cursor += 2;
                return Ok(EngineValue {
                    span: start..self.cursor,
                    kind: EngineValueKind::Dictionary(items),
                });
            }

            let key = self.parse_name()?;
            let EngineValueKind::Name(key) = key.kind else {
                self.set_error("Expected dictionary key name");
                return Err(self.fail("Expected dictionary key name"));
            };
            let value = self.parse_value(depth + 1)?;
            items.push((key, value));
        }
    }

    /// Parse `/Key value` pairs up to the end of the payload: a dictionary
    /// body without the enclosing `<<`/`>>` (the layout of `Txt2`).
    fn parse_dictionary_body(&mut self, depth: usize) -> Result<EngineValue> {
        self.check_nesting_depth(depth)?;
        let start = self.cursor;
        let mut items = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.cursor >= self.data.len() {
                return Ok(EngineValue {
                    span: start..self.cursor,
                    kind: EngineValueKind::Dictionary(items),
                });
            }
            let key = self.parse_name()?;
            let EngineValueKind::Name(key) = key.kind else {
                self.set_error("Expected dictionary key name");
                return Err(self.fail("Expected dictionary key name"));
            };
            let value = self.parse_value(depth + 1)?;
            items.push((key, value));
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<EngineValue> {
        self.check_nesting_depth(depth)?;
        let start = self.cursor;
        if self.data.get(self.cursor) != Some(&b'[') {
            self.set_error("Expected array start token '['");
            return Err(self.fail("Expected array start token '['"));
        }
        self.cursor += 1;

        let mut items = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.cursor >= self.data.len() {
                self.set_error("Unterminated array in EngineData payload");
                return Err(self.fail("Unterminated array in EngineData payload"));
            }
            if self.data[self.cursor] == b']' {
                self.cursor += 1;
                return Ok(EngineValue {
                    span: start..self.cursor,
                    kind: EngineValueKind::Array(items),
                });
            }
            items.push(self.parse_value(depth + 1)?);
        }
    }

    fn parse_name(&mut self) -> Result<EngineValue> {
        let start = self.cursor;
        if self.data.get(self.cursor) != Some(&b'/') {
            self.set_error("Expected name token starting with '/'");
            return Err(self.fail("Expected name token starting with '/'"));
        }
        self.cursor += 1;
        let value_start = self.cursor;
        while self.cursor < self.data.len() && !is_value_delimiter(self.data[self.cursor]) {
            self.cursor += 1;
        }
        let decoded = decode_name_token(&self.data[value_start..self.cursor]);
        let name = String::from_utf8_lossy(&decoded).into_owned();
        Ok(EngineValue {
            span: start..self.cursor,
            kind: EngineValueKind::Name(name),
        })
    }

    fn parse_literal_string(&mut self) -> Result<EngineValue> {
        let start = self.cursor;
        if self.data.get(self.cursor) != Some(&b'(') {
            self.set_error("Expected literal string starting with '('");
            return Err(self.fail("Expected literal string starting with '('"));
        }
        self.cursor += 1;
        let content_start = self.cursor;

        let mut escape = false;
        let mut nesting = 1i32;
        while self.cursor < self.data.len() {
            let byte = self.data[self.cursor];
            self.cursor += 1;

            if escape {
                escape = false;
                continue;
            }
            if byte == b'\\' {
                escape = true;
                continue;
            }
            if byte == b'(' {
                nesting += 1;
                continue;
            }
            if byte == b')' {
                nesting -= 1;
                if nesting == 0 {
                    let content = self.data[content_start..self.cursor - 1].to_vec();
                    let kind = match std::str::from_utf8(&content) {
                        Ok(value) => EngineValueKind::LiteralString(value.to_owned()),
                        Err(_) => EngineValueKind::LiteralStringBytes(content),
                    };
                    return Ok(EngineValue {
                        span: start..self.cursor,
                        kind,
                    });
                }
            }
        }

        self.set_error("Unterminated literal string in EngineData payload");
        Err(self.fail("Unterminated literal string in EngineData payload"))
    }

    fn parse_hex_string(&mut self) -> Result<EngineValue> {
        let start = self.cursor;
        if self.data.get(self.cursor) != Some(&b'<') {
            self.set_error("Expected hex string starting with '<'");
            return Err(self.fail("Expected hex string starting with '<'"));
        }
        self.cursor += 1;
        let content_start = self.cursor;

        while self.cursor < self.data.len() {
            if self.data[self.cursor] == b'>' {
                let content = self.data[content_start..self.cursor].to_vec();
                self.cursor += 1;
                return Ok(EngineValue {
                    span: start..self.cursor,
                    kind: EngineValueKind::HexString(
                        String::from_utf8_lossy(&content).into_owned(),
                    ),
                });
            }
            self.cursor += 1;
        }

        self.set_error("Unterminated hex string in EngineData payload");
        Err(self.fail("Unterminated hex string in EngineData payload"))
    }

    fn parse_number_or_identifier(&mut self) -> Result<EngineValue> {
        let start = self.cursor;
        while self.cursor < self.data.len() && !is_value_delimiter(self.data[self.cursor]) {
            self.cursor += 1;
        }
        if self.cursor == start {
            self.set_error("Unexpected token while parsing EngineData value");
            return Err(self.fail("Unexpected token while parsing EngineData value"));
        }
        let token = String::from_utf8_lossy(&self.data[start..self.cursor]).into_owned();
        let span = start..self.cursor;

        if token == "true" || token == "false" {
            return Ok(EngineValue {
                span,
                kind: EngineValueKind::Boolean(token == "true"),
            });
        }
        if let Some(number) = EngineNumber::parse_token(&token) {
            return Ok(EngineValue {
                span,
                kind: EngineValueKind::Number(number),
            });
        }
        Ok(EngineValue {
            span,
            kind: EngineValueKind::Identifier(token),
        })
    }
}

/// Parse an EngineData payload into the root value.
/// Parse a payload whose top level is a bare dictionary body — `/Key value`
/// pairs with no enclosing `<<`/`>>`, as in the document-level `Txt2` block
/// Photoshop CC writes. The returned dictionary spans the whole payload;
/// span-based patches work on it, while [`insert_dict_entry_bytes`] (which
/// needs a closing `>>`) refuses it.
pub fn parse_dictionary_body(data: &[u8]) -> Result<EngineValue> {
    let mut parser = Parser {
        data,
        cursor: 0,
        error_offset: 0,
        error_message: None,
    };
    parser.parse_dictionary_body(0)
}

pub fn parse(data: &[u8]) -> Result<EngineValue> {
    let mut parser = Parser {
        data,
        cursor: 0,
        error_offset: 0,
        error_message: None,
    };
    parser.skip_whitespace_and_comments();
    if parser.cursor >= data.len() {
        return Err(invalid(parser.cursor, "EngineData payload is empty"));
    }
    let root = parser.parse_value(0)?;
    parser.skip_whitespace_and_comments();
    if parser.cursor != data.len() {
        return Err(invalid(
            parser.cursor,
            "Trailing data after root EngineData value",
        ));
    }
    Ok(root)
}

/// Serialize a number the way Photoshop writes it (fixed 6-decimals with
/// trailing-zero trimming; integer literals keep their `i64` spelling).
fn format_number(number: EngineNumber) -> String {
    if let Some(integer) = number.integer {
        return integer.to_string();
    }
    let mut out = format!("{:.6}", number.value);
    while out.len() > 2 && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.push('0');
    }
    if out == "-0.0" {
        out = "0.0".to_string();
    }
    out
}

fn is_primitive(value: &EngineValue) -> bool {
    !matches!(
        value.kind,
        EngineValueKind::Dictionary(_) | EngineValueKind::Array(_)
    )
}

fn serialize_value(value: &EngineValue, out: &mut Vec<u8>, depth: usize) {
    match &value.kind {
        EngineValueKind::Dictionary(items) => {
            out.extend_from_slice(b"<<");
            if !items.is_empty() {
                out.push(b'\n');
            }
            for (index, (key, item)) in items.iter().enumerate() {
                out.extend(std::iter::repeat_n(b'\t', depth + 1));
                serialize_name_token(key, out);
                out.push(b' ');
                serialize_value(item, out, depth + 1);
                if index + 1 < items.len() {
                    out.push(b'\n');
                }
            }
            if !items.is_empty() {
                out.push(b'\n');
                out.extend(std::iter::repeat_n(b'\t', depth));
            }
            out.extend_from_slice(b">>");
        }
        EngineValueKind::Array(items) => {
            let compact = items.iter().all(is_primitive);
            out.push(b'[');
            if !items.is_empty() {
                if compact {
                    out.push(b' ');
                } else {
                    out.push(b'\n');
                }
            }
            for (index, item) in items.iter().enumerate() {
                if !compact {
                    out.extend(std::iter::repeat_n(b'\t', depth + 1));
                }
                serialize_value(item, out, depth + 1);
                if index + 1 < items.len() {
                    out.push(if compact { b' ' } else { b'\n' });
                }
            }
            if !items.is_empty() {
                if compact {
                    out.push(b' ');
                } else {
                    out.push(b'\n');
                    out.extend(std::iter::repeat_n(b'\t', depth));
                }
            }
            out.push(b']');
        }
        EngineValueKind::Name(name) => {
            serialize_name_token(name, out);
        }
        EngineValueKind::Number(number) => {
            out.extend_from_slice(format_number(*number).as_bytes());
        }
        EngineValueKind::Boolean(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" });
        }
        EngineValueKind::LiteralString(string) => {
            out.push(b'(');
            out.extend_from_slice(string.as_bytes());
            out.push(b')');
        }
        EngineValueKind::LiteralStringValue(string) => {
            out.push(b'(');
            for &byte in string.as_bytes() {
                if matches!(byte, b'(' | b')' | b'\\') {
                    out.push(b'\\');
                }
                out.push(byte);
            }
            out.push(b')');
        }
        EngineValueKind::LiteralStringBytes(string) => {
            out.push(b'(');
            out.extend_from_slice(string);
            out.push(b')');
        }
        EngineValueKind::HexString(string) => {
            out.push(b'<');
            out.extend_from_slice(string.as_bytes());
            out.push(b'>');
        }
        EngineValueKind::Identifier(identifier) => {
            out.extend_from_slice(identifier.as_bytes());
        }
    }
}

/// Serialize one value to its EngineData text (no leading newlines, no
/// trailing newline). `depth` controls indentation of nested containers.
pub fn format_value_bytes(value: &EngineValue, depth: usize) -> Vec<u8> {
    let mut out = Vec::new();
    serialize_value(value, &mut out, depth);
    out
}

/// Serialize a whole document: Photoshop expects two leading newlines before
/// the root `<<` and one trailing newline.
pub fn serialize(value: &EngineValue) -> Vec<u8> {
    let mut out = vec![0x0A, 0x0A];
    serialize_value(value, &mut out, 0);
    out.push(b'\n');
    out
}

/// Decode the source contents of an EngineData literal string, interpreting
/// PostScript backslash escapes while preserving arbitrary byte values.
pub fn decode_literal_bytes(source: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < source.len() {
        let byte = source[cursor];
        cursor += 1;
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }

        let Some(&escaped) = source.get(cursor) else {
            return Err(invalid(
                cursor - 1,
                "trailing escape in EngineData literal string",
            ));
        };
        cursor += 1;
        match escaped {
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                for _ in 0..2 {
                    let Some(&digit @ b'0'..=b'7') = source.get(cursor) else {
                        break;
                    };
                    value = value * 8 + u16::from(digit - b'0');
                    cursor += 1;
                }
                decoded.push(value as u8);
            }
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'b' => decoded.push(0x08),
            b'f' => decoded.push(0x0C),
            b'\n' => {}
            b'\r' => {
                if source.get(cursor) == Some(&b'\n') {
                    cursor += 1;
                }
            }
            other => decoded.push(other),
        }
    }
    Ok(decoded)
}

/// Decode a raw EngineData literal string containing UTF-16BE bytes to UTF-8.
/// A leading UTF-16BE BOM is optional, matching Photoshop's EngineData helper.
pub fn decode_utf16be_literal(source: &[u8]) -> Result<String> {
    let (units, _) = decode_utf16be_literal_units(source)?;
    String::from_utf16(&units).map_err(|_| {
        invalid(
            0,
            "invalid surrogate pair in EngineData UTF-16BE literal string",
        )
    })
}

/// Decode a UTF-16BE EngineData literal to code units, retaining whether a
/// leading BOM was present. This is useful for preserving non-text tail units
/// such as Photoshop's terminal carriage return during text edits.
pub fn decode_utf16be_literal_units(source: &[u8]) -> Result<(Vec<u16>, bool)> {
    let bytes = decode_literal_bytes(source)?;
    let has_bom = bytes.starts_with(&[0xFE, 0xFF]);
    let bytes = if has_bom {
        &bytes[2..]
    } else {
        bytes.as_slice()
    };
    if bytes.len() % 2 != 0 {
        return Err(invalid(
            0,
            "odd byte count in EngineData UTF-16BE literal string",
        ));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    Ok((units, has_bom))
}

/// Encode UTF-8 text to the raw PostScript source contents used by an
/// EngineData UTF-16BE literal string (including its BOM, excluding parens).
pub fn encode_utf16be_literal(text: &str) -> Vec<u8> {
    encode_utf16be_literal_units(&text.encode_utf16().collect::<Vec<_>>(), true)
}

/// Encode UTF-16 code units as literal-string source contents. Unit-level
/// encoding preserves tails even when they are not standalone valid UTF-16.
pub fn encode_utf16be_literal_units(units: &[u16], include_bom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(usize::from(include_bom) * 2 + units.len() * 2);
    if include_bom {
        out.extend_from_slice(&[0xFE, 0xFF]);
    }
    for &unit in units {
        for byte in unit.to_be_bytes() {
            if matches!(byte, b'(' | b')' | b'\\') {
                out.push(b'\\');
            }
            out.push(byte);
        }
    }
    out
}

/// Replace `range` in `payload` with `new_bytes`.
pub fn splice_payload(payload: &mut Vec<u8>, range: Range<usize>, new_bytes: &[u8]) {
    payload.splice(range, new_bytes.iter().copied());
}

/// A pending byte-range replacement inside an EngineData payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadPatch {
    pub range: Range<usize>,
    pub new_bytes: Vec<u8>,
}

/// Apply all patches; sorted by descending offset so earlier spans stay valid.
pub fn apply_patches(payload: &mut Vec<u8>, patches: &mut [PayloadPatch]) {
    patches.sort_by_key(|patch| std::cmp::Reverse(patch.range.start));
    for patch in patches.iter() {
        splice_payload(payload, patch.range.clone(), &patch.new_bytes);
    }
}

/// Validate and apply non-overlapping patches against one original payload.
/// The operation is failure-atomic: the payload is unchanged if a range is
/// out of bounds or overlaps another patch.
pub fn apply_patches_checked(payload: &mut Vec<u8>, patches: &mut [PayloadPatch]) -> Result<()> {
    let mut ordered: Vec<_> = patches.iter().collect();
    ordered.sort_by_key(|patch| (patch.range.start, patch.range.end));
    let mut previous: Option<&PayloadPatch> = None;
    for patch in ordered {
        if patch.range.start > patch.range.end || patch.range.end > payload.len() {
            return Err(invalid(
                0,
                "EngineData patch range is outside the source payload",
            ));
        }
        if previous.is_some_and(|prior| {
            prior.range.end > patch.range.start || prior.range.start == patch.range.start
        }) {
            return Err(invalid(0, "EngineData patch ranges overlap"));
        }
        previous = Some(patch);
    }
    apply_patches(payload, patches);
    Ok(())
}

/// Number of tab characters between the line start and `pos`.
fn indentation_of_line(payload: &[u8], pos: usize) -> usize {
    let mut line_start = pos;
    while line_start > 0 {
        let byte = payload[line_start - 1];
        if byte == b'\n' || byte == b'\r' {
            break;
        }
        line_start -= 1;
    }
    let mut tabs = 0;
    while line_start + tabs < pos && payload[line_start + tabs] == b'\t' {
        tabs += 1;
    }
    tabs
}

/// A patch replacing `current` (parsed from `payload`) with `replacement`,
/// formatted at the nesting depth of the line `current` starts on so a
/// replaced container keeps Photoshop's tab indentation.
pub fn replace_value_patch(
    payload: &[u8],
    current: &EngineValue,
    replacement: &EngineValue,
) -> PayloadPatch {
    let depth = indentation_of_line(payload, current.span.start.min(payload.len()));
    PayloadPatch {
        range: current.span.clone(),
        new_bytes: format_value_bytes(replacement, depth),
    }
}

/// Insert `/Key value` before the closing `>>` of `parent` (a dictionary
/// parsed from `payload`), indenting one level deeper than the `>>` line.
pub fn insert_dict_entry_bytes(
    payload: &mut Vec<u8>,
    parent: &EngineValue,
    key: &str,
    value: &EngineValue,
) -> bool {
    if !is_safe_name_token(key)
        || !matches!(parent.kind, EngineValueKind::Dictionary(_))
        || parent.span.end < 2
        || payload.get(parent.span.end - 2..parent.span.end) != Some(b">>".as_slice())
    {
        return false;
    }
    let closing = parent.span.end - 2;
    let parent_tabs = indentation_of_line(payload, closing);

    let mut entry = Vec::new();
    entry.extend(std::iter::repeat_n(b'\t', parent_tabs + 1));
    entry.push(b'/');
    entry.extend_from_slice(key.as_bytes());
    entry.push(b' ');
    entry.extend_from_slice(&format_value_bytes(value, 0));
    entry.push(b'\n');
    payload.splice(closing..closing, entry);
    true
}

/// Insert `item` before the closing `]` of `parent` (an array parsed from
/// `payload`), indenting one level deeper than the `]` line.
pub fn insert_array_item_bytes(
    payload: &mut Vec<u8>,
    parent: &EngineValue,
    item: &EngineValue,
) -> bool {
    if !matches!(parent.kind, EngineValueKind::Array(_))
        || parent.span.end < 1
        || payload.get(parent.span.end - 1) != Some(&b']')
    {
        return false;
    }
    let closing = parent.span.end - 1;
    let item_depth = indentation_of_line(payload, closing) + 1;

    let mut entry = Vec::new();
    entry.extend(std::iter::repeat_n(b'\t', item_depth));
    entry.extend_from_slice(&format_value_bytes(item, item_depth));
    entry.push(b'\n');
    payload.splice(closing..closing, entry);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "%!EngineData\n\
        << /EngineDict\n\
        \t<< /Editor\n\
        \t\t<< /Text (Hello \\(World\\))\n\
        \t\t /FontSize 28.0\n\
        \t\t /StyleRun [ 0 1 ]\n\
        \t\t /FontSet\n\
        \t\t\t[ << /Name /AdobeInvisFont >>\n\
        \t\t\t  << /Name /MyFont >>\n\
        \t\t\t]\n\
        \t\t /Color <FF8000>\n\
        \t\t /AutoKerning true\n\
        \t\t /GridDetails 16\n\
        \t\t /LeadingType Identifier\n\
        \t\t>>\n\
        \t>>\n\
        >>\n";

    #[test]
    fn parses_nested_document_with_spans() {
        let root = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(root.span.start, 13); // after the "%!EngineData\n"
        let engine = root.get("EngineDict").unwrap();
        let editor = engine.get("Editor").unwrap();

        let text = editor.get("Text").unwrap();
        assert_eq!(text.as_str(), Some("Hello \\(World\\)"));
        assert_eq!(
            &SAMPLE.as_bytes()[text.span.clone()],
            b"(Hello \\(World\\))"
        );

        // Integer vs float distinction.
        assert_eq!(
            editor.get("FontSize").unwrap().as_number(),
            Some(EngineNumber {
                value: 28.0,
                integer: None
            })
        );
        assert_eq!(
            editor.get("GridDetails").unwrap().as_number(),
            Some(EngineNumber {
                value: 16.0,
                integer: Some(16)
            })
        );
        assert_eq!(editor.get("AutoKerning").unwrap().as_bool(), Some(true));
        assert_eq!(
            editor.get("Color").unwrap().kind,
            EngineValueKind::HexString("FF8000".into())
        );
        assert_eq!(
            editor.get("LeadingType").unwrap().kind,
            EngineValueKind::Identifier("Identifier".into())
        );
        assert_eq!(
            editor.get("StyleRun").unwrap().as_int32_vector(),
            Some(vec![0, 1])
        );

        // Non-compact (nested) array keeps both entries.
        let font_set = editor.get("FontSet").unwrap();
        let names: Vec<_> = font_set
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.get("Name").unwrap().as_name().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["AdobeInvisFont", "MyFont"]);

        assert!(root
            .get_path(["EngineDict", "Editor", "FontSize"])
            .is_some());
        assert!(root.get_path(["EngineDict", "Missing"]).is_none());
    }

    #[test]
    fn serialize_reproduces_photoshop_layout() {
        let root = EngineValue {
            span: 0..0,
            kind: EngineValueKind::Dictionary(vec![(
                "Root".into(),
                EngineValue {
                    span: 0..0,
                    kind: EngineValueKind::Dictionary(vec![
                        ("Flag".into(), EngineValue::boolean(true)),
                        ("Count".into(), EngineValue::integer(3)),
                        ("Ratio".into(), EngineValue::number(0.5)),
                        (
                            "Compact".into(),
                            EngineValue {
                                span: 0..0,
                                kind: EngineValueKind::Array(vec![
                                    EngineValue::integer(1),
                                    EngineValue::integer(2),
                                ]),
                            },
                        ),
                    ]),
                },
            )]),
        };
        let bytes = serialize(&root);
        let text = String::from_utf8_lossy(&bytes);
        let expected = "\n\n<<\n\t/Root <<\n\t\t/Flag true\n\t\t/Count 3\n\t\t/Ratio 0.5\n\t\t/Compact [ 1 2 ]\n\t>>\n>>\n";
        assert_eq!(text, expected);

        // A re-parse re-serializes to the identical bytes (spans differ, so
        // compare bytes rather than values).
        let reparsed = parse(&bytes).unwrap();
        assert_eq!(serialize(&reparsed), bytes);
    }

    #[test]
    fn number_formatting_pins() {
        assert_eq!(
            format_number(EngineNumber {
                value: 28.0,
                integer: None
            }),
            "28.0"
        );
        assert_eq!(
            format_number(EngineNumber {
                value: 16.0,
                integer: Some(16)
            }),
            "16"
        );
        assert_eq!(
            format_number(EngineNumber {
                value: -0.0,
                integer: None
            }),
            "0.0"
        );
        assert_eq!(
            format_number(EngineNumber {
                value: 1.5,
                integer: None
            }),
            "1.5"
        );
    }

    #[test]
    fn set_number_preserves_integer_kind() {
        // A parsed float literal stays a float even when set to a whole
        // number (Photoshop distinguishes `20.0` from `20`).
        let mut value = parse(b"28.0").unwrap();
        assert!(value.set_number(20.0));
        assert_eq!(
            value.as_number(),
            Some(EngineNumber {
                value: 20.0,
                integer: None
            })
        );

        let mut integer = EngineValue::integer(16);
        assert!(integer.set_number(8.0));
        assert_eq!(
            integer.as_number(),
            Some(EngineNumber {
                value: 8.0,
                integer: Some(8)
            })
        );

        // Wrong types refuse.
        assert!(!EngineValue::string("x").set_number(1.0));
        assert!(!value.set_number(f64::NAN));
    }

    #[test]
    fn string_escapes_and_parens_nesting() {
        let value = parse(b"(a\\(b\\)c(d)e)").unwrap();
        assert_eq!(value.as_str(), Some("a\\(b\\)c(d)e"));

        assert_eq!(
            format_value_bytes(&EngineValue::string("a(b)\\c"), 0),
            b"(a\\(b\\)\\\\c)"
        );
        assert!(!EngineValue::integer(1).set_string("wrong type"));
    }

    #[test]
    fn name_values_and_dictionary_keys_escape_delimiters() {
        let name = EngineValue::name("Bad Name/#é");
        let encoded_name = format_value_bytes(&name, 0);
        assert_eq!(parse(&encoded_name).unwrap().as_name(), Some("Bad Name/#é"));

        // Direct AST construction can bypass EngineValue::insert's safe-key
        // guard, so serialization must still produce a valid Name token.
        let root = EngineValue {
            span: 0..0,
            kind: EngineValueKind::Dictionary(vec![("Bad Key".into(), EngineValue::integer(1))]),
        };
        let reparsed = parse(&serialize(&root)).unwrap();
        assert_eq!(
            reparsed
                .get("Bad Key")
                .and_then(EngineValue::as_number)
                .and_then(|n| n.integer),
            Some(1)
        );
    }

    #[test]
    fn checked_patches_reject_overlap_and_out_of_bounds_atomically() {
        let original = b"0123456789".to_vec();
        let mut payload = original.clone();
        let mut overlap = [
            PayloadPatch {
                range: 1..5,
                new_bytes: b"x".to_vec(),
            },
            PayloadPatch {
                range: 4..7,
                new_bytes: b"y".to_vec(),
            },
        ];
        assert!(apply_patches_checked(&mut payload, &mut overlap).is_err());
        assert_eq!(payload, original);

        let mut out_of_bounds = [PayloadPatch {
            range: 4..11,
            new_bytes: vec![],
        }];
        assert!(apply_patches_checked(&mut payload, &mut out_of_bounds).is_err());
        assert_eq!(payload, original);
    }

    #[test]
    fn literal_strings_preserve_non_utf8_and_decode_utf16be() {
        let text = "括弧 (\u{1F642})\\";
        let contents = encode_utf16be_literal(text);
        let mut payload = vec![b'('];
        payload.extend_from_slice(&contents);
        payload.push(b')');

        let parsed = parse(&payload).unwrap();
        assert!(matches!(
            parsed.kind,
            EngineValueKind::LiteralStringBytes(_)
        ));
        assert_eq!(
            decode_utf16be_literal(parsed.as_literal_bytes().unwrap()).unwrap(),
            text
        );
        assert_eq!(format_value_bytes(&parsed, 0), payload);
    }

    #[test]
    fn decodes_postscript_octal_escapes_and_line_continuations() {
        assert_eq!(
            decode_literal_bytes(b"A\\050B\\051\\\\C").unwrap(),
            b"A(B)\\C"
        );
        let continued = [b'A', b'\\', 0x0D, 0x0A, b'B'];
        assert_eq!(decode_literal_bytes(&continued).unwrap(), b"AB");
    }

    #[test]
    fn rejects_malformed_payloads() {
        for payload in [
            &b""[..],
            b"<< /A 1",       // unterminated dict
            b"[ 1 2",         // unterminated array
            b"(unterminated", // unterminated string
            b"<< /A 1 >> trailing",
        ] {
            assert!(parse(payload).is_err(), "{payload:?}");
        }
    }

    #[test]
    fn splice_and_apply_patches() {
        let mut payload = SAMPLE.as_bytes().to_vec();
        let root = parse(&payload).unwrap();
        let font_size = root
            .get_path(["EngineDict", "Editor", "FontSize"])
            .unwrap()
            .clone();

        // Replace 28.0 with 40.0 in place.
        splice_payload(&mut payload, font_size.span.clone(), b"40.0");
        let updated = parse(&payload).unwrap();
        assert_eq!(
            updated
                .get_path(["EngineDict", "Editor", "FontSize"])
                .unwrap()
                .as_double(),
            Some(40.0)
        );

        // Multiple patches apply regardless of order.
        let mut payload = SAMPLE.as_bytes().to_vec();
        let root = parse(&payload).unwrap();
        let engine = root.get("EngineDict").unwrap().clone();
        let editor = engine.get("Editor").unwrap().clone();
        let size = editor.get("FontSize").unwrap().clone();
        let grid = editor.get("GridDetails").unwrap().clone();
        let mut patches = vec![
            PayloadPatch {
                range: size.span.clone(),
                new_bytes: b"36.0".to_vec(),
            },
            PayloadPatch {
                range: grid.span.clone(),
                new_bytes: b"24".to_vec(),
            },
        ];
        apply_patches(&mut payload, &mut patches);
        let updated = parse(&payload).unwrap();
        let editor = updated.get_path(["EngineDict", "Editor"]).unwrap();
        assert_eq!(editor.get("FontSize").unwrap().as_double(), Some(36.0));
        assert_eq!(
            editor
                .get("GridDetails")
                .unwrap()
                .as_number()
                .unwrap()
                .integer,
            Some(24)
        );
    }

    #[test]
    fn insert_dict_entry_and_array_item_bytes() {
        let mut payload = SAMPLE.as_bytes().to_vec();
        let root = parse(&payload).unwrap();
        let editor = root.get_path(["EngineDict", "Editor"]).unwrap().clone();
        let original = payload.clone();

        assert!(!insert_dict_entry_bytes(
            &mut payload,
            &editor,
            "Bad Key",
            &EngineValue::integer(7)
        ));
        assert_eq!(payload, original);
        let mut constructed = EngineValue::dict();
        assert!(!constructed.insert("Bad/Key", EngineValue::integer(7)));

        assert!(insert_dict_entry_bytes(
            &mut payload,
            &editor,
            "NewKey",
            &EngineValue::integer(7)
        ));
        let updated = parse(&payload).unwrap();
        let editor = updated.get_path(["EngineDict", "Editor"]).unwrap();
        assert_eq!(
            editor.get("NewKey").unwrap().as_number().unwrap().integer,
            Some(7)
        );

        // Insert into the (compact) StyleRun array.
        let style_run = editor.get("StyleRun").unwrap().clone();
        assert!(insert_array_item_bytes(
            &mut payload,
            &style_run,
            &EngineValue::integer(9)
        ));
        let updated = parse(&payload).unwrap();
        let style_run = updated
            .get_path(["EngineDict", "Editor", "StyleRun"])
            .unwrap();
        assert_eq!(style_run.as_int32_vector(), Some(vec![0, 1, 9]));
    }

    #[test]
    fn deeply_nested_containers_are_a_typed_error_not_a_stack_overflow() {
        // `MAX_NESTING_DEPTH + 2` nested arrays must error out instead of
        // recursing until the stack overflows.
        let payload = format!(
            "{}1{}",
            "[".repeat(MAX_NESTING_DEPTH + 2),
            "]".repeat(MAX_NESTING_DEPTH + 2)
        );
        assert!(parse(payload.as_bytes()).is_err());
    }
}
