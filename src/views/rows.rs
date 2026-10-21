//! `views run`: run views over FHIR files, stdin or PostgreSQL and write
//! their rows. One view from NDJSON to NDJSON is streamed a resource at a
//! time.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fhir_pipes_sof::output::{
    NdjsonWriter, OutputWriter, ParquetWriter, TemporalEncoding, WriteSummary, get_writer,
};
use fhir_pipes_sof::{ViewDefinition, ViewResult};

use super::run::ViewRows;
use super::{load_views, read_view};
use crate::error::usage;
use crate::fhir::chunks;
use crate::fhir::input::{
    for_each_chunk, is_ndjson, parse_resource_text, push_resource, resource_files,
};
use crate::fhir::ndjson::{Output, Splitter, is_gz, read_text};

/// `views run`: run `view` (a file, or a directory of views) over `input`
/// or the database `db`, and write the rows as `output` to `out` or stdout.
pub(super) async fn run(
    view: &Path,
    input: Option<&PathBuf>,
    db: Option<&str>,
    output: &str,
    out: Option<&PathBuf>,
    parquet_temporal: &str,
    threads: Option<usize>,
) -> Result<()> {
    let threads = threads.unwrap_or_else(crate::work::default_threads);
    // A directory of views runs every view in one pass, writing one
    // file per view into the --out directory.
    let multi = view.is_dir();

    if !multi {
        check_out(input, out, output)?;
    }

    // One view, NDJSON in and NDJSON out: stream it resource by
    // resource in bounded memory. A .ndjson (or .ndjson.gz) file is
    // taken at its word.
    let mut stdin_resources: Option<Vec<serde_json::Value>> = None;
    let stdin_in = input.is_some_and(|p| p.as_os_str() == "-");
    let ndjson_in = stdin_in || input.is_some_and(|p| is_ndjson(p));
    if !multi && output.eq_ignore_ascii_case("ndjson") && ndjson_in {
        let path = input.expect("checked above");
        match stream(view, path, stdin_in, out).await? {
            Some(resources) => stdin_resources = Some(resources),
            None => return Ok(()),
        }
    }

    let views = load_views(view)?;
    let mut source = match (input, db) {
        // Evaluation runs on threads of its own; block_in_place lets
        // this runtime worker wait for them without stalling other
        // tasks.
        (Some(path), _) => Source::Files(tokio::task::block_in_place(|| {
            run_on_files(&views, path, stdin_resources.take(), threads)
        })?),
        (None, Some(db)) => connect(db).await?,
        (None, None) => {
            return Err(usage(anyhow::anyhow!(
                "pass --input <file|dir|-> to run on files, or --db <url>"
            )));
        }
    };
    let writer = select_writer(output, parquet_temporal)?;

    if multi {
        write_each(&views, &mut source, writer.as_ref(), output, out).await
    } else {
        let (name, view) = &views[0];
        let result = view_rows(&mut source, 0, view).await?;
        let mut sink = open_sink(out)?;
        let summary = writer
            .write_with_summary(&result, &mut sink)
            .context("writing output")?;
        close_sink(sink, out)?;
        report_mismatches(&[(name.as_str(), summary)]);
        Ok(())
    }
}

/// The checks of --out for one view: not the --input file, and not gzip
/// for Parquet.
fn check_out(input: Option<&PathBuf>, out: Option<&PathBuf>, output: &str) -> Result<()> {
    check_out_is_not_input(input, out)?;
    // A Parquet file inside gzip is not a Parquet file any reader
    // opens, and Parquet compresses its own pages.
    if output.eq_ignore_ascii_case("parquet") && out.is_some_and(|o| is_gz(o)) {
        return Err(usage(anyhow::anyhow!(
            "--out ends in .gz, but Parquet is not written as gzip; \
             Parquet compresses its own pages, so name the file .parquet"
        )));
    }
    Ok(())
}

/// Streams NDJSON `input` (or stdin) through the one view at `view` into
/// `out`, and returns `None`. Stdin is streamed when its first line is one
/// whole JSON value; otherwise (formatted JSON, a Bundle or an array over
/// several lines) it is read whole, as for the other output formats, and
/// its resources are returned.
async fn stream(
    view: &Path,
    input: &Path,
    stdin_in: bool,
    out: Option<&PathBuf>,
) -> Result<Option<Vec<serde_json::Value>>> {
    let (_, view_def) = read_view(view)?;
    let path = input;
    let name = if stdin_in {
        "stdin".to_string()
    } else {
        path.display().to_string()
    };
    let reader: Box<dyn io::BufRead> = if stdin_in {
        Box::new(io::BufReader::new(io::stdin()))
    } else {
        read_text(
            fs::File::open(path)
                .with_context(|| format!("opening {}", path.display()))
                .map_err(|e| if path.exists() { e } else { usage(e) })?,
            is_gz(path),
        )
    };
    let mut lines = Splitter::new(reader);
    let first = next_line(&mut lines, &name)?;
    let one_value_per_line = !stdin_in
        || first
            .as_ref()
            .is_none_or(|(_, l)| serde_json::from_str::<serde::de::IgnoredAny>(l).is_ok());
    if one_value_per_line {
        let mut sink = open_sink(out)?;
        let n = stream_ndjson(&view_def, first, lines, &name, &mut sink).await?;
        close_sink(sink, out)?;
        eprintln!("{n} rows");
        return Ok(None);
    }
    // The first line, trimmed, and the rest as they are.
    let mut text = first.map(|(_, l)| l + "\n").unwrap_or_default();
    lines
        .into_inner()
        .read_to_string(&mut text)
        .context("reading FHIR resources from stdin")?;
    let mut resources = Vec::new();
    parse_resource_text(&text, &mut resources, view_def.resource == "Bundle")
        .context("parsing stdin")
        .map_err(usage)?;
    if resources.is_empty() {
        return Err(usage(anyhow::anyhow!("no FHIR resources on stdin")));
    }
    Ok(Some(resources))
}

/// A directory of views: each view's rows to `<name>.<ext>` in the --out
/// directory.
async fn write_each(
    views: &[(String, ViewDefinition)],
    source: &mut Source,
    writer: &dyn OutputWriter,
    output: &str,
    out: Option<&PathBuf>,
) -> Result<()> {
    let out_dir = out
        .context("--out <dir> is needed to run a directory of views")
        .map_err(usage)?;
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let ext = output_extension(output);
    let mut mismatches = Vec::new();
    for (i, (name, view)) in views.iter().enumerate() {
        let result = view_rows(source, i, view).await?;
        let path = out_dir.join(format!("{name}.{ext}"));
        let mut sink: Box<dyn Write> = Box::new(
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?,
        );
        let summary = writer
            .write_with_summary(&result, &mut sink)
            .with_context(|| format!("writing {}", path.display()))?;
        sink.flush().ok();
        eprintln!("{name} → {} ({} rows)", path.display(), result.data.len());
        mismatches.push((name.as_str(), summary));
    }
    report_mismatches(&mismatches);
    Ok(())
}

/// Where `run` gets each view's rows from.
enum Source {
    /// Every view's rows, already run over the files, in view order.
    Files(Vec<Option<ViewResult>>),
    #[cfg(feature = "postgres")]
    Db(sqlx_postgres::PgPool),
}

#[cfg(feature = "postgres")]
async fn connect(db: &str) -> Result<Source> {
    crate::http::ensure_crypto_provider();
    let pool = sqlx_postgres::PgPool::connect(db)
        .await
        .context("connecting to the database")?;
    Ok(Source::Db(pool))
}

#[cfg(not(feature = "postgres"))]
async fn connect(_db: &str) -> Result<Source> {
    Err(usage(anyhow::anyhow!(
        "--db needs PostgreSQL support, which this fhir-pipes was built without; \
         build it with `--features postgres`"
    )))
}

/// View `i`'s rows: from the run over the files, or from the database.
#[cfg_attr(not(feature = "postgres"), allow(unused_variables))]
async fn view_rows(source: &mut Source, i: usize, view: &ViewDefinition) -> Result<ViewResult> {
    match source {
        Source::Files(results) => Ok(results[i].take().expect("each view's rows are taken once")),
        #[cfg(feature = "postgres")]
        Source::Db(pool) => fhir_pipes_sof::ViewRunner::new(pool.clone())
            .run(view)
            .await
            .context("running view"),
    }
}

fn output_extension(format: &str) -> &'static str {
    match format.to_lowercase().as_str() {
        "ndjson" => "ndjson",
        "json" => "json",
        "parquet" => "parquet",
        _ => "csv",
    }
}

/// The writer for `format`. --parquet-temporal is accepted for every format
/// so scripts can pass it unconditionally, but only affects Parquet.
pub(crate) fn select_writer(format: &str, parquet_temporal: &str) -> Result<Box<dyn OutputWriter>> {
    if format.eq_ignore_ascii_case("parquet") {
        let temporal = TemporalEncoding::parse(parquet_temporal)
            .context("parsing --parquet-temporal")
            .map_err(usage)?;
        return Ok(Box::new(ParquetWriter::new().with_temporal(temporal)));
    }
    get_writer(format)
        .context("choosing the output format")
        .map_err(usage)
}

/// Print, on stderr, the values each view's writer could not write as they
/// are: `view.column: 3 values did not fit integer and were written as null`.
fn report_mismatches(views: &[(&str, WriteSummary)]) {
    let lines: Vec<String> = views
        .iter()
        .flat_map(|(view, s)| s.messages().into_iter().map(move |l| format!("{view}.{l}")))
        .collect();
    if lines.is_empty() {
        return;
    }
    eprintln!("values that did not match their column's type:");
    for l in lines {
        eprintln!("   {l}");
    }
}

/// Run `views` over the FHIR resources in a file, a directory of files
/// (searched recursively), or `-` for stdin, or over `given` (stdin, already
/// read). NDJSON (`.ndjson`), single resources, arrays of resources and
/// Bundles are read, an NDJSON file a chunk of lines at a time. Resources of
/// a type no view reads are dropped as they are read. Returns each view's
/// rows, in view order, the rows in the order of the resources.
fn run_on_files(
    views: &[(String, ViewDefinition)],
    path: &Path,
    given: Option<Vec<serde_json::Value>>,
    threads: usize,
) -> Result<Vec<Option<ViewResult>>> {
    let wanted: HashSet<&str> = views.iter().map(|(_, v)| v.resource.as_str()).collect();
    let keep_bundles = wanted.contains("Bundle");
    let mut rows = ViewRows::new(views.iter().map(|(n, v)| (n.as_str(), v)).collect());
    let mut add = |resources: Vec<serde_json::Value>| -> Result<()> {
        let mut by_type: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for r in resources {
            if let Some(t) = r.get("resourceType").and_then(|t| t.as_str())
                && wanted.contains(t)
            {
                by_type.entry(t.to_string()).or_default().push(r);
            }
        }
        for (t, resources) in by_type {
            rows.add_typed(&t, &resources, threads)?;
        }
        Ok(())
    };
    let count = if let Some(resources) = given {
        let n = resources.len();
        add(resources)?;
        n
    } else if path.as_os_str() == "-" {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text)
            .context("reading FHIR resources from stdin")?;
        let mut resources = Vec::new();
        parse_resource_text(&text, &mut resources, keep_bundles)
            .context("parsing stdin")
            .map_err(usage)?;
        if resources.is_empty() {
            return Err(usage(anyhow::anyhow!("no FHIR resources on stdin")));
        }
        let n = resources.len();
        add(resources)?;
        n
    } else {
        // An --input that is not there is a usage error; its message is the
        // read's.
        resource_files(path)
            .and_then(|files| for_each_chunk(&files, threads, keep_bundles, &mut add))
            .map_err(|e| if path.exists() { e } else { usage(e) })?
    };
    if count == 0 {
        return Err(usage(anyhow::anyhow!(
            "no FHIR resources in {}",
            path.display()
        )));
    }
    Ok(rows.finish(threads)?.into_iter().map(Some).collect())
}

/// The next non-blank line of `lines` and its 1-based number. `name` is the
/// input's name in messages.
fn next_line(
    lines: &mut Splitter<Box<dyn io::BufRead>>,
    name: &str,
) -> Result<Option<(usize, String)>> {
    let mut buf = Vec::new();
    let Some(span) = lines.next_into(&mut buf).map_err(|e| {
        chunks::read_error(e, format!("reading {name} line {}", lines.lines_read() + 1))
    })?
    else {
        return Ok(None);
    };
    buf.truncate(span.end);
    buf.drain(..span.start);
    let line = String::from_utf8(buf).map_err(|e| {
        let e = io::Error::new(io::ErrorKind::InvalidData, e.utf8_error());
        chunks::read_error(e, format!("reading {name} line {}", span.number))
    })?;
    Ok(Some((span.number, line)))
}

/// Stream NDJSON through one view, writing NDJSON rows as they are produced,
/// without holding the input or the output in memory. `first` is a line the
/// caller already read from `reader`. Returns the number of rows written.
async fn stream_ndjson(
    view: &ViewDefinition,
    first: Option<(usize, String)>,
    mut lines: Splitter<Box<dyn io::BufRead>>,
    name: &str,
    sink: &mut dyn Write,
) -> Result<usize> {
    let compiled = fhir_pipes_sof::CompiledView::compile(view)
        .await
        .context("compiling view")
        .map_err(usage)?;
    let mut total = 0;
    let mut next = first;
    while let Some((number, line)) = next {
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parsing {name} line {number}"))
            .map_err(usage)?;
        // A Bundle or array on one line is expanded into its resources.
        let mut resources = Vec::new();
        push_resource(value, &mut resources, view.resource == "Bundle");
        for resource in &resources {
            for row in compiled.execute_resource(resource).await? {
                let obj = NdjsonWriter::row_to_object(compiled.columns(), row);
                writeln!(sink, "{obj}").context("writing NDJSON row")?;
                total += 1;
            }
        }
        next = next_line(&mut lines, name)?;
    }
    Ok(total)
}

/// Refuse an --out that is the --input file: the output is opened for
/// writing before (or while) the input is read, which would empty it.
fn check_out_is_not_input(input: Option<&PathBuf>, out: Option<&PathBuf>) -> Result<()> {
    let (Some(input), Some(out)) = (input, out) else {
        return Ok(());
    };
    if input.as_os_str() == "-" || !out.exists() {
        return Ok(());
    }
    let same = match (input.canonicalize(), out.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        return Err(usage(anyhow::anyhow!(
            "--out {} is the --input file; writing it would empty the input",
            out.display()
        )));
    }
    Ok(())
}

/// The file `out` names, written as gzip when its name ends in `.gz`, or
/// stdout. `close_sink` finishes it.
pub(crate) fn open_sink(out: Option<&PathBuf>) -> Result<Output<Box<dyn Write>>> {
    Ok(match out {
        Some(path) => Output::new(
            Box::new(
                fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
            ),
            is_gz(path),
        ),
        None => Output::new(Box::new(io::stdout().lock()), false),
    })
}

/// Finish writing what `open_sink` opened: the end of a gzip stream, then
/// a flush. Both are buffered, so this is where most of a small output is
/// written, and an error here (a full disk, a closed pipe) fails the run.
pub(crate) fn close_sink(sink: Output<Box<dyn Write>>, out: Option<&PathBuf>) -> Result<()> {
    sink.finish().map(drop).with_context(|| match out {
        Some(path) => format!("writing {}", path.display()),
        None => "writing to stdout".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Value, json};

    fn is_usage(err: &anyhow::Error) -> bool {
        crate::error::exit_code(err) == 2
    }

    fn view(resource: &str, columns: Value) -> ViewDefinition {
        ViewDefinition::from_json(&json!({"resource": resource,
            "select": [{"column": columns}]}))
        .unwrap()
    }

    fn ids(result: &ViewResult) -> Vec<&str> {
        result.data.iter().map(|r| r[0].as_str().unwrap()).collect()
    }

    #[test]
    fn parquet_is_not_written_as_gzip_and_out_is_not_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let gz = dir.path().join("rows.parquet.gz");
        assert!(is_usage(
            &check_out(None, Some(&gz), "Parquet").unwrap_err()
        ));
        check_out(None, Some(&gz), "csv").unwrap();

        let input = dir.path().join("in.ndjson");
        fs::write(&input, "").unwrap();
        // The same file under another name.
        let same = dir.path().join(".").join("in.ndjson");
        let err = check_out(Some(&input), Some(&same), "ndjson").unwrap_err();
        assert!(is_usage(&err));
        assert!(err.to_string().contains("is the --input file"), "{err}");
        check_out(Some(&input), Some(&dir.path().join("out.ndjson")), "ndjson").unwrap();
        check_out(Some(&PathBuf::from("-")), Some(&input), "ndjson").unwrap();
    }

    #[test]
    fn writers_and_extensions_follow_the_format() {
        assert_eq!(output_extension("NDJSON"), "ndjson");
        assert_eq!(output_extension("parquet"), "parquet");
        assert_eq!(output_extension("csv"), "csv");
        assert_eq!(output_extension("anything"), "csv");
        select_writer("CSV", "native").unwrap();
        select_writer("parquet", "native").unwrap();
        assert!(is_usage(&select_writer("parquet", "epoch").err().unwrap()));
        assert!(is_usage(&select_writer("xml", "text").err().unwrap()));
    }

    #[test]
    fn files_give_each_view_the_rows_of_its_type_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.ndjson"),
            [
                json!({"resourceType": "Patient", "id": "p1"}),
                json!({"resourceType": "Encounter", "id": "e1"}),
                json!({"resourceType": "Observation", "id": "o1"}),
                json!({"resourceType": "Patient", "id": "p2"}),
            ]
            .map(|r| r.to_string())
            .join("\n"),
        )
        .unwrap();
        fs::write(
            dir.path().join("b.json"),
            json!({"resourceType": "Bundle", "type": "collection", "entry": [
                {"resource": {"resourceType": "Patient", "id": "p3"}}
            ]})
            .to_string(),
        )
        .unwrap();
        let id = json!([{"name": "id", "path": "id"}]);
        let views = vec![
            ("patients".to_string(), view("Patient", id.clone())),
            ("observations".to_string(), view("Observation", id.clone())),
            ("conditions".to_string(), view("Condition", id)),
        ];
        let results = run_on_files(&views, dir.path(), None, 2).unwrap();
        let results: Vec<ViewResult> = results.into_iter().map(Option::unwrap).collect();
        assert_eq!(ids(&results[0]), ["p1", "p2", "p3"]);
        assert_eq!(ids(&results[1]), ["o1"]);
        // A view no resource is for still has its columns.
        assert!(results[2].data.is_empty());
        assert_eq!(results[2].columns[0].name, "id");

        let empty = tempfile::tempdir().unwrap();
        fs::write(empty.path().join("none.ndjson"), "").unwrap();
        let err = run_on_files(&views, empty.path(), None, 2).unwrap_err();
        assert!(is_usage(&err));
        assert!(
            err.to_string().starts_with("no FHIR resources in "),
            "{err}"
        );
        let missing = dir.path().join("missing.ndjson");
        assert!(is_usage(
            &run_on_files(&views, &missing, None, 2).unwrap_err()
        ));
    }

    /// Stream `text` through `view`, as NDJSON.
    fn streamed(view: &ViewDefinition, text: &str) -> Result<(usize, String)> {
        let reader: Box<dyn io::BufRead> = Box::new(io::Cursor::new(text.as_bytes().to_vec()));
        let mut lines = Splitter::new(reader);
        let first = next_line(&mut lines, "in")?;
        let mut out = Vec::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let n = rt.block_on(stream_ndjson(view, first, lines, "in", &mut out))?;
        Ok((n, String::from_utf8(out).unwrap()))
    }

    #[test]
    fn ndjson_is_streamed_a_line_at_a_time() {
        let v = view(
            "Patient",
            json!([{"name": "id", "path": "id"}, {"name": "given", "path": "name.given",
                    "collection": true}]),
        );
        let text = "\n{\"resourceType\":\"Patient\",\"id\":\"p1\",\"name\":[{\"given\":[\"A\",\"B\"]}]}\n\n\
                    {\"resourceType\":\"Observation\",\"id\":\"o1\"}\n\
                    {\"resourceType\":\"Patient\",\"id\":\"p2\"}\n";
        let (n, out) = streamed(&v, text).unwrap();
        assert_eq!(n, 2);
        let rows: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows,
            [
                json!({"id": "p1", "given": ["A", "B"]}),
                json!({"id": "p2", "given": []})
            ]
        );
        let err = streamed(&v, "{\"resourceType\":\"Patient\"}\nnot json\n").unwrap_err();
        assert!(is_usage(&err));
        assert!(err.to_string().starts_with("parsing in line 2: "), "{err}");
        assert_eq!(streamed(&v, "\n\n").unwrap(), (0, String::new()));
    }
}
