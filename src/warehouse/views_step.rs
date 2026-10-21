//! The views step: download each resource type the views read, run the views
//! in memory, and write each view's table, partitioned, to the staging
//! directory, with what each table was built from for an upsert.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use anyhow::{Context, Result};
use fhir_pipes_sof::ViewResult;
use fhir_pipes_sof::output::WriteSummary;

use crate::error::usage;
use crate::views::run::{View, ViewRows};

use super::config::Settings;
use super::{fhir, partition, stage};

/// What the views step did.
pub(super) struct ViewsDone {
    /// Whether anything was downloaded from the server.
    pub(super) downloaded: bool,
    /// The tables the views wrote.
    pub(super) tables: BTreeSet<String>,
    /// For an upsert: each table with what its rows were built from
    /// (`fingerprint`).
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub(super) built: BTreeMap<String, String>,
    /// Each table's values that did not match their column's type, in the
    /// order the tables were written.
    pub(super) mismatches: Vec<(String, WriteSummary)>,
}

/// Download each resource type the views read, then run each type's views on
/// one read of it, a chunk of resources at a time, and write each view's
/// table, partitioned by the lookup rules, to the staging directory, and the
/// views to the staged views/. A type's tables are written as soon as its
/// views are done, and its rows then dropped, except that the types whose
/// views write lookup tables run first and their tables are all written
/// before any other (`write_order`).
pub(super) fn views_step(s: &Settings, stage: &stage::Stage) -> Result<ViewsDone> {
    let dir = s
        .views
        .as_ref()
        .context("no views directory")
        .map_err(usage)?;
    println!("== views ({})", dir.display());
    // A missing, unreadable or invalid ViewDefinition is the operator's to fix.
    let all = crate::views::run::discover(dir, &s.strip_prefix).map_err(usage)?;
    if all.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "no ViewDefinitions in {}",
            dir.display()
        )));
    }
    crate::views::run::save(&all, &stage.dir("views"))?;

    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, v) in all.iter().enumerate() {
        by_type.entry(v.resource.as_str()).or_default().push(i);
    }
    let ndjson = |t: &str| s.raw_dir().join(format!("{t}.ndjson"));
    let download: Vec<&str> = by_type
        .keys()
        .copied()
        .filter(|t| {
            !s.reuse_raw
                || std::fs::metadata(ndjson(t)).is_ok_and(|m| m.len() == 0)
                || !ndjson(t).exists()
        })
        .collect();
    let downloaded = !s.reads_files() && !download.is_empty();
    if downloaded {
        fhir::export(s, &download)?;
    }

    // Each type's views, run over one read of its file.
    let run_type = |rtype: &str, idx: &[usize]| -> Result<Vec<(usize, ViewResult)>> {
        crate::stop::check()?;
        let t = Instant::now();
        let vs: Vec<&View> = idx.iter().map(|&i| &all[i]).collect();
        // A type the files lack gives its views empty tables.
        let (n, results) = if s.reads_files() && !ndjson(rtype).exists() {
            println!("   {rtype}: none in the files; its views write empty tables");
            let rows = ViewRows::new(
                vs.iter()
                    .map(|v| (v.name.as_str(), &v.definition))
                    .collect(),
            );
            (0, rows.finish(s.threads)?)
        } else {
            crate::views::run::run_file(&vs, &ndjson(rtype), s.threads)?
        };
        println!(
            "   {rtype}: {n} resources, {} view(s), {:.1}s",
            idx.len(),
            t.elapsed().as_secs_f64()
        );
        Ok(idx.iter().copied().zip(results).collect())
    };

    // Tables other views look their partition up in are written first, and
    // read back from the staging directory: the types whose views write them
    // run first, and their tables are written once they have all run, the
    // lookup tables first, each group in view order, as if every view had
    // run before any table was written.
    let sources: BTreeSet<&str> = s.lookups.iter().map(|l| l.table.as_str()).collect();
    let is_source = |i: usize| sources.contains(all[i].table.as_str());
    let write_order = |i: usize| (!is_source(i), i);
    let (first, rest): (Vec<_>, Vec<_>) = by_type
        .iter()
        .partition(|(_, idx)| idx.iter().any(|&i| is_source(i)));

    let mut writer = TableWriter {
        s,
        stage,
        lookups: HashMap::new(),
        digests: HashMap::new(),
        built: BTreeMap::new(),
        mismatches: Vec::new(),
    };
    let mut held = Vec::new();
    for (rtype, idx) in first {
        held.extend(run_type(rtype, idx)?);
    }
    held.sort_by_key(|(i, _)| write_order(*i));
    for (i, result) in held {
        writer.write(i, &all[i], result)?;
    }
    for (rtype, idx) in rest {
        for (i, result) in run_type(rtype, idx)? {
            writer.write(i, &all[i], result)?;
        }
    }
    // The type mismatches in the order the tables were written before they
    // were written a type at a time.
    let mut mismatches = writer.mismatches;
    mismatches.sort_by_key(|(i, _, _)| write_order(*i));
    Ok(ViewsDone {
        built: writer.built,
        downloaded,
        mismatches: mismatches.into_iter().map(|(_, t, w)| (t, w)).collect(),
        tables: all.into_iter().map(|v| v.table).collect(),
    })
}

/// What a table's rows are built from besides its resources, for an upsert,
/// which rewrites only the rows of resources that changed: its view, its
/// partition column and lookup (`lookup`, with a digest of the lookup's
/// keys and values), and the fhir-pipes version, whose views engine could
/// give other rows. When it differs from the last load's, the table is
/// loaded in full.
fn fingerprint(v: &View, partition_column: &str, lookup: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [
        env!("CARGO_PKG_VERSION"),
        &v.json.to_string(),
        partition_column,
        lookup,
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    crate::fs::hex(&h.finalize())
}

/// A digest of a lookup: its keys and values, sorted.
fn lookup_digest(m: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<(&String, &String)> = m.iter().collect();
    entries.sort();
    let mut h = Sha256::new();
    for (k, v) in entries {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    crate::fs::hex(&h.finalize())
}

/// Writes the views' tables to the staging directory, reading the lookup
/// tables their partitions come from once each.
struct TableWriter<'a> {
    s: &'a Settings,
    stage: &'a stage::Stage,
    /// (table, key) → key → partition value.
    lookups: HashMap<(String, String), HashMap<String, String>>,
    /// (table, key) → a digest of that lookup, for an upsert.
    digests: HashMap<(String, String), String>,
    /// Each table written, with its fingerprint, for an upsert.
    built: BTreeMap<String, String>,
    /// Each table written, by its view's index, with the values that did not
    /// match their column's type.
    mismatches: Vec<(usize, String, WriteSummary)>,
}

impl TableWriter<'_> {
    /// Write view `i`'s table from its rows.
    fn write(&mut self, i: usize, v: &View, result: ViewResult) -> Result<()> {
        let (s, stage) = (self.s, self.stage);
        // The partition column is added to every row; a view column of the
        // same name would be lost beside it.
        if result.columns.iter().any(|c| c.name == s.partition_column) {
            return Err(usage(anyhow::anyhow!(
                "view {} has a column `{}`, which is the partition column; \
                 set [partition] column to another name",
                v.name,
                s.partition_column
            )));
        }
        let rule = partition::rule_for(&result, &s.lookups);
        let mut map: Option<&HashMap<String, String>> = None;
        let mut lookup_table = None;
        if let Some((l, _)) = rule {
            let table_dir = stage.read(&l.table);
            let key = l.table_key.clone().unwrap_or_else(|| l.key.clone());
            lookup_table = Some((l, key.clone(), table_dir.is_dir()));
            if table_dir.is_dir() {
                let cache_key = (l.table.clone(), key.clone());
                if !self.lookups.contains_key(&cache_key) {
                    let m = partition::read_lookup(&table_dir, &key, &s.partition_column)
                        .with_context(|| format!("reading lookup table {}", l.table))?;
                    if s.upsert() {
                        self.digests.insert(cache_key.clone(), lookup_digest(&m));
                    }
                    self.lookups.insert(cache_key.clone(), m);
                }
                map = self.lookups.get(&cache_key);
            }
        }
        if s.upsert() {
            let lookup = match &lookup_table {
                None => "none".to_string(),
                Some((l, key, present)) => format!(
                    "{} in {}.{}: {}",
                    l.key,
                    l.table,
                    key,
                    if *present {
                        &self.digests[&(l.table.clone(), key.clone())]
                    } else {
                        "no table"
                    }
                ),
            };
            self.built.insert(
                v.table.clone(),
                fingerprint(v, &s.partition_column, &lookup),
            );
        }
        let col = rule.map(|(_, c)| c);
        // Rows whose key is null, and rows whose key is not in the lookup
        // table (or is not text: the lookup's keys are read as text), for
        // the warning below.
        let no_key = Cell::new(0usize);
        let not_found = Cell::new(0usize);
        let (n, summary) =
            partition::write(result, &stage.table(&v.table), &s.partition_column, |row| {
                let c = col?;
                if row[c].is_null() {
                    no_key.set(no_key.get() + 1);
                    return None;
                }
                let found = map.and_then(|m| row[c].as_str().and_then(|k| m.get(k)).cloned());
                if found.is_none() {
                    not_found.set(not_found.get() + 1);
                }
                found
            })?;
        println!(
            "   {} ({}) → parquet/{} ({n} rows)",
            v.name, v.resource, v.table
        );
        self.mismatches.push((i, v.table.clone(), summary));
        if let Some((l, key, present)) = lookup_table {
            let (no_key, not_found) = (no_key.get(), not_found.get());
            if !present {
                println!(
                    "   warning: parquet/{}: {} of {n} rows in the null partition: there is no \
                     lookup table parquet/{} to find `{}` in",
                    v.table,
                    no_key + not_found,
                    l.table,
                    l.key
                );
            } else if no_key + not_found > 0 {
                let mut why = Vec::new();
                if not_found > 0 {
                    why.push(format!(
                        "{not_found} with a `{}` not in {}.{key}",
                        l.key, l.table
                    ));
                }
                if no_key > 0 {
                    why.push(format!("{no_key} without a `{}`", l.key));
                }
                println!(
                    "   warning: parquet/{}: {} of {n} rows in the null partition ({})",
                    v.table,
                    no_key + not_found,
                    why.join(", ")
                );
            }
        }
        Ok(())
    }
}
