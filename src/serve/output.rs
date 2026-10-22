//! An attempt's output: read line by line, each line cut at 64 KB and
//! redacted, then written to the run's log (up to `max_log_bytes`) and
//! passed to serve's stdout through a bounded queue, so neither a full log
//! nor a slow reader of serve's stdout can stall a refresh.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::secrets::Redactor;

/// A line longer than this is cut, counted in the child's bytes.
pub const LINE_CUT: usize = 64 * 1024;
/// The stdout queue's limits.
pub const QUEUE_LINES: usize = 10_000;
pub const QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// The first `keep` bytes of `input` with every secret that starts there
/// replaced whole, even where it runs past `keep`, and how many bytes after
/// that were dropped. Positions are the child's bytes, not the redacted
/// text's, so an earlier replacement cannot pull part of a later secret in.
pub fn cut_line(input: &[u8], keep: usize, r: &Redactor) -> (String, usize) {
    let window = &input[..input.len().min(keep.saturating_add(r.longest()))];
    let mut out = Vec::with_capacity(window.len().min(keep) + 3);
    let mut pos = 0;
    for (start, len) in r.matches(window) {
        if start >= keep {
            break;
        }
        out.extend_from_slice(&input[pos..start]);
        out.extend_from_slice(b"***");
        pos = start + len;
    }
    if pos < keep {
        out.extend_from_slice(&input[pos..input.len().min(keep)]);
        pos = input.len().min(keep);
    }
    (
        String::from_utf8_lossy(&out).into_owned(),
        input.len().saturating_sub(pos),
    )
}

/// serve's stdout, through a queue of at most QUEUE_LINES lines and
/// QUEUE_BYTES bytes: a full queue drops lines and counts them.
pub struct StdoutQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

impl StdoutQueue {
    /// A queue whose lines a thread writes to `out`.
    pub fn start(mut out: Box<dyn Write + Send>) -> Arc<StdoutQueue> {
        let q = Arc::new(StdoutQueue {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        });
        let reader = q.clone();
        std::thread::spawn(move || {
            let mut noted = 0;
            loop {
                let (line, dropped) = {
                    let mut s = reader.lock();
                    while s.lines.is_empty() {
                        s = reader.ready.wait(s).unwrap_or_else(|e| e.into_inner());
                    }
                    let line = s.lines.pop_front().unwrap_or_default();
                    s.bytes -= line.len();
                    (line, s.dropped)
                };
                if dropped > noted {
                    let _ = writeln!(
                        out,
                        "… {} lines dropped: stdout was too slow",
                        dropped - noted
                    );
                    noted = dropped;
                }
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        });
        q
    }

    pub fn push(&self, line: String) {
        let mut s = self.lock();
        if s.lines.len() >= QUEUE_LINES || s.bytes + line.len() > QUEUE_BYTES {
            s.dropped += 1;
            return;
        }
        s.bytes += line.len();
        s.lines.push_back(line);
        drop(s);
        self.ready.notify_one();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lines dropped so far, for the metrics.
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }
}

/// A run's log file, which stops growing at its limit.
pub struct RunLog {
    file: std::fs::File,
    written: u64,
    max: u64,
    dropped: u64,
}

impl RunLog {
    pub fn open(path: &Path, max: u64) -> std::io::Result<RunLog> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata()?.len();
        Ok(RunLog {
            file,
            written,
            max,
            dropped: 0,
        })
    }

    pub fn line(&mut self, line: &str) {
        let n = line.len() as u64 + 1;
        if self.written + n > self.max {
            self.dropped += n;
            return;
        }
        if let Err(e) = writeln!(self.file, "{line}") {
            // A log is not a control record: the run goes on.
            eprintln!("fhir-pipes: warning: writing a run log: {e}");
            self.dropped += n;
            return;
        }
        self.written += n;
    }

    /// Say how much was dropped, if anything.
    pub fn close(&mut self) {
        if self.dropped > 0 {
            let _ = writeln!(
                self.file,
                "… {} bytes of output dropped: the log reached max_log_bytes ({})",
                self.dropped, self.max
            );
            self.dropped = 0;
        }
    }
}

/// The capture of one child's stdout and stderr.
pub struct Capture {
    threads: Vec<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Capture {
    /// Read `stdout` and `stderr` on two threads into `log` and `queue`,
    /// each line prefixed with `prefix` on serve's stdout.
    pub fn start(
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        log: Arc<Mutex<RunLog>>,
        queue: Arc<StdoutQueue>,
        redactor: Arc<Redactor>,
        prefix: String,
    ) -> Capture {
        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let reader = |input: Box<dyn Read + Send>, tail: Option<Arc<Mutex<VecDeque<String>>>>| {
            let (log, queue, redactor, prefix) =
                (log.clone(), queue.clone(), redactor.clone(), prefix.clone());
            std::thread::spawn(move || {
                let cap = LINE_CUT + redactor.longest();
                lines(input, cap, |line, extra| {
                    let (mut text, dropped) = cut_line(line, LINE_CUT, &redactor);
                    let dropped = dropped + extra;
                    if dropped > 0 {
                        text.push_str(&format!(" … {dropped} bytes of this line dropped"));
                    }
                    log.lock().unwrap_or_else(|e| e.into_inner()).line(&text);
                    if let Some(tail) = &tail {
                        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                        if t.len() == 50 {
                            t.pop_front();
                        }
                        t.push_back(text.clone());
                    }
                    queue.push(format!("{prefix} {text}"));
                });
            })
        };
        let threads = vec![
            reader(Box::new(stdout), None),
            reader(Box::new(stderr), Some(stderr_tail.clone())),
        ];
        Capture {
            threads,
            stderr_tail,
        }
    }

    /// Wait for both streams to end; the last 50 lines of stderr.
    pub fn finish(self) -> String {
        for t in self.threads {
            let _ = t.join();
        }
        let tail = self.stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// The last `lines` lines of `r` (`len` bytes long), read back from the end
/// in 64 KB blocks.
pub fn tail_from<R: Read + Seek>(r: &mut R, len: u64, lines: usize) -> std::io::Result<String> {
    const BLOCK: u64 = 64 * 1024;
    let mut start = len;
    let mut buf: Vec<u8> = Vec::new();
    // One newline more than lines, unless the file starts first; a final
    // newline does not count as a line.
    while start > 0 {
        let from = start.saturating_sub(BLOCK);
        let mut block = vec![0; (start - from) as usize];
        r.seek(SeekFrom::Start(from))?;
        r.read_exact(&mut block)?;
        block.extend_from_slice(&buf);
        buf = block;
        start = from;
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        if body.iter().filter(|&&b| b == b'\n').count() >= lines {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    // The first line of a block read from the middle may be partial.
    let skip_partial = usize::from(start > 0);
    let from = all
        .len()
        .saturating_sub(lines)
        .max(skip_partial.min(all.len()));
    Ok(all[from..].join("\n"))
}

pub fn tail(path: &Path, lines: usize) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    tail_from(&mut f, len, lines)
}

/// Read `r` a line at a time into `emit`, holding at most `cap` bytes of a
/// line; the rest of a longer line is counted, not kept.
fn lines(r: impl Read, cap: usize, mut emit: impl FnMut(&[u8], usize)) {
    let mut r = std::io::BufReader::with_capacity(64 * 1024, r);
    let mut line: Vec<u8> = Vec::new();
    let mut extra = 0;
    loop {
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if buf.is_empty() {
            if !line.is_empty() || extra > 0 {
                emit(&line, extra);
            }
            break;
        }
        let (part, ends) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], Some(i + 1)),
            None => (buf, None),
        };
        let room = cap.saturating_sub(line.len());
        line.extend_from_slice(&part[..part.len().min(room)]);
        extra += part.len().saturating_sub(room);
        let used = ends.unwrap_or(buf.len());
        r.consume(used);
        if ends.is_some() {
            emit(&line, extra);
            line.clear();
            extra = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn redactor(values: &[&str]) -> Redactor {
        Redactor::new(values.iter().map(|v| ("X".to_string(), v.to_string()))).0
    }

    const TOKEN: &str = "ABCDEFGHIJKLMNOP";

    #[test]
    fn a_secret_at_every_offset_around_the_cut_never_shows() {
        let r = redactor(&[TOKEN]);
        let keep = 100;
        for at in keep - 20..=keep + 20 {
            let mut input = "x".repeat(at).into_bytes();
            input.extend_from_slice(TOKEN.as_bytes());
            input.extend_from_slice(&b"y".repeat(50));
            let (out, dropped) = cut_line(&input, keep, &r);
            for n in 4..=TOKEN.len() {
                assert!(!out.contains(&TOKEN[..n]), "at {at}: {out}");
            }
            assert!(out.len() <= keep + 3, "at {at}");
            assert!(dropped > 0, "at {at}");
        }
    }

    #[test]
    fn an_earlier_replacement_does_not_pull_in_a_partial_secret() {
        let r = redactor(&[TOKEN]);
        let keep = 100;
        let mut input = TOKEN.as_bytes().to_vec();
        input.extend_from_slice(&b"x".repeat(keep + 8 - TOKEN.len()));
        input.extend_from_slice(TOKEN.as_bytes());
        let (out, _) = cut_line(&input, keep, &r);
        assert!(out.starts_with("***"), "{out}");
        assert!(!out.contains("ABCDE"), "{out}");
    }

    #[test]
    fn a_short_line_is_redacted_whole() {
        let r = redactor(&[TOKEN]);
        assert_eq!(
            cut_line(format!("a {TOKEN} b").as_bytes(), 100, &r),
            ("a *** b".to_string(), 0)
        );
    }

    #[test]
    fn the_log_stops_at_max_log_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut log = RunLog::open(&path, 100).unwrap();
        for _ in 0..20 {
            log.line(&"z".repeat(19));
        }
        log.close();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() < 200, "{}", text.len());
        assert!(text.contains("bytes of output dropped"), "{text}");
    }

    /// A writer that blocks until released.
    struct Blocked(Arc<(Mutex<bool>, Condvar)>, Arc<Mutex<Vec<u8>>>);

    impl Write for Blocked {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let (open, cv) = &*self.0;
            let mut o = open.lock().unwrap();
            while !*o {
                o = cv.wait(o).unwrap();
            }
            self.1.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_stdout_queue_drops_and_counts() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let q = StdoutQueue::start(Box::new(Blocked(gate.clone(), seen.clone())));
        for i in 0..QUEUE_LINES + 500 {
            q.push(format!("line {i}"));
        }
        assert!(q.dropped() >= 400, "{}", q.dropped());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let started = Instant::now();
        while !String::from_utf8_lossy(&seen.lock().unwrap()).contains("lines dropped") {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no note of the drop"
            );
            q.push("after".into());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Counts the bytes read through it.
    struct Counting<R>(R, u64);

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(b)?;
            self.1 += n as u64;
            Ok(n)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(p)
        }
    }

    #[test]
    fn tail_reads_only_the_end() {
        let line = "0123456789".repeat(10) + "\n";
        let text = line.repeat(200_000); // 20 MB
        let mut r = Counting(std::io::Cursor::new(text.as_bytes()), 0);
        let got = tail_from(&mut r, text.len() as u64, 50).unwrap();
        assert_eq!(got.lines().count(), 50);
        assert!(r.1 <= 128 * 1024, "read {} bytes", r.1);
    }

    #[test]
    fn a_slow_stdout_does_not_slow_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let q = StdoutQueue::start(Box::new(Blocked(gate.clone(), Arc::default())));
        let log = Arc::new(Mutex::new(
            RunLog::open(&dir.path().join("log"), 1_000_000).unwrap(),
        ));
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "yes 0123456789012345678901234567890123456789 | head -c 50000000",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let capture = Capture::start(
            child.stdout.take().unwrap(),
            child.stderr.take().unwrap(),
            log,
            q.clone(),
            Arc::new(Redactor::default()),
            "run".into(),
        );
        child.wait().unwrap();
        capture.finish();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(q.dropped() > 0);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    #[test]
    fn capture_redacts_cuts_and_keeps_the_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let q = StdoutQueue::start(Box::new(Blocked(gate, seen.clone())));
        let path = dir.path().join("log");
        let log = Arc::new(Mutex::new(RunLog::open(&path, 10_000_000).unwrap()));
        let script = format!(
            "echo 'token {TOKEN}'; printf '%100000s' '' | tr ' ' x; echo; echo 'fhir-pipes: views: down' >&2"
        );
        let mut child = std::process::Command::new("sh")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let capture = Capture::start(
            child.stdout.take().unwrap(),
            child.stderr.take().unwrap(),
            log.clone(),
            q,
            Arc::new(redactor(&[TOKEN])),
            "20261021T020000Z-7f3a".into(),
        );
        child.wait().unwrap();
        let tail = capture.finish();
        log.lock().unwrap().close();
        assert!(tail.contains("views: down"), "{tail}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("token ***"), "{text:.200}");
        assert!(!text.contains(TOKEN));
        assert!(
            text.contains("bytes of this line dropped"),
            "{}",
            &text[..200]
        );
        let long = text.lines().find(|l| l.starts_with("xxx")).unwrap();
        assert!(long.len() < LINE_CUT + 100, "{}", long.len());
        std::thread::sleep(Duration::from_millis(200));
        let out = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
        assert!(
            out.contains("20261021T020000Z-7f3a token ***"),
            "{out:.200}"
        );
    }
}
