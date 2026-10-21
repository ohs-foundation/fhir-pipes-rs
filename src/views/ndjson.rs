//! Reading NDJSON a chunk of lines at a time, so a large file is never held
//! whole: `refresh`'s views step, its file source and `views run --input`
//! all read NDJSON through this. A chunk's lines are parsed on several
//! threads and come back in line order. A file whose name ends in `.gz` is
//! read decompressed, every gzip member of it. The lines are found by
//! `fhir::ndjson::Splitter`, as every NDJSON reader's are.

use std::fs::File;
use std::io::{BufRead, ErrorKind};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::error::usage;
use crate::fhir::ndjson::{Span, Splitter, is_gz, read_text};

/// The most lines in one chunk. About 20 MB of text for ICR's MeasureReports,
/// and ten times that once parsed.
pub const CHUNK_LINES: usize = 20_000;

/// The most text in one chunk: a chunk ends at the line that reaches it, so
/// very large resources make smaller chunks.
pub const CHUNK_BYTES: usize = 64 << 20;

/// The chunks of an NDJSON source.
pub struct Lines<R> {
    lines: Splitter<R>,
    /// The file's name in messages.
    name: Arc<str>,
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

/// Where one line of a chunk is: its number and offset in the file, and
/// its text in the chunk.
pub type Line = Span;

/// The file at `path`, to be read a chunk at a time. Offsets into a gzip
/// file are offsets into its decompressed text.
pub fn open(path: &Path) -> Result<Lines<Box<dyn BufRead + Send>>> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Lines::new(
        read_text(file, is_gz(path)),
        &path.display().to_string(),
    ))
}

/// A failed read, with `what` as its context. When it failed on what the
/// file holds (text that is not UTF-8, a gzip stream that is corrupt or cut
/// short) rather than on the disk, it is the input's fault: a usage error.
pub fn read_error(e: std::io::Error, what: String) -> anyhow::Error {
    let bad = matches!(
        e.kind(),
        ErrorKind::InvalidData | ErrorKind::InvalidInput | ErrorKind::UnexpectedEof
    );
    let e = anyhow::Error::new(e).context(what);
    if bad { usage(e) } else { e }
}

impl<R: BufRead> Lines<R> {
    /// `name` is the source's name in messages.
    pub fn new(reader: R, name: &str) -> Self {
        Self {
            lines: Splitter::new(reader),
            name: name.into(),
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
        let mut text = Vec::new();
        let mut lines = Vec::new();
        while lines.len() < self.max_lines && text.len() < self.max_bytes {
            let line = self.lines.next_into(&mut text).map_err(|e| {
                read_error(
                    e,
                    format!("reading {} line {}", self.name, self.lines.lines_read() + 1),
                )
            })?;
            let Some(line) = line else {
                self.done = true;
                break;
            };
            lines.push(line);
        }
        if lines.is_empty() {
            return Ok(None);
        }
        // Checked once for the whole chunk; what is not UTF-8 is on a line,
        // since what lies between them is whitespace.
        let text = String::from_utf8(text).map_err(|e| {
            let at = e.utf8_error().valid_up_to();
            let line = &lines[lines.partition_point(|l| l.start <= at).saturating_sub(1)];
            let e = std::io::Error::new(ErrorKind::InvalidData, e.utf8_error());
            read_error(e, format!("reading {} line {}", self.name, line.number))
        })?;
        Ok(Some(Chunk {
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
    fn a_byte_order_mark_before_the_first_line_is_skipped() {
        let text = "\u{feff}{\"a\":1}\n\u{feff}{\"b\":2}\n";
        let parts = chunks(text, 100, usize::MAX);
        let lines: Vec<(u64, &str)> = parts[0].lines().map(|(l, t)| (l.offset, t)).collect();
        // Only the first: a BOM inside the file is the line's own text.
        assert_eq!(lines, [(3, "{\"a\":1}"), (11, "\u{feff}{\"b\":2}")]);
        assert_eq!(&text[3..10], "{\"a\":1}");
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
