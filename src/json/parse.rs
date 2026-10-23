//! `@streamparser/json` 0.0.21's tokenizer and token parser as the CMS validator configures
//! them (`paths`, `keepStack: false`), fed `text::Stream` chunks with the BOM removed from
//! the first one. Values are pulled: `next` reads chunks until a value at one of the paths
//! is complete. Numbers keep the text they are written with.

use crate::{Aliases, Error, text};
use std::collections::{BTreeMap, VecDeque};
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

#[derive(Clone, Debug, Default)]
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

impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        self.members == other.members
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

/// Why writing a chunk stopped early.
enum End {
    /// The top-level value is complete (`onEnd`); the rest of the input is never read.
    Done,
    /// The parser's error message (`onError`).
    Invalid(String),
}

/// `Err` ends parsing.
type Step = Result<(), End>;

/// The values at `paths`, in `@streamparser/json` selector syntax without the `$`
/// (`&["a", "*"]` is `$.a.*`), with names or `*` as selectors. `*` selects an array's
/// elements: the value it is under is an error when it is not an array. The paths name
/// members, so a top-level value that is not an object is an error too. Values elsewhere are
/// checked but never built.
pub struct Parser<R> {
    stream: text::Stream<R>,
    text: String,
    first: bool,
    ended: bool,
    tokens: Tokens,
}

impl<R: Read> Parser<R> {
    pub fn new(input: R, paths: &'static [&'static [&'static str]]) -> Self {
        Self::with_chunk(input, paths, text::CHUNK)
    }

    pub fn with_chunk(input: R, paths: &'static [&'static [&'static str]], chunk: usize) -> Self {
        Parser {
            stream: text::Stream::new(input, chunk),
            text: String::new(),
            first: true,
            ended: false,
            tokens: Tokens {
                paths,
                // ponytail: depths past 63 share a bit, which only costs a path comparison
                item_depths: (paths.iter())
                    .filter_map(|path| match *path {
                        [parent @ .., "*"] => Some(1 << parent.len().min(63)),
                        _ => None,
                    })
                    .fold(0, |depths, depth| depths | depth),
                queue: VecDeque::new(),
                position: Position { line: 1, column: 1 },
                state: State::START,
                buffer: String::new(),
                unicode: 0,
                high_surrogate: None,
                expect: Expect::VALUE,
                key: None,
                aliased: false,
                aliases: Aliases::default(),
                alias_lengths: 0,
                used: BTreeMap::new(),
                value: None,
                stack: Vec::new(),
                spare: Spare::default(),
            },
        }
    }

    /// The next value at a path, `None` once the top-level value is complete.
    pub fn next(&mut self) -> Result<Option<(Key, Value)>, Error> {
        loop {
            if let Some(value) = self.tokens.queue.pop_front() {
                return Ok(Some(value));
            }
            if self.ended {
                return Ok(None);
            }
            self.text.clear();
            if !self.stream.next(&mut self.text)? {
                self.ended = true;
                if let End::Invalid(message) = self.tokens.end() {
                    return Err(self.tokens.position.error(message));
                }
                continue;
            }
            let mut chunk = self.text.as_str();
            if std::mem::take(&mut self.first) {
                let rest = text::remove_bom(chunk);
                // What goes with the BOM counts for positions; the BOM itself takes no column.
                let cut = &chunk[..chunk.len() - rest.len()];
                self.tokens.position.advance(cut.strip_prefix('\u{feff}').unwrap_or_default());
                chunk = rest;
            }
            let mut i = 0;
            let step = self.tokens.write(chunk, &mut i);
            self.tokens.position.advance(&chunk[..i]);
            match step {
                Ok(()) => {}
                Err(End::Done) => self.ended = true,
                Err(End::Invalid(message)) => return Err(self.tokens.position.error(message)),
            }
        }
    }

    /// Member names to read as other names (see [`crate::Parser::with_aliases`]).
    pub fn set_aliases(&mut self, aliases: Aliases) {
        self.tokens.alias_lengths = (aliases.0.iter()).fold(0, |l, a| l | length_bit(&a.key));
        self.tokens.aliases = aliases;
    }

    /// The aliases met since the last call.
    pub fn aliases_used(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.tokens.used
    }

    /// Hands back a value `next` returned, its buffers reused for the values built next.
    pub fn recycle(&mut self, value: Value) {
        self.tokens.spare.keep(value);
    }
}

/// Line and column, 1-based, the column in chars, where the text read so far ends.
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

    fn error(&self, message: String) -> Error {
        Error::Json { line: self.line, column: self.column, message }
    }
}

/// The tokenizer's state, named as upstream's are in its error texts.
#[expect(
    non_camel_case_types,
    clippy::upper_case_acronyms,
    reason = "upstream's names, printed in its error texts"
)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    START,
    /// `true`, `false` or `null` with this many bytes read: upstream's `TRUE1` to `NULL3`.
    LITERAL(&'static str, usize),
    STRING_DEFAULT,
    STRING_AFTER_BACKSLASH,
    /// Expecting this hex digit, 1 to 4, of a `\u` escape.
    STRING_UNICODE_DIGIT(u32),
    NUMBER_AFTER_INITIAL_MINUS,
    NUMBER_AFTER_INITIAL_ZERO,
    NUMBER_AFTER_INITIAL_NON_ZERO,
    NUMBER_AFTER_FULL_STOP,
    NUMBER_AFTER_DECIMAL,
    NUMBER_AFTER_E,
    NUMBER_AFTER_E_AND_SIGN,
    NUMBER_AFTER_E_AND_DIGIT,
}

impl State {
    fn name(self) -> String {
        match self {
            State::LITERAL(word, read) => format!("{}{read}", word.to_ascii_uppercase()),
            State::STRING_UNICODE_DIGIT(n) => format!("STRING_UNICODE_DIGIT_{n}"),
            state => format!("{state:?}"),
        }
    }
}

/// The token parser's state.
#[expect(clippy::upper_case_acronyms, reason = "upstream's names, printed in its error texts")]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    VALUE,
    KEY,
    COLON,
    COMMA,
}

enum Token {
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Colon,
    Comma,
    Value(Value),
}

impl Token {
    /// `Unexpected ${TokenType[token]} (${JSON.stringify(value)})`.
    fn describe(&self) -> String {
        let (name, json) = match self {
            Token::LeftBrace => ("LEFT_BRACE", "\"{\"".into()),
            Token::RightBrace => ("RIGHT_BRACE", "\"}\"".into()),
            Token::LeftBracket => ("LEFT_BRACKET", "\"[\"".into()),
            Token::RightBracket => ("RIGHT_BRACKET", "\"]\"".into()),
            Token::Colon => ("COLON", "\":\"".into()),
            Token::Comma => ("COMMA", "\",\"".into()),
            Token::Value(Value::Bool(true)) => ("TRUE", "true".into()),
            Token::Value(Value::Bool(false)) => ("FALSE", "false".into()),
            Token::Value(Value::String(s)) => ("STRING", format!("{s:?}")),
            Token::Value(Value::Number(n)) => ("NUMBER", n.clone()),
            Token::Value(_) => ("NULL", "null".into()),
        };
        format!("Unexpected {name} ({json})")
    }
}

/// The buffers of values the caller is done with, for the values built next: without them
/// every string, array and object of every charge item is allocated and freed. Strings are
/// emptied when the tokenizer takes them.
#[derive(Default)]
struct Spare {
    strings: Vec<String>,
    arrays: Vec<Vec<Value>>,
    objects: Vec<Object>,
}

impl Spare {
    fn keep(&mut self, value: Value) {
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

/// An open container's parent, as `push` saves it.
struct Frame {
    key: Option<Key>,
    aliased: bool,
    value: Option<Value>,
    /// Whether the container this frame opened is emitted when it closes.
    emit: bool,
}

struct Tokens {
    paths: &'static [&'static [&'static str]],
    /// A bit for each length of a path's part before `*`: the depths `items_parent` looks at.
    item_depths: u64,
    /// Emitted values not yet pulled.
    queue: VecDeque<(Key, Value)>,
    /// Where the chunks written so far end.
    position: Position,
    // tokenizer
    state: State,
    /// The string or number being read.
    buffer: String,
    unicode: u32,
    /// Survives the string it was read in, as upstream's does.
    high_surrogate: Option<u32>,
    // token parser
    expect: Expect,
    /// An array element's index, an object member's name, or none: what the open container
    /// is, too.
    key: Option<Key>,
    /// Whether `key` is a name read through an alias.
    aliased: bool,
    aliases: Aliases,
    /// A bit for each alias length, as `Object::lengths`: most names skip the aliases.
    alias_lengths: u64,
    /// The aliases met and not yet taken by `aliases_used`.
    used: BTreeMap<String, String>,
    /// The open container, built only while it or an ancestor is emitted: elsewhere
    /// `keepStack: false` deletes each member once it is complete.
    value: Option<Value>,
    stack: Vec<Frame>,
    spare: Spare,
}

impl Tokens {
    /// The tokenizer's `write` loop over one chunk; on `Err`, `i` is the byte it stopped at,
    /// always at a char boundary: every state but `STRING_DEFAULT` stops at the first byte
    /// that is not ASCII.
    #[expect(clippy::too_many_lines, reason = "one state machine, as upstream's")]
    fn write(&mut self, chunk: &str, i: &mut usize) -> Step {
        #[allow(clippy::enum_glob_use, reason = "states read as upstream names them")]
        use State::*;
        let b = chunk.as_bytes();
        while *i < b.len() {
            let n = b[*i];
            match self.state {
                START => match n {
                    b' ' | b'\n' | b'\r' | b'\t' => {}
                    b'{' => self.token(Token::LeftBrace)?,
                    b'}' => self.token(Token::RightBrace)?,
                    b'[' => self.token(Token::LeftBracket)?,
                    b']' => self.token(Token::RightBracket)?,
                    b':' => self.token(Token::Colon)?,
                    b',' => self.token(Token::Comma)?,
                    b't' => self.state = LITERAL("true", 1),
                    b'f' => self.state = LITERAL("false", 1),
                    b'n' => self.state = LITERAL("null", 1),
                    b'"' => {
                        self.buffer.clear();
                        self.state = STRING_DEFAULT;
                    }
                    b'0'..=b'9' | b'-' => {
                        self.buffer.clear();
                        self.buffer.push(n as char);
                        self.state = match n {
                            b'0' => NUMBER_AFTER_INITIAL_ZERO,
                            b'-' => NUMBER_AFTER_INITIAL_MINUS,
                            _ => NUMBER_AFTER_INITIAL_NON_ZERO,
                        };
                    }
                    _ => return Err(self.unexpected(chunk, *i)),
                },
                LITERAL(word, read) => {
                    if n != word.as_bytes()[read] {
                        return Err(self.unexpected(chunk, *i));
                    }
                    if read + 1 < word.len() {
                        self.state = LITERAL(word, read + 1);
                    } else {
                        self.state = START;
                        let value = match word {
                            "true" => Value::Bool(true),
                            "false" => Value::Bool(false),
                            _ => Value::Null,
                        };
                        self.token(Token::Value(value))?;
                    }
                }
                STRING_DEFAULT => match n {
                    b'"' => {
                        self.state = START;
                        let s = Value::String(self.take_buffer());
                        self.token(Token::Value(s))?;
                    }
                    b'\\' => self.state = STRING_AFTER_BACKSLASH,
                    0x20.. => {
                        // A run of plain bytes, which ends at ASCII, so at a char boundary.
                        let run = (b[*i..].iter())
                            .position(|&c| c == b'"' || c == b'\\' || c < 0x20)
                            .unwrap_or(b.len() - *i);
                        self.buffer.push_str(&chunk[*i..*i + run]);
                        *i += run;
                        continue;
                    }
                    _ => return Err(self.unexpected(chunk, *i)),
                },
                STRING_AFTER_BACKSLASH if n == b'u' => {
                    self.unicode = 0;
                    self.state = STRING_UNICODE_DIGIT(1);
                }
                STRING_AFTER_BACKSLASH => {
                    let c = match n {
                        b'"' | b'\\' | b'/' => n as char,
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        _ => return Err(self.unexpected(chunk, *i)),
                    };
                    self.buffer.push(c);
                    self.state = STRING_DEFAULT;
                }
                STRING_UNICODE_DIGIT(k) => {
                    let Some(digit) = (n as char).to_digit(16) else {
                        return Err(self.unexpected(chunk, *i));
                    };
                    self.unicode = self.unicode * 16 + digit;
                    self.state = if k < 4 {
                        STRING_UNICODE_DIGIT(k + 1)
                    } else {
                        self.push_unicode();
                        STRING_DEFAULT
                    };
                }
                NUMBER_AFTER_INITIAL_MINUS => match n {
                    b'0' => self.digit(n, NUMBER_AFTER_INITIAL_ZERO),
                    b'1'..=b'9' => self.digit(n, NUMBER_AFTER_INITIAL_NON_ZERO),
                    _ => return Err(self.unexpected(chunk, *i)),
                },
                NUMBER_AFTER_FULL_STOP => match n {
                    b'0'..=b'9' => self.digit(n, NUMBER_AFTER_DECIMAL),
                    _ => return Err(self.unexpected(chunk, *i)),
                },
                NUMBER_AFTER_E | NUMBER_AFTER_E_AND_SIGN => match n {
                    b'+' | b'-' if self.state == NUMBER_AFTER_E => {
                        self.digit(n, NUMBER_AFTER_E_AND_SIGN);
                    }
                    b'0'..=b'9' => self.digit(n, NUMBER_AFTER_E_AND_DIGIT),
                    _ => return Err(self.unexpected(chunk, *i)),
                },
                NUMBER_AFTER_INITIAL_ZERO
                | NUMBER_AFTER_INITIAL_NON_ZERO
                | NUMBER_AFTER_DECIMAL
                | NUMBER_AFTER_E_AND_DIGIT => match n {
                    b'0'..=b'9' if self.state != NUMBER_AFTER_INITIAL_ZERO => {
                        self.buffer.push(n as char);
                    }
                    b'.' if matches!(
                        self.state,
                        NUMBER_AFTER_INITIAL_ZERO | NUMBER_AFTER_INITIAL_NON_ZERO
                    ) =>
                    {
                        self.digit(n, NUMBER_AFTER_FULL_STOP);
                    }
                    b'e' | b'E' if self.state != NUMBER_AFTER_E_AND_DIGIT => {
                        self.digit(n, NUMBER_AFTER_E);
                    }
                    _ => {
                        // The number ended before this byte, which is read again in START.
                        self.state = START;
                        self.emit_number()?;
                        continue;
                    }
                },
            }
            *i += 1;
        }
        Ok(())
    }

    fn digit(&mut self, n: u8, next: State) {
        self.buffer.push(n as char);
        self.state = next;
    }

    /// The char at byte `i`.
    fn unexpected(&self, chunk: &str, i: usize) -> End {
        let c = chunk[i..].chars().next().expect("a byte is there");
        End::Invalid(format!("Unexpected {c:?} in state {}", self.state.name()))
    }

    /// A completed `\u` escape. Surrogates pair across escapes; a lone one becomes U+FFFD,
    /// and an escape that fails to complete a pending high surrogate is dropped with it.
    fn push_unicode(&mut self) {
        let code = self.unicode;
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
        self.buffer.push(c.unwrap_or('\u{fffd}'));
    }

    fn emit_number(&mut self) -> Step {
        let n = Value::Number(self.take_buffer());
        self.token(Token::Value(n))
    }

    /// The string or number read, a spare buffer taking its place.
    fn take_buffer(&mut self) -> String {
        let spare = self.spare.strings.pop().unwrap_or_default();
        std::mem::replace(&mut self.buffer, spare)
    }

    /// The tokenizer's `end`.
    fn end(&mut self) -> End {
        #[allow(clippy::enum_glob_use, reason = "states read as upstream names them")]
        use State::*;
        match self.state {
            NUMBER_AFTER_INITIAL_ZERO
            | NUMBER_AFTER_INITIAL_NON_ZERO
            | NUMBER_AFTER_DECIMAL
            | NUMBER_AFTER_E_AND_DIGIT => {
                (self.emit_number().err()).unwrap_or_else(|| self.end_tokens())
            }
            START => self.end_tokens(),
            state => End::Invalid(format!(
                "the input ends in the middle of a token (state {})",
                state.name()
            )),
        }
    }

    /// The token parser's `end`: a complete top-level value ends parsing before the input
    /// does, so here there is none.
    fn end_tokens(&self) -> End {
        End::Invalid(if self.stack.is_empty() {
            "the input holds no JSON value".into()
        } else {
            format!("the input ends in the middle of the document (state {:?})", self.expect)
        })
    }

    /// The token parser's `write`. At the top level a value ends parsing before `expect`
    /// is read again, so it is set as inside a container.
    fn token(&mut self, token: Token) -> Step {
        if self.stack.is_empty() && !matches!(token, Token::LeftBrace) {
            return Err(End::Invalid("the top-level value is not an object".into()));
        }
        if self.expect == Expect::VALUE
            && self.item_depths & (1 << self.stack.len().min(63)) != 0
            && matches!(token, Token::Value(_) | Token::LeftBrace)
            && let Some(name) = self.items_parent()
        {
            return Err(End::Invalid(format!("{name} is not an array")));
        }
        let array = matches!(self.key, Some(Key::Index(_)));
        match (self.expect, token) {
            (Expect::VALUE, Token::Value(value)) => {
                self.expect = Expect::COMMA;
                let emit = self.should_emit();
                self.emit(Some(value), emit)
            }
            (Expect::VALUE, token @ (Token::LeftBrace | Token::LeftBracket)) => {
                let emit = self.should_emit();
                let build = emit || self.value.is_some();
                let key = self.key.take();
                self.stack.push(Frame {
                    key,
                    aliased: self.aliased,
                    value: self.value.take(),
                    emit,
                });
                if matches!(token, Token::LeftBracket) {
                    self.key = Some(Key::Index(0));
                    let spare = &mut self.spare.arrays;
                    self.value = build.then(|| Value::Array(spare.pop().unwrap_or_default()));
                } else {
                    self.expect = Expect::KEY;
                    let spare = &mut self.spare.objects;
                    self.value = build.then(|| Value::Object(spare.pop().unwrap_or_default()));
                }
                Ok(())
            }
            // `this.value.length === 0`, which only holds right after `[`.
            (Expect::VALUE, Token::RightBracket) if self.key == Some(Key::Index(0)) => self.pop(),
            (Expect::KEY, Token::Value(Value::String(mut name))) => {
                self.aliased = false;
                if self.alias_lengths & length_bit(&name) != 0
                    && let Some(alias) = self.aliases.get(&name)
                {
                    self.aliased = true;
                    name.clear();
                    name.push_str(alias.take(&mut self.used));
                }
                self.key = Some(Key::Name(name));
                self.expect = Expect::COLON;
                Ok(())
            }
            // `Object.keys(this.value).length === 0`: members not kept count as gone.
            (Expect::KEY, Token::RightBrace)
                if self
                    .value
                    .as_ref()
                    .is_none_or(|v| matches!(v, Value::Object(o) if o.members.is_empty())) =>
            {
                self.pop()
            }
            (Expect::COLON, Token::Colon) => {
                self.expect = Expect::VALUE;
                Ok(())
            }
            (Expect::COMMA, Token::Comma) => {
                if let Some(Key::Index(i)) = &mut self.key {
                    *i += 1;
                    self.expect = Expect::VALUE;
                } else {
                    self.expect = Expect::KEY;
                }
                Ok(())
            }
            (Expect::COMMA, Token::RightBrace) if !array => self.pop(),
            (Expect::COMMA, Token::RightBracket) if array => self.pop(),
            (expect, token) => {
                Err(End::Invalid(format!("{} in state {expect:?}", token.describe())))
            }
        }
    }

    fn pop(&mut self) -> Step {
        let value = self.value.take();
        let frame = self.stack.pop().expect("a container is open");
        self.key = frame.key;
        self.aliased = frame.aliased;
        self.value = frame.value;
        self.expect = Expect::COMMA;
        self.emit(value, frame.emit)
    }

    /// A completed value: queued if emitted, else kept in a built parent. With the paths
    /// used here an emitted value never has a built parent.
    fn emit(&mut self, value: Option<Value>, emit: bool) -> Step {
        // An array keeps its index for the next element; a member's name is not read again.
        let key = match self.key {
            Some(Key::Index(i)) => Some(Key::Index(i)),
            _ => self.key.take(),
        };
        if emit {
            let (Some(key), Some(value)) = (key, value) else {
                unreachable!("emitted values are built and have keys")
            };
            self.queue.push_back((key, value));
        } else if let (Some(parent), Some(value)) = (&mut self.value, value) {
            match (parent, key) {
                (Value::Array(items), _) => items.push(value),
                // A member read through an alias gives way to one under its own name.
                (Value::Object(o), Some(Key::Name(name))) => {
                    if !(self.aliased && o.members.iter().any(|(k, _)| *k == name)) {
                        o.set(name, value);
                    }
                }
                _ => unreachable!("object members have names"),
            }
        }
        if self.stack.is_empty() {
            return Err(End::Done);
        }
        Ok(())
    }

    /// Whether the value at the current key matches a path.
    fn should_emit(&self) -> bool {
        self.paths.iter().any(|path| self.at(path))
    }

    /// The name of the current key when a path's `*` is under it, so the value must be an
    /// array.
    fn items_parent(&self) -> Option<&'static str> {
        self.paths.iter().find_map(|path| match *path {
            [parent @ .., "*"] if self.at(parent) => parent.last().copied(),
            _ => None,
        })
    }

    /// Whether the current key's path is `path`.
    fn at(&self, path: &[&str]) -> bool {
        let keys = (self.stack.iter().skip(1).map(|f| &f.key)).chain([&self.key]);
        path.len() == self.stack.len()
            && (path.iter().zip(keys))
                .all(|(s, k)| *s == "*" || matches!(k, Some(Key::Name(n)) if n == s))
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
        let input = "{\"x\": [1], \"b\": [1.50, -0e+1, \"s\"], \"a\": {\"n\": 10}} trailing";
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
        assert_eq!(message("{\"a\": tru}"), "Unexpected '}' in state TRUE3");
        assert_eq!(
            message("{\"a\": \"\\u12x\"}"),
            "Unexpected 'x' in state STRING_UNICODE_DIGIT_3"
        );
        assert_eq!(message("{\"a\" 1}"), "Unexpected NUMBER (1) in state COLON");
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
