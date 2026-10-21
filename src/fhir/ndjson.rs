//! Streaming NDJSON. `Splitter` finds the lines of NDJSON text, and every
//! NDJSON reader in fhir-pipes goes through it: `NdjsonReader` yields each
//! line with its byte offset so pass two can seek straight back to it;
//! `LineAccess` does that seek; `fhir::chunks::Lines` reads a chunk of
//! lines at a time. `NdjsonWriter` writes NDJSON output that a failed run
//! does not leave half-written, and that `dhis2 reports --resume` can carry
//! on with.
//!
//! A path ending in `.gz` is gzip: read through a decoder (any number of
//! concatenated members), and written through an encoder by `Output`.
//! Offsets into a gzip file are offsets into its decompressed text, so
//! `LineAccess` refuses one.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::fs::AtomicFile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub number: usize,
    pub offset: u64,
    pub len: usize,
    pub text: String,
}

const HEAD_PEEK: u64 = 64 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// True for a path fhir-pipes reads or writes as gzip.
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
    let file = File::open(path).map_err(|e| PipesError::io(path, e))?;
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
pub enum Output<W: Write = File> {
    Plain(BufWriter<W>),
    Gz(Box<GzEncoder<BufWriter<W>>>),
}

#[cfg(test)]
impl Output<File> {
    pub fn create(path: &Path, gz: bool) -> Result<Self> {
        let file = File::create(path).map_err(|e| PipesError::io(path, e))?;
        Ok(Self::new(file, gz))
    }
}

impl<W: Write> Output<W> {
    pub fn new(inner: W, gz: bool) -> Self {
        let w = BufWriter::with_capacity(1 << 20, inner);
        if gz {
            Output::Gz(Box::new(GzEncoder::new(w, Compression::default())))
        } else {
            Output::Plain(w)
        }
    }

    pub fn finish(self) -> std::io::Result<W> {
        let mut w = match self {
            Output::Plain(w) => w,
            Output::Gz(e) => e.finish()?,
        };
        w.flush()?;
        w.into_inner().map_err(|e| e.into_error())
    }
}

impl<W: Write> Write for Output<W> {
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

/// JSON's own whitespace: what is trimmed from around a line, so the text
/// parses as the whole line would. A line of nothing else is blank.
fn is_json_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Where `Splitter::next_into` put one non-blank line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Its 1-based number in the text, blank lines counted.
    pub number: usize,
    /// Where its text starts in the text, in bytes, after any leading
    /// whitespace (and the byte order mark before the first line).
    pub offset: u64,
    /// Its text in the buffer, without the whitespace around it.
    pub start: usize,
    pub end: usize,
}

/// The non-blank lines of NDJSON text. A UTF-8 byte order mark before the
/// first line is not part of it; one inside the text is the line's own.
/// Lines are bytes here: what is not UTF-8 is for the caller to report,
/// with the line's number.
pub struct Splitter<R> {
    reader: R,
    /// Lines read so far, blank ones too.
    number: usize,
    /// Bytes read so far.
    offset: u64,
    /// The longest line read; `None` for no limit.
    max_len: Option<u64>,
}

impl<R: BufRead> Splitter<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            number: 0,
            offset: 0,
            max_len: None,
        }
    }

    /// A line longer than `max` bytes fails the read, instead of being held
    /// whole: for text from the network, where a file with no newline
    /// could be any size.
    pub fn with_max_len(mut self, max: u64) -> Self {
        self.max_len = Some(max);
        self
    }

    /// Lines read so far, blank ones too: the number of the line a failed
    /// read was on, less one.
    pub fn lines_read(&self) -> usize {
        self.number
    }

    /// What is left of the text, for a caller that reads the rest another
    /// way.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Appends the next non-blank line to `buf`, newline and all, and says
    /// where its text is. Blank lines are read past and not kept. `None` at
    /// the end of the text.
    pub fn next_into(&mut self, buf: &mut Vec<u8>) -> std::io::Result<Option<Span>> {
        loop {
            let from = buf.len();
            let n = match self.max_len {
                Some(max) => (&mut self.reader).take(max + 1).read_until(b'\n', buf)?,
                None => self.reader.read_until(b'\n', buf)?,
            };
            if n == 0 {
                return Ok(None);
            }
            if let Some(max) = self.max_len
                && n as u64 > max
            {
                buf.truncate(from);
                return Err(std::io::Error::other(format!(
                    "line {} is longer than {} MB",
                    self.number + 1,
                    max >> 20
                )));
            }
            let start_offset = self.offset;
            self.offset += n as u64;
            self.number += 1;
            let mut line = &buf[from..];
            let mut skip = 0;
            if self.number == 1 && line.starts_with(BOM) {
                line = &line[BOM.len()..];
                skip = BOM.len();
            }
            let Some(lead) = line.iter().position(|&b| !is_json_space(b)) else {
                buf.truncate(from);
                continue; // blank line
            };
            let end = line
                .iter()
                .rposition(|&b| !is_json_space(b))
                .map_or(0, |p| p + 1);
            let start = from + skip + lead;
            return Ok(Some(Span {
                number: self.number,
                offset: start_offset + (skip + lead) as u64,
                start,
                end: from + skip + end,
            }));
        }
    }
}

pub struct NdjsonReader {
    path: PathBuf,
    lines: Splitter<Box<dyn BufRead + Send>>,
    buf: Vec<u8>,
}

impl std::fmt::Debug for NdjsonReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NdjsonReader")
            .field("path", &self.path)
            .field("offset", &self.lines.offset)
            .field("number", &self.lines.number)
            .finish()
    }
}

impl NdjsonReader {
    /// Opens and rejects a full JSON document (first non-blank line is a lone
    /// `{` or an array opener) before yielding anything, so the operator gets
    /// one clear message instead of an error per line. The peek is bounded to
    /// `HEAD_PEEK` bytes: pulling a whole line here would read a minified
    /// single-line JSON document entirely into memory before we could reject
    /// it. A leading UTF-8 BOM is skipped (by `Splitter`) and accounted for
    /// so every downstream offset -- and `LineAccess::read_at` -- stays
    /// correct.
    /// A `.gz` file is read decompressed; offsets are into its text.
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = open_text(path)?;
        let mut head = Vec::new();
        loop {
            head.clear();
            let n = (&mut reader)
                .take(HEAD_PEEK)
                .read_until(b'\n', &mut head)
                .map_err(|e| PipesError::io(path, e))?;
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
            return Err(PipesError::Usage(format!(
                "{}: looks like a JSON document, not NDJSON (one resource per line)",
                path.display()
            )));
        }
        // Start again from the top (a gzip stream cannot seek back).
        Ok(Self {
            path: path.to_path_buf(),
            lines: Splitter::new(open_text(path)?),
            buf: Vec::new(),
        })
    }
}

impl Iterator for NdjsonReader {
    type Item = Result<Line>;

    fn next(&mut self) -> Option<Self::Item> {
        self.buf.clear();
        let span = match self.lines.next_into(&mut self.buf) {
            Ok(span) => span?,
            Err(e) => return Some(Err(PipesError::io(&self.path, e))),
        };
        let text = match std::str::from_utf8(&self.buf[span.start..span.end]) {
            Ok(s) => s.to_string(),
            Err(e) => {
                return Some(Err(PipesError::Usage(format!(
                    "{}: line {}: {e}",
                    self.path.display(),
                    span.number
                ))));
            }
        };
        Some(Ok(Line {
            number: span.number,
            offset: span.offset,
            len: span.end - span.start,
            text,
        }))
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
            return Err(PipesError::Usage(format!(
                "{}: read by offset, which a gzip file does not allow; decompress it first",
                path.display()
            )));
        }
        let file = File::open(path).map_err(|e| PipesError::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn read_at(&mut self, offset: u64, len: usize) -> Result<String> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| PipesError::io(&self.path, e))?;
        let mut buf = vec![0u8; len];
        self.file
            .read_exact(&mut buf)
            .map_err(|e| PipesError::io(&self.path, e))?;
        String::from_utf8(buf).map_err(|e| {
            PipesError::Usage(format!("{}: offset {offset}: {e}", self.path.display()))
        })
    }
}

/// One resource per line, written to a temporary file and renamed into
/// place, so a failed run never leaves a half-written file behind.
pub fn write_ndjson<'a>(
    out: &Path,
    resources: impl IntoIterator<Item = &'a Value>,
) -> Result<usize> {
    let mut w = NdjsonWriter::create(out)?;
    for r in resources {
        w.write(r)?;
    }
    w.finish()
}

/// NDJSON written as it is produced: lines go to `<out>.tmp`, which
/// `finish` renames into place. Dropped unfinished (a failed run), the
/// temporary file is removed, unless `keep_partial` asked to keep it for a
/// later run to continue.
///
/// When `out` ends in `.gz` the file is gzip, written as one gzip member
/// per `sync` (a batch of reports). The members concatenated are one valid
/// gzip file, and the length after each is a place an interrupted run can
/// be cut back to, which the middle of a member is not.
pub struct NdjsonWriter {
    tmp: PathBuf,
    file: Option<BufWriter<AtomicFile>>,
    /// The gzip member being written, holding the file meanwhile.
    member: Option<GzEncoder<BufWriter<AtomicFile>>>,
    gz: bool,
    n: usize,
    /// Length of the file up to the last line written (plain), or the last
    /// member finished (gzip).
    bytes: u64,
}

impl NdjsonWriter {
    pub fn tmp_path(out: &Path) -> PathBuf {
        with_suffix(out, ".tmp")
    }

    pub fn create(out: &Path) -> Result<Self> {
        let file = AtomicFile::create_at(out, Self::tmp_path(out))?;
        Ok(Self::new(out, file, 0))
    }

    /// Carries on with the temporary file of an interrupted run: cut back
    /// to `bytes`, the length last synced, dropping anything half-written
    /// after it, then appended to.
    pub fn resume(out: &Path, bytes: u64) -> Result<Self> {
        let tmp = Self::tmp_path(out);
        let mut file = OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(|e| PipesError::io(&tmp, e))?;
        let len = file.metadata().map_err(|e| PipesError::io(&tmp, e))?.len();
        if len < bytes {
            return Err(PipesError::Usage(format!(
                "{}: {len} bytes, shorter than the {bytes} recorded; run without --resume to start over",
                tmp.display()
            )));
        }
        file.set_len(bytes).map_err(|e| PipesError::io(&tmp, e))?;
        file.seek(SeekFrom::End(0))
            .map_err(|e| PipesError::io(&tmp, e))?;
        Ok(Self::new(out, AtomicFile::with_file(out, tmp, file), bytes))
    }

    fn new(out: &Path, file: AtomicFile, bytes: u64) -> Self {
        Self {
            tmp: file.tmp().to_path_buf(),
            file: Some(BufWriter::with_capacity(1 << 20, file)),
            member: None,
            gz: is_gz(out),
            n: 0,
            bytes,
        }
    }

    /// Keep the temporary file if the run fails.
    /// Set on the `AtomicFile` itself, so the file survives however the
    /// writer is dropped, a failed `finish` included.
    pub fn keep_partial(mut self) -> Self {
        if let Some(m) = self.member.as_mut() {
            m.get_mut().get_mut().keep_on_drop(true);
        }
        if let Some(f) = self.file.as_mut() {
            f.get_mut().keep_on_drop(true);
        }
        self
    }

    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// The text written up to the last sync, decompressed.
    pub fn read_back(&self) -> Result<Box<dyn std::io::BufRead + Send>> {
        let file = File::open(&self.tmp).map_err(|e| PipesError::io(&self.tmp, e))?;
        Ok(read_text(file.take(self.bytes), self.gz))
    }

    pub fn write(&mut self, r: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(r)?;
        line.push(b'\n');
        self.write_line(&line)
    }

    /// One line already serialised, ending in a newline.
    pub fn write_line(&mut self, line: &[u8]) -> Result<()> {
        let written = if self.gz {
            if self.member.is_none() {
                let file = self.file.take().expect("written after finish");
                self.member = Some(GzEncoder::new(file, Compression::default()));
            }
            self.member.as_mut().map(|m| m.write_all(line))
        } else {
            self.bytes += line.len() as u64;
            self.file.as_mut().map(|f| f.write_all(line))
        };
        written
            .expect("written after finish")
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        self.n += 1;
        Ok(())
    }

    /// Ends the gzip member being written, if any.
    fn end_member(&mut self) -> std::io::Result<()> {
        if let Some(m) = self.member.take() {
            let mut file = m.finish()?;
            self.bytes = file.stream_position()?;
            self.file = Some(file);
        }
        Ok(())
    }

    /// Flushes everything written to disk; returns the file's length.
    pub fn sync(&mut self) -> Result<u64> {
        self.end_member()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        let f = self.file.as_mut().expect("synced after finish");
        f.flush().map_err(|e| PipesError::io(&self.tmp, e))?;
        f.get_ref()
            .file()
            .sync_data()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        Ok(self.bytes)
    }

    /// Renames the file into place; returns the lines this writer wrote.
    pub fn finish(mut self) -> Result<usize> {
        self.end_member()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        let mut f = self.file.take().expect("finished once");
        // An empty gzip file is not valid gzip; an empty member is.
        if self.gz && self.bytes == 0 {
            f = GzEncoder::new(f, Compression::default())
                .finish()
                .map_err(|e| PipesError::io(&self.tmp, e))?;
        }
        f.into_inner()
            .map_err(|e| PipesError::io(&self.tmp, e.into_error()))?
            .commit()?;
        Ok(self.n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
    fn splitter_keeps_each_line_and_its_place() {
        let text = b"\xEF\xBB\xBF {\"a\":1}\r\n \t\n\x0C\n{\"b\":2}";
        let mut lines = Splitter::new(&text[..]);
        let mut buf = Vec::new();
        let mut found = Vec::new();
        while let Some(span) = lines.next_into(&mut buf).unwrap() {
            found.push((span, buf[span.start..span.end].to_vec()));
        }
        // Only JSON's whitespace is blank: a form feed is the line's text,
        // for the JSON parser to reject.
        let texts: Vec<&[u8]> = found.iter().map(|(_, t)| t.as_slice()).collect();
        assert_eq!(texts, [&b"{\"a\":1}"[..], b"\x0C", b"{\"b\":2}"]);
        let places: Vec<(usize, u64)> = found.iter().map(|(s, _)| (s.number, s.offset)).collect();
        assert_eq!(places, [(1, 4), (3, 16), (4, 18)]);
        for (span, t) in &found {
            let at = span.offset as usize;
            assert_eq!(&text[at..at + t.len()], t.as_slice());
        }
        assert_eq!(lines.lines_read(), 4);
    }

    #[test]
    fn splitter_refuses_a_line_over_its_limit() {
        let mut lines = Splitter::new(&b"{}\n{\"long\":1}\n"[..]).with_max_len(8);
        let mut buf = Vec::new();
        assert!(lines.next_into(&mut buf).unwrap().is_some());
        let err = lines.next_into(&mut buf).unwrap_err();
        assert!(err.to_string().contains("line 2 is longer"), "{err}");
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

    #[test]
    fn keep_partial_keeps_the_file_when_finish_fails() {
        for name in ["out.ndjson", "out.ndjson.gz"] {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(name);
            // A directory with a file in it: the rename onto it fails.
            std::fs::create_dir(&out).unwrap();
            std::fs::write(out.join("x"), "x").unwrap();
            let mut w = NdjsonWriter::create(&out).unwrap().keep_partial();
            w.write(&json!({"id": "a"})).unwrap();
            let bytes = w.sync().unwrap();
            w.write(&json!({"id": "b"})).unwrap();
            assert!(w.finish().is_err(), "{name}");
            let tmp = NdjsonWriter::tmp_path(&out);
            assert!(tmp.is_file(), "{name}: the partial output is kept");
            let resumed = NdjsonWriter::resume(&out, bytes).unwrap();
            let mut text = String::new();
            resumed
                .read_back()
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(text, "{\"id\":\"a\"}\n", "{name}");
        }
    }

    #[test]
    fn without_keep_partial_a_failed_finish_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.ndjson");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("x"), "x").unwrap();
        let mut w = NdjsonWriter::create(&out).unwrap();
        w.write(&json!({"id": "a"})).unwrap();
        assert!(w.finish().is_err());
        assert!(!NdjsonWriter::tmp_path(&out).exists());
    }
}
