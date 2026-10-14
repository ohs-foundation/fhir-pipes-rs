//! Streaming NDJSON. `NdjsonReader` yields each line with its byte offset so
//! pass two can seek straight back to it; `LineAccess` does that seek.
//!
//! A path ending in `.gz` is gzip: read through a decoder (any number of
//! concatenated members), and written through an encoder by `Output`.
//! Offsets into a gzip file are offsets into its decompressed text, so
//! `LineAccess` refuses one.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::error::{KilnError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub number: usize,
    pub offset: u64,
    pub len: usize,
    pub text: String,
}

const HEAD_PEEK: u64 = 64 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// True for a path kiln reads or writes as gzip.
pub fn is_gz(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

/// `path` with `suffix` added after its whole name: `a.ndjson.gz` + `.tmp`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// A file's text, decompressed when it is gzip.
pub fn open_text(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let file = File::open(path).map_err(|e| KilnError::io(path, e))?;
    Ok(read_text(file, is_gz(path)))
}

/// Text from any reader, decompressed when `gz`.
pub fn read_text(r: impl Read + Send + 'static, gz: bool) -> Box<dyn BufRead + Send> {
    if gz {
        Box::new(BufReader::with_capacity(1 << 20, MultiGzDecoder::new(r)))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, r))
    }
}

/// A file being written, plain or gzip. `finish` completes the gzip
/// stream and flushes; dropping it unfinished may leave the stream cut off,
/// which is what a temporary file of a failed run is.
pub enum Output {
    Plain(BufWriter<File>),
    Gz(GzEncoder<BufWriter<File>>),
}

impl Output {
    pub fn create(path: &Path, gz: bool) -> Result<Self> {
        let file = File::create(path).map_err(|e| KilnError::io(path, e))?;
        let w = BufWriter::with_capacity(1 << 20, file);
        Ok(if gz {
            Output::Gz(GzEncoder::new(w, Compression::default()))
        } else {
            Output::Plain(w)
        })
    }

    pub fn finish(self) -> std::io::Result<File> {
        let mut w = match self {
            Output::Plain(w) => w,
            Output::Gz(e) => e.finish()?,
        };
        w.flush()?;
        w.into_inner().map_err(|e| e.into_error())
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Output::Plain(w) => w.write(buf),
            Output::Gz(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Output::Plain(w) => w.flush(),
            Output::Gz(w) => w.flush(),
        }
    }
}

pub struct NdjsonReader {
    path: PathBuf,
    reader: Box<dyn BufRead + Send>,
    offset: u64,
    number: usize,
    buf: Vec<u8>,
}

impl std::fmt::Debug for NdjsonReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NdjsonReader")
            .field("path", &self.path)
            .field("offset", &self.offset)
            .field("number", &self.number)
            .finish()
    }
}

impl NdjsonReader {
    /// Opens and rejects a full JSON document (first non-blank line is a lone
    /// `{` or an array opener) before yielding anything, so the operator gets
    /// one clear message instead of an error per line. The peek is bounded to
    /// `HEAD_PEEK` bytes: pulling a whole line here would read a minified
    /// single-line JSON document entirely into memory before we could reject
    /// it. A leading UTF-8 BOM is skipped and accounted for so every
    /// downstream offset -- and `LineAccess::read_at` -- stays correct.
    /// A `.gz` file is read decompressed; offsets are into its text.
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = open_text(path)?;
        let mut head = Vec::new();
        loop {
            head.clear();
            let n = (&mut reader)
                .take(HEAD_PEEK)
                .read_until(b'\n', &mut head)
                .map_err(|e| KilnError::io(path, e))?;
            if n == 0 || head.iter().any(|b| !b.is_ascii_whitespace()) {
                break;
            }
        }
        let bom = head.starts_with(BOM);
        // Lossy on purpose: invalid UTF-8 on line 1 must not be misclassified
        // as an Io error here -- it surfaces from the iterator as a Usage
        // error (exit 2) once we actually try to parse that line's text.
        let text = String::from_utf8_lossy(if bom { &head[BOM.len()..] } else { &head });
        let text = text.trim();
        if text == "{" || text.starts_with('[') {
            return Err(KilnError::Usage(format!(
                "{}: looks like a JSON document, not NDJSON (one resource per line)",
                path.display()
            )));
        }
        // Start again from the top (a gzip stream cannot seek back).
        let offset = if bom { BOM.len() as u64 } else { 0 };
        let mut reader = open_text(path)?;
        let mut skip = vec![0u8; offset as usize];
        reader
            .read_exact(&mut skip)
            .map_err(|e| KilnError::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            offset,
            number: 0,
            buf: Vec::new(),
        })
    }
}

impl Iterator for NdjsonReader {
    type Item = Result<Line>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.buf.clear();
            let start = self.offset;
            let n = match self.reader.read_until(b'\n', &mut self.buf) {
                Ok(n) => n,
                Err(e) => return Some(Err(KilnError::io(&self.path, e))),
            };
            if n == 0 {
                return None;
            }
            self.offset += n as u64;
            self.number += 1;
            let trimmed_end = self
                .buf
                .iter()
                .rposition(|b| !b.is_ascii_whitespace())
                .map_or(0, |p| p + 1);
            let leading = self.buf[..trimmed_end]
                .iter()
                .position(|b| !b.is_ascii_whitespace());
            let Some(leading) = leading else {
                continue; // blank line
            };
            let slice = &self.buf[leading..trimmed_end];
            let text = match std::str::from_utf8(slice) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    return Some(Err(KilnError::Usage(format!(
                        "{}: line {}: {e}",
                        self.path.display(),
                        self.number
                    ))))
                }
            };
            return Some(Ok(Line {
                number: self.number,
                offset: start + leading as u64,
                len: slice.len(),
                text,
            }));
        }
    }
}

/// Seeks and reads back one recorded `(offset, len)` line. Deliberately
/// unbuffered: pass two visits lines in Hilbert-curve order, which is
/// effectively random access into the file, and a `BufReader` throws away
/// its buffer on every `seek` -- measured about 70x slower than a raw `File`
/// here with a 1 MiB buffer. Do not add buffering.
#[derive(Debug)]
pub struct LineAccess {
    path: PathBuf,
    file: File,
}

impl LineAccess {
    pub fn open(path: &Path) -> Result<Self> {
        if is_gz(path) {
            return Err(KilnError::Usage(format!(
                "{}: read by offset, which a gzip file does not allow; decompress it first",
                path.display()
            )));
        }
        let file = File::open(path).map_err(|e| KilnError::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn read_at(&mut self, offset: u64, len: usize) -> Result<String> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| KilnError::io(&self.path, e))?;
        let mut buf = vec![0u8; len];
        self.file
            .read_exact(&mut buf)
            .map_err(|e| KilnError::io(&self.path, e))?;
        String::from_utf8(buf)
            .map_err(|e| KilnError::Usage(format!("{}: offset {offset}: {e}", self.path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(content: &str) -> tempfile::NamedTempFile {
        tmp_bytes(content.as_bytes())
    }

    fn tmp_bytes(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f
    }

    #[test]
    fn yields_each_nonblank_line_with_its_offset() {
        let f = tmp("{\"id\":\"a\"}\n\n{\"id\":\"b\"}\n");
        let lines: Vec<Line> = NdjsonReader::open(f.path())
            .unwrap()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].offset, 0);
        assert_eq!(lines[0].text, "{\"id\":\"a\"}");
        assert_eq!(lines[0].number, 1);
        // Byte layout: `{"id":"a"}` (0..10) + `\n` (10) + blank line's own
        // `\n` (11) + `{"id":"b"}` starting at 12 + trailing `\n` (22).
        assert_eq!(lines[1].offset, 12);
        assert_eq!(lines[1].len, 10);
        assert_eq!(lines[1].number, 3);
    }

    #[test]
    fn gzip_is_read_through_every_member() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ndjson.gz");
        let mut bytes = Vec::new();
        for part in ["\u{feff}{\"id\":\"a\"}\n\n", "{\"id\":\"b\"}\n"] {
            let mut out = Output::create(&dir.path().join("part"), true).unwrap();
            out.write_all(part.as_bytes()).unwrap();
            out.finish().unwrap();
            bytes.extend(std::fs::read(dir.path().join("part")).unwrap());
        }
        std::fs::write(&path, bytes).unwrap();
        let texts: Vec<String> = NdjsonReader::open(&path)
            .unwrap()
            .map(|l| l.unwrap().text)
            .collect();
        assert_eq!(texts, ["{\"id\":\"a\"}", "{\"id\":\"b\"}"]);
        assert!(LineAccess::open(&path).is_err());
        assert!(is_gz(&path) && !is_gz(Path::new("a.ndjson")));
        assert_eq!(
            with_suffix(&path, ".tmp"),
            dir.path().join("a.ndjson.gz.tmp")
        );
    }

    #[test]
    fn read_at_returns_the_same_bytes() {
        let f = tmp("{\"id\":\"a\"}\n{\"id\":\"b\"}\n");
        let lines: Vec<Line> = NdjsonReader::open(f.path())
            .unwrap()
            .map(|l| l.unwrap())
            .collect();
        let mut random = LineAccess::open(f.path()).unwrap();
        assert_eq!(
            random.read_at(lines[1].offset, lines[1].len).unwrap(),
            "{\"id\":\"b\"}"
        );
        assert_eq!(
            random.read_at(lines[0].offset, lines[0].len).unwrap(),
            "{\"id\":\"a\"}"
        );
    }

    #[test]
    fn pretty_printed_json_is_rejected_up_front() {
        let f = tmp("{\n  \"resourceType\": \"Bundle\"\n}\n");
        let err = NdjsonReader::open(f.path()).unwrap_err();
        assert!(err.to_string().contains("not NDJSON"), "{err}");
    }

    #[test]
    fn non_object_lines_are_yielded_for_the_caller_to_report() {
        // `open()` already rejects a whole file that looks like a single
        // pretty-printed JSON document; a per-line non-object (e.g. a
        // malformed line in an otherwise-NDJSON file) is not this reader's
        // job to reject -- it's yielded like any other line, and the
        // caller (build_index) reports it as `malformed_field` once it
        // fails to parse as JSON.
        let f = tmp("{\"id\":\"a\"}\nnot json\n");
        let mut reader = NdjsonReader::open(f.path()).unwrap();
        let first = reader.next().unwrap().unwrap();
        assert_eq!(first.text, "{\"id\":\"a\"}");
        let second = reader.next().unwrap().unwrap();
        assert_eq!(second.number, 2);
        assert_eq!(second.text, "not json");
    }

    #[test]
    fn read_at_round_trips_every_line_whatever_the_layout() {
        let f = tmp("{\"id\":\"a\"}\r\n\r\n  \t{\"id\":\"b\"}\n{\"id\":\"c\"}");
        let lines: Vec<Line> = NdjsonReader::open(f.path())
            .unwrap()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(
            lines.iter().map(|l| l.number).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        let mut random = LineAccess::open(f.path()).unwrap();
        for line in &lines {
            assert_eq!(random.read_at(line.offset, line.len).unwrap(), line.text);
        }
    }

    #[test]
    fn bom_is_skipped() {
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(b"{\"id\":\"a\"}\n");
        let f = tmp_bytes(&bytes);
        let lines: Vec<Line> = NdjsonReader::open(f.path())
            .unwrap()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].offset, 3);
        assert_eq!(lines[0].len, 10);
        assert_eq!(lines[0].text, "{\"id\":\"a\"}");
        let mut random = LineAccess::open(f.path()).unwrap();
        assert_eq!(
            random.read_at(lines[0].offset, lines[0].len).unwrap(),
            "{\"id\":\"a\"}"
        );
    }

    #[test]
    fn bom_does_not_defeat_document_detection() {
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(b"{\n  \"a\": 1\n}\n");
        let f = tmp_bytes(&bytes);
        let err = NdjsonReader::open(f.path()).unwrap_err();
        assert!(err.to_string().contains("not NDJSON"), "{err}");
    }

    #[test]
    fn minified_array_is_rejected_at_open() {
        let f = tmp("[{\"id\":\"a\"},{\"id\":\"b\"}]\n");
        let err = NdjsonReader::open(f.path()).unwrap_err();
        assert!(err.to_string().contains("not NDJSON"), "{err}");
    }

    #[test]
    fn invalid_utf8_on_first_line_is_a_usage_error() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"id\":\"");
        bytes.push(0xFF);
        bytes.extend_from_slice(b"\"}\n");
        let f = tmp_bytes(&bytes);
        let mut reader = NdjsonReader::open(f.path()).unwrap();
        let err = reader.next().unwrap().unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
