//! papaparse 5.5.3's `Parser.parse` loop as the CMS validator configures it (`delimiter:
//! ","`, `newline: "\n"`, quote and escape `"`), fed the way a Node file stream feeds it
//! (`text::Stream`): the BOM removed from the first chunk only, and an unfinished row parsed
//! again once the next chunk arrives. Quote problems are not reported: the fields come out
//! as papaparse gives them.

use crate::text;
use std::io::{self, Read};

/// One row as `Reader::next_row` lends it, its fields borrowed from the reader's text.
pub struct Row<'a> {
    /// Physical line where the row starts, 1-based, counting lines removed with the BOM.
    pub line: u64,
    /// The text from the row's start on.
    text: &'a str,
    spans: &'a [(usize, usize, usize)],
    unescaped: &'a str,
}

impl<'a> Row<'a> {
    /// The fields that are not empty, with their index; most fields of a wide file are.
    pub fn non_empty(self) -> impl Iterator<Item = (usize, &'a str)> {
        let (text, unescaped) = (self.text, self.unescaped);
        self.spans.iter().map(move |&(i, start, end)| {
            let field = if end <= text.len() {
                &text[start..end]
            } else {
                &unescaped[start - text.len()..end - text.len()]
            };
            (i, field)
        })
    }
}

/// A row's fields, the buffers reused for the next row, so reading
/// allocates only for a longer row.
#[derive(Default)]
struct Parsed {
    fields: usize,
    /// Each non-empty field's index and byte range in the text from the row's start on; past
    /// the text's end, in `unescaped` after the text's length.
    spans: Vec<(usize, usize, usize)>,
    /// Quoted fields with `""`, read as `"`, back to back.
    unescaped: String,
}

impl Parsed {
    fn push(&mut self, start: usize, end: usize) {
        if start < end {
            self.spans.push((self.fields, start, end));
        }
        self.fields += 1;
    }

    /// The quoted field `s[start..end]`; `doubled` tells whether it holds any `""`.
    fn push_quoted(&mut self, s: &str, start: usize, end: usize, doubled: bool) {
        if !doubled {
            return self.push(start, end);
        }
        let from = s.len() + self.unescaped.len();
        let mut field = &s[start..end];
        // A char search: splitting on `""` would set up a substring search for every field.
        while let Some(i) = field.find('"') {
            self.unescaped.push_str(&field[..=i]);
            let rest = &field[i + 1..];
            field = rest.strip_prefix('"').unwrap_or(rest);
        }
        self.unescaped.push_str(field);
        self.push(from, s.len() + self.unescaped.len());
    }
}

pub struct Reader<R> {
    stream: text::Stream<R>,
    /// Decoded text; rows before `pos` have been returned.
    text: String,
    pos: usize,
    /// Text after `pos` already parsed without finishing a row.
    scanned: usize,
    first: bool,
    eof: bool,
    /// Physical line the next row starts on.
    pub line: u64,
    /// Newlines removed together with the BOM.
    pub bom_lines: u64,
    parsed: Parsed,
}

impl<R: Read> Reader<R> {
    pub fn new(input: R) -> Self {
        Self::with_chunk(input, text::CHUNK)
    }

    pub fn with_chunk(input: R, chunk: usize) -> Self {
        Reader {
            stream: text::Stream::new(input, chunk),
            text: String::new(),
            pos: 0,
            scanned: 0,
            first: true,
            eof: false,
            line: 1,
            bom_lines: 0,
            parsed: Parsed::default(),
        }
    }

    pub fn next_row(&mut self) -> io::Result<Option<Row<'_>>> {
        loop {
            // Before the end of input a row ends only at a newline; a row not tried yet needs
            // no search for one, as parse_row finds out.
            // ponytail: a quoted field spanning chunks with newlines is parsed again per chunk,
            // as papaparse does; make parse_row resumable if such files show up
            if (self.eof
                || self.scanned == self.pos
                || self.text.as_bytes()[self.scanned..].contains(&b'\n'))
                && let Some((len, lines)) =
                    parse_row(&self.text[self.pos..], self.eof, &mut self.parsed)
            {
                let (line, start) = (self.line, self.pos);
                self.line += lines;
                self.pos += len;
                self.scanned = self.pos;
                return Ok(Some(Row {
                    line,
                    text: &self.text[start..],
                    spans: &self.parsed.spans,
                    unescaped: &self.parsed.unescaped,
                }));
            }
            self.scanned = self.text.len();
            if self.eof {
                return Ok(None);
            }
            self.read_chunk()?;
        }
    }

    fn read_chunk(&mut self) -> io::Result<()> {
        self.text.drain(..self.pos);
        self.scanned -= self.pos;
        self.pos = 0;
        if !self.stream.next(&mut self.text)? {
            self.eof = true;
        } else if std::mem::take(&mut self.first) {
            let cut = self.text.len() - text::remove_bom(&self.text).len();
            self.bom_lines = self.text[..cut].matches('\n').count() as u64;
            self.line += self.bom_lines;
            self.text.drain(..cut);
        }
        Ok(())
    }
}

/// One row of papaparse's parse loop at the start of `s`, into `row`. Returns the row's
/// length in bytes and the newlines in it, or `None` when `s` is empty, or when the row may
/// continue past the end of `s` and more input is coming (papaparse's `ignoreLastRow`).
fn parse_row(s: &str, eof: bool, row: &mut Parsed) -> Option<(usize, u64)> {
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    row.fields = 0;
    row.spans.clear();
    row.unescaped.clear();
    let mut cursor = 0;
    // The newlines in quoted fields so far: any other newline ends the row.
    let mut lines = 0;
    loop {
        if b.get(cursor) == Some(&b'"') {
            let start = cursor + 1;
            let mut q = cursor;
            let mut doubled = false;
            loop {
                let Some(next) = scan(b, q + 1, |v| eq(v, b'"') | eq(v, b'\n')) else {
                    if !eof {
                        return None;
                    }
                    // No closing quote: the rest of the input is the field, quotes as written.
                    row.push(start, s.len());
                    return Some((s.len(), lines));
                };
                q = next;
                // A newline before the closing quote is the field's.
                if b[q] == b'\n' {
                    lines += 1;
                    continue;
                }
                if q == s.len() - 1 {
                    if !eof {
                        return None;
                    }
                    row.push_quoted(s, start, q, doubled);
                    return Some((s.len(), lines));
                }
                if b[q + 1] == b'"' {
                    q += 1;
                    doubled = true;
                    continue;
                }
                // A closing quote is followed by optional JS whitespace and `,` or `\n`.
                let after = &s[q + 1..];
                let end = if matches!(b[q + 1], b',' | b'\n') {
                    q + 1
                } else {
                    q + 1 + after.len()
                        - after.trim_start_matches(|c| c != '\n' && text::is_space(c)).len()
                };
                // Otherwise not a closing quote: the field goes on.
                if let Some(&c @ (b',' | b'\n')) = b.get(end) {
                    row.push_quoted(s, start, q, doubled);
                    if c == b'\n' {
                        return Some((end + 1, lines + 1));
                    }
                    cursor = end + 1;
                    break;
                }
            }
            continue;
        }
        // Most cells of a wide file are empty, many of them in a row.
        let empty = scan(b, cursor, |v| v ^ (ONES * u64::from(b','))).unwrap_or(b.len()) - cursor;
        if empty > 0 {
            row.fields += empty;
            cursor += empty;
            continue;
        }
        match scan(b, cursor, |v| eq(v, b',') | eq(v, b'\n')) {
            Some(comma) if b[comma] == b',' => {
                row.push(cursor, comma);
                cursor = comma + 1;
            }
            Some(n) => {
                row.push(cursor, n);
                return Some((n + 1, lines + 1));
            }
            None if !eof => return None,
            None => {
                row.push(cursor, s.len());
                return Some((s.len(), lines));
            }
        }
    }
}

/// A byte in each of a word's eight bytes.
const ONES: u64 = u64::from_le_bytes([1; 8]);

/// The bytes of `v` equal to `c`, as their top bit: exact at the lowest one, which is all
/// `scan` reads (a borrow can mark a byte above a match).
fn eq(v: u64, c: u8) -> u64 {
    let x = v ^ (ONES * u64::from(c));
    x.wrapping_sub(ONES) & !x & (ONES << 7)
}

/// The first byte of `b` from `from` on that `hit` marks, eight bytes at a time: `hit` maps
/// eight bytes, the first lowest, to a word whose lowest set bit is in the first marked
/// byte. Std's memchr reads byte by byte up to its alignment, and most fields are shorter.
fn scan(b: &[u8], from: usize, hit: impl Fn(u64) -> u64) -> Option<usize> {
    let mut i = from;
    while let Some(word) = b.get(i..i + 8) {
        let m = hit(u64::from_le_bytes(word.try_into().expect("eight bytes")));
        if m != 0 {
            return Some(i + m.trailing_zeros() as usize / 8);
        }
        i += 8;
    }
    let mut last = [0; 8];
    last[..b.len() - i].copy_from_slice(&b[i..]);
    Some(i + hit(u64::from_le_bytes(last)).trailing_zeros() as usize / 8).filter(|&p| p < b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Each row's fields and line.
    type Rows = Vec<(Vec<String>, u64)>;

    fn read(bytes: &[u8], chunk: usize) -> (Rows, u64) {
        let mut reader = Reader::with_chunk(bytes, chunk);
        let mut rows = Vec::new();
        while let Some(row) = reader.next_row().unwrap() {
            let line = row.line;
            let non_empty: Vec<(usize, String)> =
                row.non_empty().map(|(i, field)| (i, field.into())).collect();
            let mut fields = vec![String::new(); reader.parsed.fields];
            for (i, field) in non_empty {
                fields[i] = field;
            }
            rows.push((fields, line));
        }
        (rows, reader.bom_lines)
    }

    /// The corpus's golden files were written by papaparse itself; their `errors` half is
    /// the validator's concern.
    #[test]
    fn papaparse_rows() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reader");
        let mut cases = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().unwrap() != "csv" {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let golden: Value = serde_json::from_str(
                &std::fs::read_to_string(path.with_extension("json")).unwrap(),
            )
            .unwrap();
            for chunk in [1, 1024, 65536] {
                let (rows, _) = read(&bytes, chunk);
                let fields: Vec<&Vec<String>> = rows.iter().map(|r| &r.0).collect();
                let expected = &golden[chunk.to_string()]["rows"];
                assert!(json!(fields) == *expected, "{} at chunk {chunk}", path.display());
            }
            cases += 1;
        }
        assert!(cases > 0);
    }

    #[test]
    fn cells_after_commas() {
        // The empty-cell scan compares eight bytes with commas at once.
        let (rows, _) = read("a,,-5,\u{5e1}x\n".as_bytes(), 65536);
        assert_eq!(rows, [(vec!["a".into(), String::new(), "-5".into(), "\u{5e1}x".into()], 1)]);
    }

    #[test]
    fn physical_lines() {
        let lines =
            |bytes: &[u8], chunk| read(bytes, chunk).0.iter().map(|r| r.1).collect::<Vec<_>>();
        assert_eq!(lines(b"a,b\n\n\"c\n\nd\"\ne\n", 65536), [1, 2, 3, 6]);
        assert_eq!(lines("\u{feff}\n\n\na,b\n".as_bytes(), 65536), [4]);
        assert_eq!(lines("\u{feff}\n\n\na,b\n".as_bytes(), 1), [1, 2, 3, 4]);
        assert_eq!(read("\u{feff}\n\n\na,b\n".as_bytes(), 65536).1, 3);
    }
}
