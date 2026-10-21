//! Text as the files' producers and the CMS validator see it. The validator is JavaScript:
//! it trims cells with the JS whitespace set (which also drops the `\r` of CRLF and a stray
//! U+FEFF) and decodes the file in 64 KiB reads with invalid UTF-8 replaced by U+FFFD.
//! Reading the same way gives the same rows and values.

use std::io::{self, Read};

/// JS whitespace (`trim`, `\s`): Rust's `char::is_whitespace` minus U+0085, plus U+FEFF.
pub fn is_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

pub fn trim(s: &str) -> &str {
    // ASCII above the space is never JS whitespace, and most values start and end with it
    // or are empty.
    let plain = |c: Option<&u8>| c.is_none_or(|&c| c > b' ' && c.is_ascii());
    if plain(s.as_bytes().first()) && plain(s.as_bytes().last()) {
        return s;
    }
    s.trim_matches(is_space)
}

/// Applied to the first chunk: a leading BOM goes with all the whitespace after it.
pub fn remove_bom(chunk: &str) -> &str {
    if chunk.starts_with('\u{feff}') { chunk.trim_start_matches(is_space) } else { chunk }
}

/// The read size of a Node file stream.
pub const CHUNK: usize = 64 * 1024;

/// A file read in fixed-size chunks, each decoded as UTF-8 as it comes, never an empty one.
pub struct Stream<R> {
    input: R,
    chunk: u64,
    /// Bytes of a UTF-8 sequence split by the chunk boundary.
    undecoded: Vec<u8>,
    ended: bool,
}

impl<R: Read> Stream<R> {
    pub fn new(input: R, chunk: usize) -> Self {
        Stream { input, chunk: chunk as u64, undecoded: Vec::new(), ended: false }
    }

    /// Appends the next chunk's text to `out`; false once the input is exhausted.
    pub fn next(&mut self, out: &mut String) -> io::Result<bool> {
        while !self.ended {
            let n = (self.input.by_ref()).take(self.chunk).read_to_end(&mut self.undecoded)?;
            if n == 0 {
                self.ended = true;
                // An unfinished sequence at the end is one U+FFFD, its own chunk.
                if !self.undecoded.is_empty() {
                    self.undecoded.clear();
                    out.push('\u{fffd}');
                    return Ok(true);
                }
            } else {
                let start = out.len();
                decode(&mut self.undecoded, out);
                if out.len() > start {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// Streaming UTF-8 decoding: complete sequences go to `out`, each maximal invalid subpart
/// becomes U+FFFD, and an incomplete sequence at the end stays in `bytes`.
fn decode(bytes: &mut Vec<u8>, out: &mut String) {
    let done = match std::str::from_utf8(bytes) {
        Ok(text) => {
            out.push_str(text);
            bytes.len()
        }
        // Valid but for a sequence cut short at the end: no lossy pass needed.
        Err(e) if e.error_len().is_none() => {
            out.push_str(
                std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("valid up to there"),
            );
            e.valid_up_to()
        }
        Err(_) => {
            let incomplete = bytes.utf8_chunks().last().map_or(0, |c| {
                let bad = c.invalid();
                let cut_short = std::str::from_utf8(bad).is_err_and(|e| e.error_len().is_none());
                if cut_short { bad.len() } else { 0 }
            });
            let done = bytes.len() - incomplete;
            out.push_str(&String::from_utf8_lossy(&bytes[..done]));
            done
        }
    };
    bytes.drain(..done);
}
