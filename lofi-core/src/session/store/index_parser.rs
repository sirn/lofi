use std::io::{BufRead, Seek};

use lofi_error::{Error, Result};
use serde::Deserialize;

use crate::agent::{MAX_EXEC_RESULT_BYTES, MAX_TOOL_RESULT_BYTES};

const MAX_DEPTH: usize = 128;
const MAX_KEY_BYTES: usize = 256;
const MAX_FIELD_BYTES: usize = 64 * 1024;
const MAX_INDEX_LINE_BYTES: usize = 64 * 1024;
const MAX_PROJECTED_STRINGS_BYTES: usize = 2 * 1024 * 1024;
const MAX_COLLECTION_ITEMS: usize = 4096;
const MAX_OBJECT_FIELDS: usize = MAX_COLLECTION_ITEMS * 4;
const TRUNCATED_STRING: &[u8] = br"\n[transcript content truncated for display]";

pub(super) struct ProjectionBudget {
    content_bytes: usize,
    metadata_bytes: usize,
    array_items: usize,
    object_fields: usize,
}

impl ProjectionBudget {
    pub(super) fn collection() -> Self {
        Self {
            content_bytes: MAX_PROJECTED_STRINGS_BYTES,
            metadata_bytes: 256 * 1024,
            array_items: MAX_COLLECTION_ITEMS,
            object_fields: MAX_OBJECT_FIELDS,
        }
    }

    pub(super) fn event() -> Self {
        Self::collection()
    }

    fn begin_value(&mut self) {
        self.metadata_bytes = 256 * 1024;
        self.array_items = MAX_COLLECTION_ITEMS;
        self.object_fields = MAX_OBJECT_FIELDS;
    }
}

pub(super) struct ParsedIndexEvent {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind_type: String,
    pub role: Option<String>,
    pub leaf_id: Option<String>,
    pub checkpointed_tail: bool,
    pub first_kept_entry_id: String,
}

#[derive(Deserialize)]
struct BorrowedIndexEvent<'a> {
    #[serde(default, borrow)]
    id: &'a str,
    #[serde(default, borrow)]
    parent_id: Option<&'a str>,
    #[serde(default, borrow, rename = "type")]
    kind_type: &'a str,
    #[serde(default, borrow)]
    role: Option<&'a str>,
    #[serde(default, borrow)]
    leaf_id: Option<&'a str>,
    #[serde(default)]
    checkpointed_tail: bool,
    #[serde(default, borrow)]
    first_kept_entry_id: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct IndexEventRef<'a> {
    pub id: &'a str,
    pub parent_id: Option<&'a str>,
    pub kind_type: &'a str,
    pub role: Option<&'a str>,
    pub leaf_id: Option<&'a str>,
    pub checkpointed_tail: bool,
    pub first_kept_entry_id: &'a str,
}

impl<'a> From<&'a BorrowedIndexEvent<'a>> for IndexEventRef<'a> {
    fn from(event: &'a BorrowedIndexEvent<'a>) -> Self {
        Self {
            id: event.id,
            parent_id: event.parent_id,
            kind_type: event.kind_type,
            role: event.role,
            leaf_id: event.leaf_id,
            checkpointed_tail: event.checkpointed_tail,
            first_kept_entry_id: event.first_kept_entry_id,
        }
    }
}

impl<'a> From<&'a ParsedIndexEvent> for IndexEventRef<'a> {
    fn from(event: &'a ParsedIndexEvent) -> Self {
        Self {
            id: &event.id,
            parent_id: event.parent_id.as_deref(),
            kind_type: &event.kind_type,
            role: event.role.as_deref(),
            leaf_id: event.leaf_id.as_deref(),
            checkpointed_tail: event.checkpointed_tail,
            first_kept_entry_id: &event.first_kept_entry_id,
        }
    }
}

pub(super) fn read_index_event<R, T>(
    reader: &mut std::io::BufReader<R>,
    line: &mut Vec<u8>,
    map: impl FnOnce(u64, u64, IndexEventRef<'_>) -> T,
) -> Result<Option<T>>
where
    R: std::io::Read + std::io::Seek,
{
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        let whitespace = available
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        reader.consume(whitespace);
        if whitespace == 0 {
            break;
        }
    }
    let start = reader.stream_position()?;
    line.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let take = memchr::memchr(b'\n', available).map_or(available.len(), |at| at + 1);
        if line.len().saturating_add(take) > MAX_INDEX_LINE_BYTES {
            reader.seek(std::io::SeekFrom::Start(start))?;
            let mut parser = Parser { reader };
            let event = parser.parse_event()?;
            parser.skip_whitespace()?;
            let end = parser.reader.stream_position()?;
            return Ok(Some(map(start, end, IndexEventRef::from(&event))));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            break;
        }
    }
    let event = serde_json::from_slice::<BorrowedIndexEvent<'_>>(line)
        .map_err(|error| json_error(&error.to_string()))?;
    let end = reader.stream_position()?;
    Ok(Some(map(start, end, IndexEventRef::from(&event))))
}

pub(super) fn read_projected_value<T, R>(
    reader: &mut std::io::BufReader<R>,
    budget: &mut ProjectionBudget,
) -> Result<Option<(u64, u64, T)>>
where
    T: serde::de::DeserializeOwned,
    R: std::io::Read + std::io::Seek,
{
    budget.begin_value();
    let mut parser = Parser { reader };
    parser.skip_whitespace()?;
    if parser.peek()?.is_none() {
        return Ok(None);
    }
    let start = parser.reader.stream_position()?;
    let mut projected = Vec::with_capacity(8 * 1024);
    parser.copy_value(&mut projected, budget, None, 1)?;
    parser.skip_whitespace()?;
    let end = parser.reader.stream_position()?;
    let value = serde_json::from_slice(&projected)
        .map_err(|error| json_error(&format!("projected value: {error}")))?;
    Ok(Some((start, end, value)))
}

struct Parser<'a, R> {
    reader: &'a mut std::io::BufReader<R>,
}

impl<R> Parser<'_, R>
where
    R: std::io::Read + std::io::Seek,
{
    fn copy_value(
        &mut self,
        out: &mut Vec<u8>,
        budget: &mut ProjectionBudget,
        key: Option<&str>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(json_error("projected event exceeds maximum JSON depth"));
        }
        match self.peek()? {
            Some(b'"') => self.copy_string(out, budget, key),
            Some(b'{') => self.copy_object(out, budget, depth),
            Some(b'[') => self.copy_array(out, budget, depth),
            Some(b't') => {
                self.expect_literal(b"true")?;
                out.extend_from_slice(b"true");
                Ok(())
            }
            Some(b'f') => {
                self.expect_literal(b"false")?;
                out.extend_from_slice(b"false");
                Ok(())
            }
            Some(b'n') => {
                self.expect_literal(b"null")?;
                out.extend_from_slice(b"null");
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => self.copy_number(out),
            Some(_) => Err(json_error("invalid JSON value")),
            None => Err(json_error("unexpected end of JSON value")),
        }
    }

    fn copy_object(
        &mut self,
        out: &mut Vec<u8>,
        budget: &mut ProjectionBudget,
        depth: usize,
    ) -> Result<()> {
        self.expect(b'{')?;
        out.push(b'{');
        self.skip_whitespace()?;
        if self.consume_if(b'}')? {
            out.push(b'}');
            return Ok(());
        }
        let mut first = true;
        loop {
            let key = self.parse_string(MAX_KEY_BYTES)?;
            self.skip_whitespace()?;
            self.expect(b':')?;
            self.skip_whitespace()?;
            if budget.object_fields == 0 {
                self.skip_value(depth + 1)?;
            } else {
                budget.object_fields -= 1;
                if !first {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, &key)
                    .map_err(|error| json_error(&error.to_string()))?;
                out.push(b':');
                self.copy_value(out, budget, Some(&key), depth + 1)?;
                first = false;
            }
            self.skip_whitespace()?;
            if self.consume_if(b'}')? {
                out.push(b'}');
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace()?;
        }
    }

    fn copy_array(
        &mut self,
        out: &mut Vec<u8>,
        budget: &mut ProjectionBudget,
        depth: usize,
    ) -> Result<()> {
        self.expect(b'[')?;
        out.push(b'[');
        self.skip_whitespace()?;
        if self.consume_if(b']')? {
            out.push(b']');
            return Ok(());
        }
        let mut first = true;
        loop {
            if budget.array_items == 0 {
                self.skip_value(depth + 1)?;
            } else {
                budget.array_items -= 1;
                if !first {
                    out.push(b',');
                }
                self.copy_value(out, budget, None, depth + 1)?;
                first = false;
            }
            self.skip_whitespace()?;
            if self.consume_if(b']')? {
                out.push(b']');
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace()?;
        }
    }

    fn copy_string(
        &mut self,
        out: &mut Vec<u8>,
        budget: &mut ProjectionBudget,
        key: Option<&str>,
    ) -> Result<()> {
        if key == Some("bytes") {
            self.skip_string()?;
            out.extend_from_slice(br#""""#);
            return Ok(());
        }
        self.expect(b'"')?;
        out.push(b'"');
        let field_limit = if key == Some("result") {
            MAX_TOOL_RESULT_BYTES
        } else {
            MAX_EXEC_RESULT_BYTES
        };
        let remaining = if key.is_some_and(is_structural_key) {
            &mut budget.metadata_bytes
        } else {
            &mut budget.content_bytes
        };
        let limit = field_limit.min(*remaining);
        let prefix_limit = limit.saturating_sub(TRUNCATED_STRING.len()) / 2;
        let mut prefix = Vec::with_capacity(prefix_limit.min(8 * 1024));
        let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut tail_units = std::collections::VecDeque::new();
        let mut source_bytes = 0usize;
        loop {
            let first = self
                .next()?
                .ok_or_else(|| json_error("unterminated string"))?;
            if first == b'"' {
                break;
            }
            let mut unit = [0_u8; 12];
            unit[0] = first;
            let len = match first {
                b'\\' => {
                    let mut escaped = Vec::with_capacity(12);
                    escaped.push(first);
                    let mut capture = Some((&mut escaped, 12));
                    self.scan_escape(&mut capture)?;
                    unit[..escaped.len()].copy_from_slice(&escaped);
                    escaped.len()
                }
                0x00..=0x1f => return Err(json_error("control character in string")),
                0x20..=0x7f => 1,
                0xc2..=0xdf => self.read_utf8_tail(&mut unit, 2, 0x80, 0xbf)?,
                0xe0 => self.read_utf8_tail(&mut unit, 3, 0xa0, 0xbf)?,
                0xe1..=0xec | 0xee..=0xef => self.read_utf8_tail(&mut unit, 3, 0x80, 0xbf)?,
                0xed => self.read_utf8_tail(&mut unit, 3, 0x80, 0x9f)?,
                0xf0 => self.read_utf8_tail(&mut unit, 4, 0x90, 0xbf)?,
                0xf1..=0xf3 => self.read_utf8_tail(&mut unit, 4, 0x80, 0xbf)?,
                0xf4 => self.read_utf8_tail(&mut unit, 4, 0x80, 0x8f)?,
                _ => return Err(json_error("invalid UTF-8 in string")),
            };
            source_bytes = source_bytes.saturating_add(len);
            if prefix.len().saturating_add(len) <= prefix_limit {
                prefix.extend_from_slice(&unit[..len]);
                continue;
            }
            tail.extend(&unit[..len]);
            tail_units.push_back(len as u8);
            let tail_limit = limit.saturating_sub(prefix.len());
            while tail.len() > tail_limit {
                let Some(remove) = tail_units.pop_front() else {
                    break;
                };
                for _ in 0..remove {
                    tail.pop_front();
                }
            }
        }
        let truncated = source_bytes > limit;
        if truncated {
            let tail_limit = limit
                .saturating_sub(prefix.len())
                .saturating_sub(TRUNCATED_STRING.len());
            while tail.len() > tail_limit {
                let Some(remove) = tail_units.pop_front() else {
                    break;
                };
                for _ in 0..remove {
                    tail.pop_front();
                }
            }
        }
        out.extend_from_slice(&prefix);
        if truncated && limit >= TRUNCATED_STRING.len() {
            out.extend_from_slice(TRUNCATED_STRING);
        }
        out.extend(tail);
        let emitted = prefix.len()
            + tail_units.into_iter().map(usize::from).sum::<usize>()
            + usize::from(truncated && limit >= TRUNCATED_STRING.len()) * TRUNCATED_STRING.len();
        *remaining = remaining.saturating_sub(emitted);
        out.push(b'"');
        Ok(())
    }

    fn read_utf8_tail(
        &mut self,
        unit: &mut [u8; 12],
        len: usize,
        first_min: u8,
        first_max: u8,
    ) -> Result<usize> {
        for (index, slot) in unit.iter_mut().enumerate().take(len).skip(1) {
            let byte = self
                .next()?
                .ok_or_else(|| json_error("incomplete UTF-8 in string"))?;
            let valid = if index == 1 {
                (first_min..=first_max).contains(&byte)
            } else {
                (0x80..=0xbf).contains(&byte)
            };
            if !valid {
                return Err(json_error("invalid UTF-8 in string"));
            }
            *slot = byte;
        }
        Ok(len)
    }

    fn copy_number(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let mut number = Vec::with_capacity(24);
        while self
            .peek()?
            .is_some_and(|byte| matches!(byte, b'+' | b'-' | b'.' | b'0'..=b'9' | b'e' | b'E'))
        {
            if number.len() == 128 {
                return Err(json_error("number exceeds maximum size"));
            }
            let Some(byte) = self.next()? else {
                return Err(json_error("unexpected end of JSON number"));
            };
            number.push(byte);
        }
        serde_json::from_slice::<serde_json::Number>(&number)
            .map_err(|error| json_error(&error.to_string()))?;
        out.extend_from_slice(&number);
        Ok(())
    }

    fn parse_event(&mut self) -> Result<ParsedIndexEvent> {
        self.expect(b'{')?;
        let mut event = ParsedIndexEvent {
            id: String::new(),
            parent_id: None,
            kind_type: String::new(),
            role: None,
            leaf_id: None,
            checkpointed_tail: false,
            first_kept_entry_id: String::new(),
        };
        self.skip_whitespace()?;
        if self.consume_if(b'}')? {
            return Ok(event);
        }
        loop {
            let key = self.parse_string(MAX_KEY_BYTES)?;
            self.skip_whitespace()?;
            self.expect(b':')?;
            self.skip_whitespace()?;
            match key.as_str() {
                "id" => event.id = self.parse_string(MAX_FIELD_BYTES)?,
                "parent_id" => event.parent_id = self.parse_optional_string()?,
                "type" => event.kind_type = self.parse_string(MAX_FIELD_BYTES)?,
                "role" => event.role = self.parse_optional_string()?,
                "leaf_id" => event.leaf_id = self.parse_optional_string()?,
                "checkpointed_tail" => event.checkpointed_tail = self.parse_bool()?,
                "first_kept_entry_id" => {
                    event.first_kept_entry_id = self.parse_string(MAX_FIELD_BYTES)?;
                }
                _ => self.skip_value(1)?,
            }
            self.skip_whitespace()?;
            if self.consume_if(b'}')? {
                return Ok(event);
            }
            self.expect(b',')?;
            self.skip_whitespace()?;
        }
    }

    fn parse_optional_string(&mut self) -> Result<Option<String>> {
        if self.peek()? == Some(b'n') {
            self.expect_literal(b"null")?;
            Ok(None)
        } else {
            self.parse_string(MAX_FIELD_BYTES).map(Some)
        }
    }

    fn parse_bool(&mut self) -> Result<bool> {
        match self.peek()? {
            Some(b't') => {
                self.expect_literal(b"true")?;
                Ok(true)
            }
            Some(b'f') => {
                self.expect_literal(b"false")?;
                Ok(false)
            }
            _ => Err(json_error("expected boolean")),
        }
    }

    fn skip_value(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(json_error("index event exceeds maximum JSON depth"));
        }
        match self.peek()? {
            Some(b'"') => self.skip_string(),
            Some(b'{') => self.skip_object(depth),
            Some(b'[') => self.skip_array(depth),
            Some(b't') => self.expect_literal(b"true"),
            Some(b'f') => self.expect_literal(b"false"),
            Some(b'n') => self.expect_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.skip_number(),
            Some(_) => Err(json_error("invalid JSON value")),
            None => Err(json_error("unexpected end of JSON value")),
        }
    }

    fn skip_object(&mut self, depth: usize) -> Result<()> {
        self.expect(b'{')?;
        self.skip_whitespace()?;
        if self.consume_if(b'}')? {
            return Ok(());
        }
        loop {
            self.skip_string()?;
            self.skip_whitespace()?;
            self.expect(b':')?;
            self.skip_whitespace()?;
            self.skip_value(depth + 1)?;
            self.skip_whitespace()?;
            if self.consume_if(b'}')? {
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace()?;
        }
    }

    fn skip_array(&mut self, depth: usize) -> Result<()> {
        self.expect(b'[')?;
        self.skip_whitespace()?;
        if self.consume_if(b']')? {
            return Ok(());
        }
        loop {
            self.skip_value(depth + 1)?;
            self.skip_whitespace()?;
            if self.consume_if(b']')? {
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace()?;
        }
    }

    fn parse_string(&mut self, max_bytes: usize) -> Result<String> {
        let mut raw = Vec::with_capacity(64);
        self.scan_string(Some((&mut raw, max_bytes.saturating_add(2))))?;
        serde_json::from_slice(&raw).map_err(|error| json_error(&error.to_string()))
    }

    fn skip_string(&mut self) -> Result<()> {
        self.scan_string(None)
    }

    fn scan_string(&mut self, mut capture: Option<(&mut Vec<u8>, usize)>) -> Result<()> {
        let quote = self.next()?.ok_or_else(|| json_error("expected string"))?;
        if quote != b'"' {
            return Err(json_error("expected string"));
        }
        push_captured(&mut capture, quote)?;
        let mut utf8_remaining = 0u8;
        let mut continuation_min = 0x80u8;
        let mut continuation_max = 0xbfu8;
        loop {
            let byte = self
                .next()?
                .ok_or_else(|| json_error("unterminated string"))?;
            push_captured(&mut capture, byte)?;
            if utf8_remaining > 0 {
                if !(continuation_min..=continuation_max).contains(&byte) {
                    return Err(json_error("invalid UTF-8 in string"));
                }
                utf8_remaining -= 1;
                continuation_min = 0x80;
                continuation_max = 0xbf;
                continue;
            }
            match byte {
                b'"' => return Ok(()),
                b'\\' => self.scan_escape(&mut capture)?,
                0x00..=0x1f => return Err(json_error("control character in string")),
                0x20..=0x7f => {}
                0xc2..=0xdf => utf8_remaining = 1,
                0xe0 => {
                    utf8_remaining = 2;
                    continuation_min = 0xa0;
                }
                0xe1..=0xec | 0xee..=0xef => utf8_remaining = 2,
                0xed => {
                    utf8_remaining = 2;
                    continuation_max = 0x9f;
                }
                0xf0 => {
                    utf8_remaining = 3;
                    continuation_min = 0x90;
                }
                0xf1..=0xf3 => utf8_remaining = 3,
                0xf4 => {
                    utf8_remaining = 3;
                    continuation_max = 0x8f;
                }
                _ => return Err(json_error("invalid UTF-8 in string")),
            }
        }
    }

    fn scan_escape(&mut self, capture: &mut Option<(&mut Vec<u8>, usize)>) -> Result<()> {
        let escaped = self
            .next()?
            .ok_or_else(|| json_error("unterminated string escape"))?;
        push_captured(capture, escaped)?;
        match escaped {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Ok(()),
            b'u' => {
                let unit = self.scan_hex_quad(capture)?;
                if (0xd800..=0xdbff).contains(&unit) {
                    let slash = self
                        .next()?
                        .ok_or_else(|| json_error("missing low surrogate"))?;
                    let u = self
                        .next()?
                        .ok_or_else(|| json_error("missing low surrogate"))?;
                    push_captured(capture, slash)?;
                    push_captured(capture, u)?;
                    if slash != b'\\' || u != b'u' {
                        return Err(json_error("missing low surrogate"));
                    }
                    let low = self.scan_hex_quad(capture)?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(json_error("invalid low surrogate"));
                    }
                } else if (0xdc00..=0xdfff).contains(&unit) {
                    return Err(json_error("unpaired low surrogate"));
                }
                Ok(())
            }
            _ => Err(json_error("invalid string escape")),
        }
    }

    fn scan_hex_quad(&mut self, capture: &mut Option<(&mut Vec<u8>, usize)>) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self
                .next()?
                .ok_or_else(|| json_error("incomplete unicode escape"))?;
            push_captured(capture, byte)?;
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a' + 10),
                b'A'..=b'F' => u16::from(byte - b'A' + 10),
                _ => return Err(json_error("invalid unicode escape")),
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn skip_number(&mut self) -> Result<()> {
        self.consume_if(b'-')?;
        match self.next()? {
            Some(b'0') => {
                if self.peek()?.is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(json_error("leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => {
                while self.peek()?.is_some_and(|byte| byte.is_ascii_digit()) {
                    self.next()?;
                }
            }
            _ => return Err(json_error("invalid number")),
        }
        if self.consume_if(b'.')? {
            self.require_digits()?;
        }
        if self.peek()?.is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.next()?;
            if self.peek()?.is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.next()?;
            }
            self.require_digits()?;
        }
        Ok(())
    }

    fn require_digits(&mut self) -> Result<()> {
        if !self.peek()?.is_some_and(|byte| byte.is_ascii_digit()) {
            return Err(json_error("expected digit"));
        }
        while self.peek()?.is_some_and(|byte| byte.is_ascii_digit()) {
            self.next()?;
        }
        Ok(())
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<()> {
        for expected in literal {
            self.expect(*expected)?;
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) -> Result<()> {
        while self
            .peek()?
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.next()?;
        }
        Ok(())
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        match self.next()? {
            Some(actual) if actual == expected => Ok(()),
            Some(_) => Err(json_error("unexpected JSON token")),
            None => Err(json_error("unexpected end of JSON")),
        }
    }

    fn consume_if(&mut self, expected: u8) -> Result<bool> {
        if self.peek()? == Some(expected) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(self.reader.fill_buf()?.first().copied())
    }

    fn next(&mut self) -> Result<Option<u8>> {
        let byte = self.reader.fill_buf()?.first().copied();
        if byte.is_some() {
            self.reader.consume(1);
        }
        Ok(byte)
    }
}

fn is_structural_key(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "role"
            | "id"
            | "parent_id"
            | "tool_use_id"
            | "parent"
            | "name"
            | "provider"
            | "model"
            | "format"
            | "kind"
    )
}

fn push_captured(capture: &mut Option<(&mut Vec<u8>, usize)>, byte: u8) -> Result<()> {
    if let Some((bytes, limit)) = capture {
        if bytes.len() >= *limit {
            return Err(json_error("index field exceeds maximum size"));
        }
        bytes.push(byte);
    }
    Ok(())
}

fn json_error(message: &str) -> Error {
    Error::State(format!("json: {message}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn projection_caps_large_result_without_changing_source_extent() {
        let result = "x".repeat(4 * 1024 * 1024);
        let source = serde_json::to_vec(&serde_json::json!({
            "type": "native_tool",
            "parent": "exec-1",
            "call_id": 1,
            "name": "bash",
            "args": "{}",
            "result": result,
            "is_error": false,
        }))
        .unwrap();
        let mut reader = BufReader::new(Cursor::new(source.clone()));
        let (_, end, projected) = read_projected_value::<serde_json::Value, _>(
            &mut reader,
            &mut ProjectionBudget::event(),
        )
        .unwrap()
        .unwrap();

        let displayed = projected["result"].as_str().unwrap();
        assert!(displayed.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(displayed.contains("transcript content truncated for display"));
        assert_eq!(end, source.len() as u64);
    }

    #[test]
    fn projection_keeps_the_end_of_a_truncated_string() {
        let marker = "RECENT_TAIL_MARKER";
        let result = format!("{}{}", "x".repeat(MAX_TOOL_RESULT_BYTES), marker);
        let source = serde_json::to_vec(&serde_json::json!({
            "type": "native_tool",
            "parent": "exec-1",
            "call_id": 1,
            "name": "bash",
            "args": "{}",
            "result": result,
            "is_error": false,
        }))
        .unwrap();
        let mut reader = BufReader::new(Cursor::new(source));
        let (_, _, projected) = read_projected_value::<serde_json::Value, _>(
            &mut reader,
            &mut ProjectionBudget::event(),
        )
        .unwrap()
        .unwrap();

        let displayed = projected["result"].as_str().unwrap();
        assert!(displayed.contains("transcript content truncated for display"));
        assert!(displayed.ends_with(marker));
        assert!(displayed.len() <= MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn projection_keeps_image_events_deserializable_without_binary_payload() {
        let event = lofi_types::SessionEvent {
            id: "image".into(),
            parent_id: None,
            kind: lofi_types::SessionEventKind::Message(lofi_types::Message {
                role: lofi_types::Role::User,
                blocks: vec![lofi_types::ContentBlock::Image {
                    bytes: vec![1; 1024 * 1024],
                    media_type: "image/png".into(),
                }],
                kind: lofi_types::PromptKind::default(),
            }),
        };
        let source = serde_json::to_vec(&event).unwrap();
        let mut reader = BufReader::new(Cursor::new(source));
        let (_, _, projected) = read_projected_value::<lofi_types::SessionEvent, _>(
            &mut reader,
            &mut ProjectionBudget::event(),
        )
        .unwrap()
        .unwrap();

        let lofi_types::SessionEventKind::Message(message) = projected.kind else {
            panic!("expected message");
        };
        assert!(matches!(
            &message.blocks[0],
            lofi_types::ContentBlock::Image { bytes, .. } if bytes.is_empty()
        ));
    }

    #[test]
    fn projection_keeps_valid_escaped_unicode() {
        let source = r#"{"type":"message","text":"A\n\uD83D\uDE00é"}"#.as_bytes().to_vec();
        let mut reader = BufReader::new(Cursor::new(source));
        let (_, _, projected) = read_projected_value::<serde_json::Value, _>(
            &mut reader,
            &mut ProjectionBudget::event(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(projected["text"], "A\n😀é");
    }

    #[test]
    fn shared_projection_budget_bounds_many_content_fields() {
        let source = format!(
            "{{\"text\":\"{}\"}}\n{{\"text\":\"{}\"}}",
            "a".repeat(MAX_PROJECTED_STRINGS_BYTES),
            "b".repeat(MAX_PROJECTED_STRINGS_BYTES),
        );
        let mut reader = BufReader::new(Cursor::new(source));
        let mut budget = ProjectionBudget::event();
        let (_, _, first) = read_projected_value::<serde_json::Value, _>(&mut reader, &mut budget)
            .unwrap()
            .unwrap();
        let (_, _, second) = read_projected_value::<serde_json::Value, _>(&mut reader, &mut budget)
            .unwrap()
            .unwrap();

        let retained =
            first["text"].as_str().unwrap().len() + second["text"].as_str().unwrap().len();
        assert!(retained <= MAX_PROJECTED_STRINGS_BYTES);
    }

    #[test]
    fn projection_bounds_arrays_per_value() {
        let items = std::iter::repeat_n("0", MAX_COLLECTION_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            r#"{{"items":[{items}]}}
{{"items":[{items}]}}"#
        );
        let mut reader = BufReader::new(Cursor::new(source));
        let mut budget = ProjectionBudget::collection();
        let (_, _, first) = read_projected_value::<serde_json::Value, _>(&mut reader, &mut budget)
            .unwrap()
            .unwrap();
        let (_, _, second) = read_projected_value::<serde_json::Value, _>(&mut reader, &mut budget)
            .unwrap()
            .unwrap();

        assert_eq!(
            first["items"].as_array().unwrap().len(),
            MAX_COLLECTION_ITEMS
        );
        assert_eq!(
            second["items"].as_array().unwrap().len(),
            MAX_COLLECTION_ITEMS
        );
    }

    #[test]
    fn projection_caps_repeated_structural_fields() {
        let fields = std::iter::repeat_n(r#""type":"message""#, MAX_OBJECT_FIELDS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("{{{fields}}}").into_bytes();
        let mut reader = BufReader::new(Cursor::new(source));
        let mut projected = Vec::new();
        Parser {
            reader: &mut reader,
        }
        .copy_value(&mut projected, &mut ProjectionBudget::event(), None, 1)
        .unwrap();

        assert_eq!(
            projected
                .windows(br#""type":"#.len())
                .filter(|window| *window == br#""type":"#)
                .count(),
            MAX_OBJECT_FIELDS
        );
    }
}
