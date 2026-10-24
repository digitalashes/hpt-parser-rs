//! The top-level object of a JSON file read as `@streamparser/json` 0.0.21 reads it where
//! the CMS validator configures it (`paths`, `keepStack: false`), from `text::Stream`
//! chunks with the BOM removed from the first one. Values are pulled: `next` reads until a
//! value at one of the paths is complete. Numbers keep the text they are written with.

use crate::{Aliases, Error, text};
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// As written: `1.50` stays `1.50`.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Object),
}

impl Value {
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            // Most lookups are of optional members that are not there.
            Value::Object(o) if o.lengths & length_bit(name) != 0 => {
                o.members.iter().find(|(k, _)| k == name).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    /// In first-seen order (see `set`).
    pub members: Vec<(String, Value)>,
    /// A bit for each name length among the members, modulo 64.
    lengths: u64,
}

impl Object {
    /// Assigns a member: a repeated name replaces the value in place, as JS does.
    pub fn set(&mut self, name: String, value: Value) {
        if let Some(member) = self.members.iter_mut().find(|(k, _)| *k == name) {
            member.1 = value;
        } else {
            self.lengths |= length_bit(&name);
            self.members.push((name, value));
        }
    }
}

fn length_bit(name: &str) -> u64 {
    1 << (name.len() % 64)
}

/// Where a value sits in its parent.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Name(String),
    Index(usize),
}

/// The values at `paths`, each a member name of the top-level object (`&["a"]`), or one
/// with `*` after it for the elements of the array the member holds (`&["b", "*"]`): a value
/// there that is not an array is an error. The paths name members, so a top-level value that
/// is not an object is an error too. Values elsewhere are checked but never built, and
/// nothing after the top-level object is read.
pub struct Parser<R> {
    stream: text::Stream<R>,
    /// The chunk being read, up to byte `at`; `position` is where it starts.
    text: String,
    at: usize,
    position: Position,
    first: bool,
    paths: &'static [&'static [&'static str]],
    /// The containers open, the top-level object first.
    open: Vec<Open>,
    /// Whether a value is complete, so a comma or the end of its container comes next.
    after: bool,
    /// Survives the string it was read in, as upstream's does.
    high_surrogate: Option<u32>,
    aliases: Aliases,
    /// A bit for each alias length, as `Object::lengths`: most names skip the aliases.
    alias_lengths: u64,
    /// The aliases met and not yet taken by `aliases_used`.
    used: BTreeMap<String, String>,
    spare: Spare,
}

/// An open array or object.
struct Open {
    /// Built only while it or a container it is in is emitted: elsewhere `keepStack: false`
    /// deletes each member once it is complete.
    value: Option<Value>,
    array: bool,
    /// Whether it is the array whose elements are emitted.
    items: bool,
    /// An array element's index or an object member's name.
    key: Key,
    /// Whether `key` is a name read through an alias.
    aliased: bool,
}

/// A value read: `None` while it is a container still open, `Some(None)` when complete but
/// not built.
type Got = Option<Option<Value>>;

impl<R: Read> Parser<R> {
    pub fn new(input: R, paths: &'static [&'static [&'static str]]) -> Self {
        Self::with_chunk(input, paths, text::CHUNK)
    }

    pub fn with_chunk(input: R, paths: &'static [&'static [&'static str]], chunk: usize) -> Self {
        Parser {
            stream: text::Stream::new(input, chunk),
            text: String::new(),
            at: 0,
            position: Position { line: 1, column: 1 },
            first: true,
            paths,
            open: Vec::new(),
            after: false,
            high_surrogate: None,
            aliases: Aliases::default(),
            alias_lengths: 0,
            used: BTreeMap::new(),
            spare: Spare::default(),
        }
    }

    /// The next value at a path, `None` once the top-level object is complete.
    pub fn next(&mut self) -> Result<Option<(Key, Value)>, Error> {
        while !(self.after && self.open.is_empty()) {
            let read = if self.after { self.close()? } else { self.start()? };
            self.after = read.is_some();
            if let Some(value) = read
                && let Some(emitted) = self.complete(value)
            {
                return Ok(Some(emitted));
            }
        }
        Ok(None)
    }

    /// Member names to read as other names (see [`crate::Parser::with_aliases`]).
    pub fn set_aliases(&mut self, aliases: Aliases) {
        self.alias_lengths = (aliases.0.iter()).fold(0, |l, a| l | length_bit(&a.key));
        self.aliases = aliases;
    }

    /// The aliases met since the last call.
    pub fn aliases_used(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.used
    }

    /// Hands back a value `next` returned, its buffers reused for the values built next.
    pub fn recycle(&mut self, value: Value) {
        self.spare.keep(value);
    }

    /// Hands back the buffers of values `next` returned, taken apart elsewhere.
    #[cfg(feature = "parquet")]
    pub fn reuse(&mut self, mut spare: Spare) {
        self.spare.strings.append(&mut spare.strings);
        self.spare.arrays.append(&mut spare.arrays);
        self.spare.objects.append(&mut spare.objects);
    }

    /// Reads a value, all of it but for a container that stays open, its first member's name
    /// read then.
    fn start(&mut self) -> Result<Got, Error> {
        let b = self.peek()?;
        let Some(parent) = self.open.last() else {
            return match b {
                Some(b'{') => self.open(false, false, false),
                Some(_) => Err(self.error("the top-level value is not an object".into())),
                None => Err(self.expected("'{'")),
            };
        };
        let build = parent.value.is_some() || self.emits();
        let items = self.path(true);
        if let Some(name) = items
            && b != Some(b'[')
            && b.is_some()
        {
            return Err(self.error(format!("{name} is not an array")));
        }
        let value = match b {
            Some(b'[') => return self.open(true, build, items.is_some()),
            Some(b'{') => return self.open(false, build, false),
            Some(b'"') => {
                self.at += 1;
                Value::String(self.string()?)
            }
            Some(b'-' | b'0'..=b'9') => Value::Number(self.number()?),
            Some(b't') => self.literal("true", Value::Bool(true))?,
            Some(b'f') => self.literal("false", Value::Bool(false))?,
            Some(b'n') => self.literal("null", Value::Null)?,
            _ => return Err(self.expected("a value")),
        };
        if build {
            return Ok(Some(Some(value)));
        }
        self.spare.keep(value);
        Ok(Some(None))
    }

    /// Opens the array or object at `at`, built when `build`.
    fn open(&mut self, array: bool, build: bool, items: bool) -> Result<Got, Error> {
        self.at += 1;
        let spare = &mut self.spare;
        let value = build.then(|| {
            if array {
                Value::Array(spare.arrays.pop().unwrap_or_default())
            } else {
                Value::Object(spare.objects.pop().unwrap_or_default())
            }
        });
        let key = if array { Key::Index(0) } else { Key::Name(String::new()) };
        self.open.push(Open { value, array, items, key, aliased: false });
        match self.peek()? {
            Some(b']') if array => {}
            Some(b'}') if !array => {}
            _ if array => return Ok(None),
            _ => return self.name("a member name or '}'").map(|()| None),
        }
        self.at += 1;
        Ok(self.open.pop().map(|open| open.value))
    }

    /// Reads what follows a complete value: a comma, and in an object the next member's name,
    /// or the end of its container.
    fn close(&mut self) -> Result<Got, Error> {
        let open = self.open.last().expect("a value is complete in a container");
        let (array, built) = (open.array, open.value.is_some());
        match self.peek()? {
            Some(b',') => {
                self.at += 1;
                if let Some(Open { key: Key::Index(i), .. }) = self.open.last_mut() {
                    *i += 1;
                    return Ok(None);
                }
                // Upstream counts the members it does not keep as gone, so an object not built
                // takes a comma before its end.
                if built || self.peek()? != Some(b'}') {
                    return self.name("a member name").map(|()| None);
                }
            }
            Some(b']') if array => {}
            Some(b'}') if !array => {}
            _ => return Err(self.expected(if array { "',' or ']'" } else { "',' or '}'" })),
        }
        self.at += 1;
        Ok(self.open.pop().map(|open| open.value))
    }

    /// A complete value: emitted, or kept in its container when that is built.
    fn complete(&mut self, value: Option<Value>) -> Option<(Key, Value)> {
        let value = value?;
        let emit = self.emits();
        let open = self.open.last_mut()?;
        let key = match &mut open.key {
            Key::Index(i) => Key::Index(*i),
            Key::Name(name) => Key::Name(std::mem::take(name)),
        };
        if emit {
            return Some((key, value));
        }
        match (&mut open.value, key) {
            (Some(Value::Array(items)), _) => items.push(value),
            // A member read through an alias gives way to one under its own name.
            (Some(Value::Object(o)), Key::Name(name)) => {
                if !(open.aliased && o.members.iter().any(|(k, _)| *k == name)) {
                    o.set(name, value);
                }
            }
            _ => unreachable!("a built value is emitted or in a built container"),
        }
        None
    }

    /// Whether the value in the open container is emitted.
    fn emits(&self) -> bool {
        self.open.last().is_some_and(|open| open.items) || self.path(false).is_some()
    }

    /// The name of the member being read in the top-level object when it is a path: `[name]`,
    /// or with `items` `[name, "*"]`.
    fn path(&self, items: bool) -> Option<&'static str> {
        let [Open { key: Key::Name(name), .. }] = self.open.as_slice() else {
            return None;
        };
        self.paths.iter().find_map(|path| match **path {
            [n] if !items && n == name => Some(n),
            [n, "*"] if items && n == name => Some(n),
            _ => None,
        })
    }

    /// Reads a member's name and the colon after it, for the object open.
    fn name(&mut self, expected: &str) -> Result<(), Error> {
        if self.peek()? != Some(b'"') {
            return Err(self.expected(expected));
        }
        self.at += 1;
        let mut name = self.string()?;
        let mut aliased = false;
        if self.alias_lengths & length_bit(&name) != 0
            && let Some(alias) = self.aliases.get(&name)
        {
            aliased = true;
            name.clear();
            name.push_str(alias.take(&mut self.used));
        }
        if self.peek()? != Some(b':') {
            return Err(self.expected("':'"));
        }
        self.at += 1;
        let open = self.open.last_mut().expect("an object is open");
        if let Key::Name(old) = std::mem::replace(&mut open.key, Key::Name(name))
            && old.capacity() > 0
        {
            self.spare.strings.push(old);
        }
        open.aliased = aliased;
        Ok(())
    }

    /// A string, its opening quote taken.
    fn string(&mut self) -> Result<String, Error> {
        let mut s = self.spare.strings.pop().unwrap_or_default();
        s.clear();
        loop {
            // A run of plain bytes, which ends at ASCII, so at a char boundary.
            let b = self.text.as_bytes();
            let plain = |v| text::eq(v, b'"') | text::eq(v, b'\\') | text::below(v, 0x20);
            let end = text::scan(b, self.at, plain).unwrap_or(b.len());
            s.push_str(&self.text[self.at..end]);
            self.at = end;
            match self.raw()? {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.at += 1;
                    self.escape(&mut s)?;
                }
                Some(c) if c < 0x20 => {
                    let message = format!("unescaped {:?} in a string", char::from(c));
                    return Err(self.error(message));
                }
                Some(_) => {}
                None => return Err(self.expected("'\"'")),
            }
        }
    }

    /// An escape in a string, its backslash taken.
    fn escape(&mut self, s: &mut String) -> Result<(), Error> {
        let c = match self.raw()? {
            Some(b'u') => {
                self.at += 1;
                let mut code = 0;
                for _ in 0..4 {
                    let digit = self.raw()?.and_then(|b| char::from(b).to_digit(16));
                    let Some(digit) = digit else { return Err(self.expected("a hex digit")) };
                    code = code * 16 + digit;
                    self.at += 1;
                }
                self.push_unicode(code, s);
                return Ok(());
            }
            Some(n @ (b'"' | b'\\' | b'/')) => char::from(n),
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            _ => return Err(self.expected("an escape")),
        };
        self.at += 1;
        s.push(c);
        Ok(())
    }

    /// A completed `\u` escape. Surrogates pair across escapes; a lone one becomes U+FFFD,
    /// and an escape that fails to complete a pending high surrogate is dropped with it.
    fn push_unicode(&mut self, code: u32, s: &mut String) {
        let c = match self.high_surrogate.take() {
            None if (0xd800..=0xdbff).contains(&code) => {
                self.high_surrogate = Some(code);
                return;
            }
            None => char::from_u32(code),
            Some(high) if (0xdc00..=0xdfff).contains(&code) => {
                char::from_u32(0x10000 + ((high - 0xd800) << 10) + (code - 0xdc00))
            }
            Some(_) => None,
        };
        s.push(c.unwrap_or('\u{fffd}'));
    }

    /// A number: `-`, an integer without leading zeros, then a fraction and an exponent if
    /// there. What follows is the next token's, so `01` is two numbers.
    fn number(&mut self) -> Result<String, Error> {
        let mut n = self.spare.strings.pop().unwrap_or_default();
        n.clear();
        if self.raw()? == Some(b'-') {
            self.step(&mut n);
        }
        if self.raw()? == Some(b'0') {
            self.step(&mut n);
        } else {
            self.digits(&mut n)?;
        }
        if self.raw()? == Some(b'.') {
            self.step(&mut n);
            self.digits(&mut n)?;
        }
        if let Some(b'e' | b'E') = self.raw()? {
            self.step(&mut n);
            if let Some(b'+' | b'-') = self.raw()? {
                self.step(&mut n);
            }
            self.digits(&mut n)?;
        }
        Ok(n)
    }

    /// One digit or more.
    fn digits(&mut self, n: &mut String) -> Result<(), Error> {
        if !matches!(self.raw()?, Some(b'0'..=b'9')) {
            return Err(self.expected("a digit"));
        }
        loop {
            let rest = &self.text.as_bytes()[self.at..];
            let run = rest.iter().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
            n.push_str(&self.text[self.at..self.at + run]);
            self.at += run;
            if self.at < self.text.len() || !self.refill()? {
                return Ok(());
            }
        }
    }

    /// The ASCII byte at `at`, taken into `s`.
    fn step(&mut self, s: &mut String) {
        s.push(char::from(self.text.as_bytes()[self.at]));
        self.at += 1;
    }

    /// `true`, `false` or `null`, from its first letter.
    fn literal(&mut self, word: &'static str, value: Value) -> Result<Value, Error> {
        for &b in word.as_bytes() {
            if self.raw()? != Some(b) {
                return Err(self.expected(word));
            }
            self.at += 1;
        }
        Ok(value)
    }

    /// The next byte after whitespace, not taken; `None` at the end of the input.
    fn peek(&mut self) -> Result<Option<u8>, Error> {
        while let Some(b) = self.raw()? {
            if !matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                return Ok(Some(b));
            }
            self.at += 1;
        }
        Ok(None)
    }

    /// The byte at `at`, not taken; `None` at the end of the input.
    fn raw(&mut self) -> Result<Option<u8>, Error> {
        while self.at == self.text.len() {
            if !self.refill()? {
                return Ok(None);
            }
        }
        Ok(Some(self.text.as_bytes()[self.at]))
    }

    /// The next chunk in `text`; false at the end of the input.
    fn refill(&mut self) -> Result<bool, Error> {
        self.position.advance(&self.text);
        self.text.clear();
        self.at = 0;
        if !self.stream.next(&mut self.text)? {
            return Ok(false);
        }
        if std::mem::take(&mut self.first) {
            let cut = self.text.len() - text::remove_bom(&self.text).len();
            // What goes with the BOM counts for positions; the BOM itself takes no column.
            self.position.advance(self.text[..cut].strip_prefix('\u{feff}').unwrap_or_default());
            self.text.drain(..cut);
        }
        Ok(true)
    }

    /// An error at `at`.
    fn error(&self, message: String) -> Error {
        let mut position = self.position;
        position.advance(&self.text[..self.at]);
        Error::Json { line: position.line, column: position.column, message }
    }

    /// `expected {what}, found` the char at `at` or the end of the input.
    fn expected(&self, what: &str) -> Error {
        let found = match self.text[self.at..].chars().next() {
            Some(c) => format!("{c:?}"),
            None => "the end of the input".into(),
        };
        self.error(format!("expected {what}, found {found}"))
    }
}

/// Line and column, 1-based, the column in chars.
#[derive(Clone, Copy)]
struct Position {
    line: u64,
    column: u64,
}

impl Position {
    fn advance(&mut self, text: &str) {
        match text.rfind('\n') {
            Some(last) => {
                self.line += text.matches('\n').count() as u64;
                self.column = 1 + text[last + 1..].chars().count() as u64;
            }
            None => self.column += text.chars().count() as u64,
        }
    }
}

/// The buffers of values the caller is done with, for the values built next: without them
/// every string, array and object of every charge item is allocated and freed. Strings are
/// emptied when they are taken.
#[derive(Default)]
pub struct Spare {
    strings: Vec<String>,
    arrays: Vec<Vec<Value>>,
    objects: Vec<Object>,
}

impl Spare {
    pub fn keep(&mut self, value: Value) {
        match value {
            Value::String(s) | Value::Number(s) => self.strings.push(s),
            Value::Array(mut items) => {
                items.drain(..).for_each(|v| self.keep(v));
                self.arrays.push(items);
            }
            Value::Object(mut o) => {
                for (name, v) in o.members.drain(..) {
                    self.strings.push(name);
                    self.keep(v);
                }
                o.lengths = 0;
                self.objects.push(o);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATHS: &[&[&str]] = &[&["a"], &["b", "*"]];

    fn values(input: &str, chunk: usize) -> Result<Vec<(Key, Value)>, Error> {
        let mut parser = Parser::with_chunk(input.as_bytes(), PATHS, chunk);
        let mut values = Vec::new();
        while let Some(value) = parser.next()? {
            values.push(value);
        }
        Ok(values)
    }

    fn position(input: &str, chunk: usize) -> (u64, u64) {
        match values(input, chunk) {
            Err(Error::Json { line, column, .. }) => (line, column),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn paths_and_numbers() {
        let input =
            "{\"x\": [1], \"b\": [1.50, -0e+1, \"s\", true, false, null], \"a\": {\"n\": 10}} x";
        for chunk in [1, 3, 65536] {
            let number = |n: &str| Value::Number(n.into());
            let mut object = Object::default();
            object.set("n".into(), number("10"));
            assert_eq!(
                values(input, chunk).unwrap(),
                [
                    (Key::Index(0), number("1.50")),
                    (Key::Index(1), number("-0e+1")),
                    (Key::Index(2), Value::String("s".into())),
                    (Key::Index(3), Value::Bool(true)),
                    (Key::Index(4), Value::Bool(false)),
                    (Key::Index(5), Value::Null),
                    (Key::Name("a".into()), Value::Object(object)),
                ]
            );
        }
    }

    #[test]
    fn positions() {
        for chunk in [1, 2, 65536] {
            assert_eq!(position("{\n\"a\": tru}", chunk), (2, 9));
            assert_eq!(position("{\n\"é\" {", chunk), (2, 5));
            assert_eq!(position("\u{feff}\n\n{\"a\": ", chunk), (3, 7));
            assert_eq!(position("\u{feff}x", chunk), (1, 1));
            assert_eq!(position("{\"b\": [1, 2", chunk), (1, 12));
            assert_eq!(position(" \n ", chunk), (2, 2));
            // A token out of place: where it starts.
            assert_eq!(position("{\"a\" 1}", chunk), (1, 6));
            assert_eq!(position("{\"a\": 1 \"b\": 2}", chunk), (1, 9));
        }
    }

    #[test]
    fn escapes() {
        let input = r#"{"a": "\" \\ \/ \b\f\n\r\t \u00e9 \ud83d\ude00 \udc00 \ud83d\u0041."}"#;
        for chunk in [1, 65536] {
            let want = "\" \\ / \u{8}\u{c}\n\r\t \u{e9} \u{1f600} \u{fffd} \u{fffd}.";
            assert_eq!(
                values(input, chunk).unwrap(),
                [(Key::Name("a".into()), Value::String(want.into()))]
            );
        }
    }

    #[test]
    fn messages() {
        let message = |input: &str| match values(input, 65536) {
            Err(Error::Json { message, .. }) => message,
            other => panic!("{other:?}"),
        };
        for (input, want) in [
            ("{\"a\": tru}", "expected true, found '}'"),
            ("{\"a\" 1}", "expected ':', found '1'"),
            ("{\"a\": 1 \"b\": 2}", "expected ',' or '}', found '\"'"),
            ("{\"b\": [1 2]}", "expected ',' or ']', found '2'"),
            ("{\"b\": [1,]}", "expected a value, found ']'"),
            ("{\"a\": {\"n\": 1,}}", "expected a member name, found '}'"),
            ("{1}", "expected a member name or '}', found '1'"),
            ("{\"a\": {]}", "expected a member name or '}', found ']'"),
            ("{\"a\": \"\\u12x\"}", "expected a hex digit, found 'x'"),
            ("{\"a\": \"\\x\"}", "expected an escape, found 'x'"),
            ("{\"a\": 1.}", "expected a digit, found '}'"),
            ("{\"a\": -é}", "expected a digit, found 'é'"),
            ("{\"a\": \"\t\"}", "unescaped '\\t' in a string"),
            ("{\"a\": \"x", "expected '\"', found the end of the input"),
            ("{\"a\": 1", "expected ',' or '}', found the end of the input"),
            ("", "expected '{', found the end of the input"),
        ] {
            assert_eq!(message(input), want, "{input}");
        }
    }

    /// Upstream's: a comma before the end of an object not built, two numbers in `01`, the
    /// values before an error, nothing read after the top-level object.
    #[test]
    fn quirks() {
        let a = |n: &str| (Key::Name("a".into()), Value::Number(n.into()));
        assert_eq!(values("{\"x\": {\"n\": 1,}, \"a\": 1,} [", 65536).unwrap(), [a("1")]);
        assert!(values("{\"a\": 01}", 65536).is_err());
        let mut parser = Parser::new("{\"a\": 1, \"a\": 2, \"a\": x".as_bytes(), PATHS);
        assert_eq!(parser.next().unwrap(), Some(a("1")));
        assert_eq!(parser.next().unwrap(), Some(a("2")));
        assert!(parser.next().is_err());
    }

    #[test]
    fn closing_tokens() {
        // A comma before a closing bracket or brace, and a bracket closing an object, a brace
        // an array, in values that are kept.
        for input in
            [r#"{"b": [1,]}"#, r#"{"a": {"n": 1,}}"#, r#"{"b": [1}}"#, r#"{"a": {"n": 1]}"#]
        {
            assert!(values(input, 65536).is_err(), "{input}");
        }
    }

    #[test]
    fn items_must_be_an_array() {
        for input in ["{\"b\": {\"a\": 1}}", "{\"b\": 5}", "{\"b\": null}"] {
            let Err(Error::Json { message, .. }) = values(input, 65536) else {
                panic!("{input}");
            };
            assert_eq!(message, "b is not an array");
        }
        assert_eq!(values("{\"b\": []}", 65536).unwrap(), []);
    }

    #[test]
    fn top_level_must_be_an_object() {
        for input in ["[{\"a\": 1}]", " 5", "\"a\"", "null"] {
            let Err(Error::Json { message, .. }) = values(input, 65536) else {
                panic!("{input}");
            };
            assert_eq!(message, "the top-level value is not an object", "{input}");
        }
    }
}
