//! Reading NDJSON a chunk of lines at a time, so a large file is never held
//! whole: `refresh`'s views step, its file source and `views run --input`
//! all read NDJSON through this. A chunk's lines are parsed on several
//! threads and come back in line order.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

/// The most lines in one chunk. About 20 MB of text for ICR's MeasureReports,
/// and ten times that once parsed.
pub const CHUNK_LINES: usize = 20_000;

/// The most text in one chunk: a chunk ends at the line that reaches it, so
/// very large resources make smaller chunks.
pub const CHUNK_BYTES: usize = 64 << 20;

/// The chunks of an NDJSON source.
pub struct Lines<R> {
    reader: R,
    /// The file's name in messages.
    name: Arc<str>,
    /// Lines read so far, blank ones too.
    line_no: usize,
    /// Bytes read so far.
    offset: u64,
    max_lines: usize,
    max_bytes: usize,
    done: bool,
}

/// One chunk: the text of its lines, blank lines left out.
pub struct Chunk {
    name: Arc<str>,
    text: String,
    lines: Vec<Line>,
}

/// Where one line of a chunk is.
#[derive(Debug, Clone, Copy)]
pub struct Line {
    /// Its 1-based number in the file.
    pub number: usize,
    /// Where its text starts in the file, in bytes, after any leading
    /// whitespace.
    pub offset: u64,
    /// Its text in the chunk, without the whitespace around it.
    start: usize,
    end: usize,
}

/// The file at `path`, to be read a chunk at a time.
pub fn open(path: &Path) -> Result<Lines<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Lines::new(
        BufReader::with_capacity(1 << 20, file),
        &path.display().to_string(),
    ))
}

impl<R: BufRead> Lines<R> {
    /// `name` is the source's name in messages.
    pub fn new(reader: R, name: &str) -> Self {
        Self {
            reader,
            name: name.into(),
            line_no: 0,
            offset: 0,
            max_lines: CHUNK_LINES,
            max_bytes: CHUNK_BYTES,
            done: false,
        }
    }

    /// Smaller chunks than the defaults, for tests.
    #[cfg(test)]
    pub fn with_limits(mut self, max_lines: usize, max_bytes: usize) -> Self {
        self.max_lines = max_lines.max(1);
        self.max_bytes = max_bytes;
        self
    }

    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        let mut text = String::new();
        let mut lines = Vec::new();
        while lines.len() < self.max_lines && text.len() < self.max_bytes {
            let start = text.len();
            let n = self
                .reader
                .read_line(&mut text)
                .with_context(|| format!("reading {} line {}", self.name, self.line_no + 1))?;
            if n == 0 {
                self.done = true;
                break;
            }
            self.line_no += 1;
            let line = &text[start..];
            if line.trim().is_empty() {
                text.truncate(start);
            } else {
                // JSON's own whitespace only, so the text parses as the whole
                // line would.
                let json_ws: &[char] = &[' ', '\t', '\r', '\n'];
                let lead = line.len() - line.trim_start_matches(json_ws).len();
                let kept = line.trim_matches(json_ws).len();
                lines.push(Line {
                    number: self.line_no,
                    offset: self.offset + lead as u64,
                    start: start + lead,
                    end: start + lead + kept,
                });
            }
            self.offset += n as u64;
        }
        Ok((!lines.is_empty()).then(|| Chunk {
            name: self.name.clone(),
            text,
            lines,
        }))
    }
}

impl<R: BufRead> Iterator for Lines<R> {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_chunk() {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

impl Chunk {
    /// The number of (non-blank) lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The lines, each with its text.
    pub fn lines(&self) -> impl Iterator<Item = (&Line, &str)> {
        self.lines.iter().map(|l| (l, &self.text[l.start..l.end]))
    }

    /// Each line read by `read` on up to `threads` threads, in line order.
    /// An error names the first bad line.
    pub fn parse<T: Send>(
        &self,
        threads: usize,
        read: impl Fn(&str) -> Result<T> + Sync,
    ) -> Result<Vec<T>> {
        let one = |part: &[Line]| -> Result<Vec<T>> {
            part.iter()
                .map(|l| {
                    read(&self.text[l.start..l.end])
                        .with_context(|| format!("parsing {} line {}", self.name, l.number))
                })
                .collect()
        };
        let threads = threads.clamp(1, self.lines.len().max(1));
        if threads == 1 {
            return one(&self.lines);
        }
        let part = self.lines.len().div_ceil(threads);
        std::thread::scope(|s| {
            let workers: Vec<_> = self
                .lines
                .chunks(part)
                .map(|p| s.spawn(move || one(p)))
                .collect();
            let mut out = Vec::with_capacity(self.lines.len());
            for w in workers {
                out.extend(
                    w.join()
                        .map_err(|_| anyhow::anyhow!("parsing thread panicked"))??,
                );
            }
            Ok(out)
        })
    }

    /// Each line as a JSON value, in line order.
    pub fn values(&self, threads: usize) -> Result<Vec<Value>> {
        self.parse(threads, |l| Ok(serde_json::from_str(l)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(text: &str, lines: usize, bytes: usize) -> Vec<Chunk> {
        Lines::new(text.as_bytes(), "x.ndjson")
            .with_limits(lines, bytes)
            .collect::<Result<_>>()
            .unwrap()
    }

    #[test]
    fn chunks_keep_line_order_and_skip_blank_lines() {
        let text: String = (0..50).map(|i| format!("{{\"n\":{i}}}\n\n")).collect();
        let parts = chunks(&text, 7, usize::MAX);
        assert_eq!(
            parts.iter().map(Chunk::len).collect::<Vec<_>>(),
            [7, 7, 7, 7, 7, 7, 7, 1]
        );
        let values: Vec<Value> = parts.iter().flat_map(|c| c.values(4).unwrap()).collect();
        assert_eq!(values.len(), 50);
        assert!(values.iter().enumerate().all(|(i, v)| v["n"] == i));
        // Line numbers count the blank lines.
        let numbers: Vec<usize> = parts[1].lines().map(|(l, _)| l.number).collect();
        assert_eq!(numbers, [15, 17, 19, 21, 23, 25, 27]);
    }

    #[test]
    fn a_chunk_ends_at_the_line_that_reaches_the_byte_limit() {
        let text = "{\"a\":1}\n{\"b\":22}\n{\"c\":3}\n";
        let parts = chunks(text, 100, 10);
        assert_eq!(parts.iter().map(Chunk::len).collect::<Vec<_>>(), [2, 1]);
    }

    #[test]
    fn offsets_point_at_each_lines_text_in_the_file() {
        let text = "  {\"a\":1}  \r\n\n\t{\"b\":2}\n{\"c\":3}";
        let parts = chunks(text, 2, usize::MAX);
        let found: Vec<(usize, u64, String)> = parts
            .iter()
            .flat_map(|c| {
                c.lines()
                    .map(|(l, t)| (l.number, l.offset, t.to_string()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            found,
            [
                (1, 2, "{\"a\":1}".to_string()),
                (3, 15, "{\"b\":2}".to_string()),
                (4, 23, "{\"c\":3}".to_string()),
            ]
        );
        for (_, offset, line) in &found {
            let at = *offset as usize;
            assert_eq!(&text[at..at + line.len()], line);
        }
    }

    #[test]
    fn an_error_names_the_file_and_line() {
        let parts = chunks("{}\n\n{}\n{oops\n{}\n", 2, usize::MAX);
        assert!(parts[0].values(2).is_ok());
        let err = format!("{:#}", parts[1].values(2).unwrap_err());
        assert!(err.contains("parsing x.ndjson line 4"), "{err}");
        let bad = [b'{', 0xff, b'}', b'\n'];
        let err = Lines::new(&bad[..], "y.ndjson")
            .next()
            .unwrap()
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("reading y.ndjson line 1"),
            "{err:#}"
        );
    }
}
