//! Resolve partOf chains into depth, ancestor indices and admin columns.
//!
//! No chain cache: each node's status (Unvisited / InProgress / Done /
//! Dropped) is tracked in a single pass, so every node is walked at most
//! once and the total work across the whole call is O(n) regardless of
//! how many nodes share an ancestor chain. `HierarchyInfo` stores indices
//! into the `records` slice it was built from rather than cloned strings,
//! keeping it small (see `size_of_hierarchy_info_is_small` below); id
//! strings are looked back up in `records` only on demand, and only for
//! the handful of nodes a caller actually asks about.
//!
//! Duplicate ids resolve to the first occurrence (the Python used the
//! last); deliberate.

use std::collections::HashMap;
use std::num::NonZeroU32;

use crate::index::IndexRecord;
use crate::report::Report;

pub const MAX_DEPTH: usize = 12;
pub const ADMIN_COLUMNS: usize = 5;

/// Bound on how many ids a report `detail` string reconstructs, so a
/// malformed chain or cycle of size n costs O(n) to report, not O(n^2).
const DETAIL_CAP: usize = 32;

/// A record index, stored as `NonZeroU32(index + 1)` so `Option<NodeIdx>`
/// fits in the niche and costs 4 bytes instead of the 8 bytes a plain
/// `Option<u32>` needs (`u32` has no spare bit pattern to steal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeIdx(NonZeroU32);

impl NodeIdx {
    fn new(i: u32) -> Self {
        NodeIdx(NonZeroU32::new(i + 1).expect("record index too large"))
    }

    pub fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

/// Per-node result. Strings are derived on demand from `records` via
/// `Hierarchy`'s methods, so this is small: 28 bytes today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HierarchyInfo {
    pub depth: u8,
    /// Number of admin-unit nodes on the root..node chain, including the node itself.
    pub admin_count: u8,
    /// Some only when the node is itself an admin unit: admin_count - 1.
    pub admin_level: Option<i8>,
    pub parent: Option<NodeIdx>,
    /// Indices of the first ADMIN_COLUMNS admin-unit nodes on the chain, root first.
    pub admin: [Option<NodeIdx>; ADMIN_COLUMNS],
}

/// Parallel to the records slice it was built from. `None` = dropped (duplicate id, cycle, unreachable, too deep).
#[derive(Debug, Default)]
pub struct Hierarchy {
    pub infos: Vec<Option<HierarchyInfo>>,
}

impl Hierarchy {
    pub fn get(&self, i: usize) -> Option<&HierarchyInfo> {
        self.infos.get(i).and_then(|o| o.as_ref())
    }

    /// Root-first ids of the strict ancestors of record i.
    pub fn ancestor_ids(&self, records: &[IndexRecord], i: usize) -> Vec<String> {
        let mut ids = Vec::new();
        let mut cur = self.get(i).and_then(|info| info.parent);
        while let Some(nidx) = cur {
            let idx = nidx.index();
            ids.push(records[idx].id.clone());
            cur = self.infos[idx].and_then(|info| info.parent);
        }
        ids.reverse();
        ids
    }

    /// "/root/.../node".
    pub fn path(&self, records: &[IndexRecord], i: usize) -> String {
        let mut ids = self.ancestor_ids(records, i);
        ids.push(records[i].id.clone());
        format!("/{}", ids.join("/"))
    }

    pub fn admin_names(
        &self,
        records: &[IndexRecord],
        i: usize,
    ) -> [Option<String>; ADMIN_COLUMNS] {
        let mut out: [Option<String>; ADMIN_COLUMNS] = Default::default();
        if let Some(info) = self.get(i) {
            for (k, slot) in info.admin.iter().enumerate() {
                out[k] = slot.and_then(|nidx| records[nidx.index()].name.clone());
            }
        }
        out
    }

    pub fn admin_codes(
        &self,
        records: &[IndexRecord],
        i: usize,
    ) -> [Option<String>; ADMIN_COLUMNS] {
        let mut out: [Option<String>; ADMIN_COLUMNS] = Default::default();
        if let Some(info) = self.get(i) {
            for (k, slot) in info.admin.iter().enumerate() {
                out[k] = slot.and_then(|nidx| records[nidx.index()].admin_code.clone());
            }
        }
        out
    }

    /// Admin code of the level-0 admin ancestor, if any.
    pub fn country(&self, records: &[IndexRecord], i: usize) -> Option<String> {
        self.get(i)?.admin[0].and_then(|n| records[n.index()].admin_code.clone())
    }
}

fn is_admin(rec: &IndexRecord) -> bool {
    rec.is_admin
}

#[derive(Clone, Copy)]
enum Reason {
    Cycle { entry: u32 },
    TooDeep { depth: u8 },
}

#[derive(Clone, Copy)]
enum Status {
    Unvisited,
    InProgress,
    Done(HierarchyInfo),
    Dropped(Reason),
}

enum Outcome {
    /// The walk reached a node with no parent; it is pushed onto `path` already.
    Root,
    /// The walk reached an already-resolved node.
    Done(u32, HierarchyInfo),
    /// The walk reached a node already InProgress on this same path: a cycle.
    Cycle(u32),
    /// The walk reached a node already dropped in an earlier walk.
    Dropped(Reason),
}

/// The `cap` ids closest to `start` on its root..start chain (root-first
/// order), reconstructed from `parent` at report time (only ever called
/// for the handful of nodes actually reported). Stops after `cap` ancestors
/// rather than walking to the true root, so a single call is O(cap) even
/// on a chain of a million nodes; the bool says whether it was cut short.
fn root_chain_ids<'a>(
    start: u32,
    parent: &[Option<u32>],
    records: &'a [IndexRecord],
    cap: usize,
) -> (Vec<&'a str>, bool) {
    let mut ids = Vec::new();
    let mut idx = start;
    let mut truncated = false;
    loop {
        if ids.len() >= cap {
            truncated = true;
            break;
        }
        ids.push(records[idx as usize].id.as_str());
        match parent[idx as usize] {
            Some(p) => idx = p,
            None => break,
        }
    }
    ids.reverse();
    (ids, truncated)
}

/// Assign HierarchyInfo to every node in `path` (child-first order, as
/// built by the walk), starting from `prev` (the already-resolved node
/// closer to the root, or None if `path`'s last element is itself a root).
fn propagate(
    path: &[u32],
    mut prev: Option<(u32, HierarchyInfo)>,
    records: &[IndexRecord],
    parent: &[Option<u32>],
    status: &mut [Status],
    report: &mut Report,
) {
    let mut too_deep = false;
    for &idx in path.iter().rev() {
        let depth = match prev {
            Some((_, info)) => info.depth.saturating_add(1),
            None => 0,
        };

        if too_deep || depth as usize >= MAX_DEPTH {
            too_deep = true;
            let (chain, truncated) = root_chain_ids(idx, parent, records, DETAIL_CAP);
            let prefix = if truncated { "... -> " } else { "" };
            report.add(
                "too_deep",
                &records[idx as usize].id,
                &format!(
                    "chain exceeds MAX_DEPTH={MAX_DEPTH}: {prefix}{}",
                    chain.join(" -> ")
                ),
            );
            status[idx as usize] = Status::Dropped(Reason::TooDeep { depth });
            // Keep a dummy info around purely to carry `depth` forward so the
            // next node's depth keeps incrementing; its other fields are
            // never read (nothing after this can become Done).
            prev = Some((
                idx,
                HierarchyInfo {
                    depth,
                    admin_count: 0,
                    admin_level: None,
                    parent: None,
                    admin: [None; ADMIN_COLUMNS],
                },
            ));
            continue;
        }

        let rec = &records[idx as usize];
        let admin = is_admin(rec);
        let (mut admin_count, mut admin_arr) = match prev {
            Some((_, info)) => (info.admin_count, info.admin),
            None => (0u8, [None; ADMIN_COLUMNS]),
        };
        if admin {
            admin_count += 1;
            if (admin_count as usize) <= ADMIN_COLUMNS {
                admin_arr[(admin_count - 1) as usize] = Some(NodeIdx::new(idx));
            } else {
                report.add(
                    "admin_level_beyond_columns",
                    &rec.id,
                    &format!(
                        "admin level {} is beyond the {ADMIN_COLUMNS} admin columns; \
                         not present in adminN_name/adminN_code",
                        admin_count - 1
                    ),
                );
            }
        }
        let admin_level = admin.then(|| (admin_count - 1) as i8);
        let parent_field = prev.map(|(pidx, _)| NodeIdx::new(pidx));
        let info = HierarchyInfo {
            depth,
            admin_count,
            admin_level,
            parent: parent_field,
            admin: admin_arr,
        };
        status[idx as usize] = Status::Done(info);
        prev = Some((idx, info));
    }
}

/// `path` (in the walk's child-first order) hit a node already InProgress
/// on the same path: `cyc_node`. Everything from that position onward is
/// on the cycle itself; everything before it merely leads into the cycle.
fn handle_cycle(
    path: &[u32],
    cyc_node: u32,
    records: &[IndexRecord],
    status: &mut [Status],
    report: &mut Report,
) {
    let pos = path
        .iter()
        .position(|&x| x == cyc_node)
        .expect("a detected cycle's entry node must be on the current path");
    let cycle_len = path.len() - pos;

    for j in pos..path.len() {
        let idx = path[j];
        let detail = if cycle_len <= DETAIL_CAP {
            let mut seq: Vec<u32> = Vec::with_capacity(cycle_len + 1);
            for k in 0..cycle_len {
                seq.push(path[pos + (j - pos + k) % cycle_len]);
            }
            seq.push(idx);
            seq.iter()
                .map(|&i| records[i as usize].id.as_str())
                .collect::<Vec<_>>()
                .join(" -> ")
        } else {
            // Full rotation is O(cycle_len) per member, O(cycle_len^2)
            // overall; cap it at DETAIL_CAP ids starting from this member.
            let mut seq: Vec<u32> = Vec::with_capacity(DETAIL_CAP);
            for k in 0..DETAIL_CAP {
                seq.push(path[pos + (j - pos + k) % cycle_len]);
            }
            let joined = seq
                .iter()
                .map(|&i| records[i as usize].id.as_str())
                .collect::<Vec<_>>()
                .join(" -> ");
            format!("{joined} ... (cycle of {cycle_len} nodes)")
        };
        report.add("cycle", &records[idx as usize].id, &detail);
        status[idx as usize] = Status::Dropped(Reason::Cycle { entry: cyc_node });
    }

    for &idx in &path[..pos] {
        report.add(
            "unreachable_ancestor",
            &records[idx as usize].id,
            &format!("cycle involving {}", records[cyc_node as usize].id),
        );
        status[idx as usize] = Status::Dropped(Reason::Cycle { entry: cyc_node });
    }
}

/// `path` hit a node already dropped in an earlier walk; relay the same
/// fate onto every newly-discovered node in `path`.
fn handle_dropped(
    path: &[u32],
    reason: Reason,
    records: &[IndexRecord],
    parent: &[Option<u32>],
    status: &mut [Status],
    report: &mut Report,
) {
    match reason {
        Reason::Cycle { entry } => {
            for &idx in path.iter().rev() {
                report.add(
                    "unreachable_ancestor",
                    &records[idx as usize].id,
                    &format!("cycle involving {}", records[entry as usize].id),
                );
                status[idx as usize] = Status::Dropped(Reason::Cycle { entry });
            }
        }
        Reason::TooDeep { depth: base_depth } => {
            let mut depth = base_depth.saturating_add(1);
            for &idx in path.iter().rev() {
                let (chain, truncated) = root_chain_ids(idx, parent, records, DETAIL_CAP);
                let prefix = if truncated { "... -> " } else { "" };
                report.add(
                    "too_deep",
                    &records[idx as usize].id,
                    &format!(
                        "chain exceeds MAX_DEPTH={MAX_DEPTH}: {prefix}{}",
                        chain.join(" -> ")
                    ),
                );
                status[idx as usize] = Status::Dropped(Reason::TooDeep { depth });
                depth = depth.saturating_add(1);
            }
        }
    }
}

pub fn resolve_hierarchy(records: &[IndexRecord], report: &mut Report) -> Hierarchy {
    let n = records.len();

    // Ids map to the index of their FIRST occurrence; later ones are
    // reported and never walked.
    let mut by_id: HashMap<&str, u32> = HashMap::with_capacity(n);
    let mut is_duplicate = vec![false; n];
    for (i, r) in records.iter().enumerate() {
        if by_id.contains_key(r.id.as_str()) {
            report.add(
                "duplicate_id",
                &r.id,
                "id appears more than once; the first occurrence is used",
            );
            is_duplicate[i] = true;
        } else {
            by_id.insert(r.id.as_str(), i as u32);
        }
    }

    // Pre-walk: report every dangling parent exactly once.
    for r in records {
        if let Some(p) = &r.part_of
            && !by_id.contains_key(p.as_str())
        {
            report.add(
                "orphan",
                &r.id,
                &format!("partOf references missing id {p}"),
            );
        }
    }

    let parent: Vec<Option<u32>> = records
        .iter()
        .map(|r| r.part_of.as_deref().and_then(|p| by_id.get(p).copied()))
        .collect();

    let mut status: Vec<Status> = vec![Status::Unvisited; n];

    for start in 0..n {
        if is_duplicate[start] || !matches!(status[start], Status::Unvisited) {
            continue;
        }

        let mut path: Vec<u32> = Vec::new();
        let mut current = start as u32;
        let outcome = loop {
            match status[current as usize] {
                Status::Unvisited => {
                    status[current as usize] = Status::InProgress;
                    path.push(current);
                    match parent[current as usize] {
                        Some(p) => current = p,
                        None => break Outcome::Root,
                    }
                }
                Status::InProgress => break Outcome::Cycle(current),
                Status::Done(info) => break Outcome::Done(current, info),
                Status::Dropped(reason) => break Outcome::Dropped(reason),
            }
        };

        match outcome {
            Outcome::Root => propagate(&path, None, records, &parent, &mut status, report),
            Outcome::Done(idx, info) => propagate(
                &path,
                Some((idx, info)),
                records,
                &parent,
                &mut status,
                report,
            ),
            Outcome::Cycle(cyc_node) => handle_cycle(&path, cyc_node, records, &mut status, report),
            Outcome::Dropped(reason) => {
                handle_dropped(&path, reason, records, &parent, &mut status, report)
            }
        }
    }

    let infos = (0..n)
        .map(|i| match status[i] {
            Status::Done(info) => Some(info),
            _ => None,
        })
        .collect();

    Hierarchy { infos }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexRecord;
    use crate::report::Report;
    use std::collections::BTreeSet;

    fn rec(id: &str, parent: Option<&str>, ty: &str, pcode: Option<&str>) -> IndexRecord {
        IndexRecord {
            id: id.into(),
            part_of: parent.map(str::to_string),
            name: Some(id.to_uppercase()),
            admin_code: pcode.map(str::to_string),
            type_code: Some(ty.into()),
            is_admin: ty == "admin-unit",
            ..Default::default()
        }
    }

    fn fixture() -> Vec<IndexRecord> {
        vec![
            rec("ng", None, "admin-unit", Some("NG")),
            rec("kano", Some("ng"), "admin-unit", Some("NG001")),
            rec("nassarawa", Some("kano"), "admin-unit", Some("NG001002")),
            rec("gama", Some("nassarawa"), "settlement", None),
            rec("clinic", Some("gama"), "facility", None),
            rec("orphan", Some("does-not-exist"), "facility", None),
            rec("cyc-a", Some("cyc-b"), "admin-unit", None),
            rec("cyc-b", Some("cyc-a"), "admin-unit", None),
            rec("victim", Some("cyc-a"), "facility", None),
        ]
    }

    fn idx(records: &[IndexRecord], id: &str) -> usize {
        records.iter().position(|r| r.id == id).unwrap()
    }

    #[test]
    fn walks_past_non_admin_ancestors() {
        let recs = fixture();
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        let clinic = idx(&recs, "clinic");
        assert_eq!(h.get(clinic).unwrap().depth, 4);
        assert_eq!(h.get(clinic).unwrap().admin_level, None);
        assert_eq!(h.path(&recs, clinic), "/ng/kano/nassarawa/gama/clinic");
        assert_eq!(
            h.ancestor_ids(&recs, clinic),
            vec!["ng", "kano", "nassarawa", "gama"]
        );
        let names = h.admin_names(&recs, clinic);
        assert_eq!(names[0].as_deref(), Some("NG"));
        assert_eq!(names[2].as_deref(), Some("NASSARAWA"));
        assert_eq!(names[3], None);
        let codes = h.admin_codes(&recs, clinic);
        assert_eq!(codes[2].as_deref(), Some("NG001002"));
        assert_eq!(h.country(&recs, clinic).as_deref(), Some("NG"));

        let nassarawa = idx(&recs, "nassarawa");
        assert_eq!(h.get(nassarawa).unwrap().admin_level, Some(2));

        let ng = idx(&recs, "ng");
        assert_eq!(h.get(ng).unwrap().depth, 0);
    }

    #[test]
    fn admin_levels_beyond_admin_columns_are_reported_once_each() {
        // A 7-level admin chain: levels 0..4 fit in ADMIN_COLUMNS=5, levels
        // 5 and 6 don't and must each be reported exactly once.
        let mut recs = vec![rec("l0", None, "admin-unit", Some("L0"))];
        for i in 1..7 {
            recs.push(rec(
                &format!("l{i}"),
                Some(&format!("l{}", i - 1)),
                "admin-unit",
                None,
            ));
        }
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        assert_eq!(report.count("admin_level_beyond_columns"), 2);
        let flagged: BTreeSet<&str> = report
            .issues
            .iter()
            .filter(|issue| issue.kind == "admin_level_beyond_columns")
            .map(|issue| issue.location_id.as_str())
            .collect();
        assert_eq!(flagged, BTreeSet::from(["l5", "l6"]));

        for i in 0..5 {
            let n = idx(&recs, &format!("l{i}"));
            assert_eq!(h.get(n).unwrap().admin_level, Some(i as i8));
        }
        for i in 5..7 {
            let n = idx(&recs, &format!("l{i}"));
            assert_eq!(h.get(n).unwrap().admin_level, Some(i as i8));
        }
    }

    #[test]
    fn dangling_parent_is_reported_once_and_treated_as_root() {
        let recs = fixture();
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        assert_eq!(report.count("orphan"), 1);
        let orphan = idx(&recs, "orphan");
        assert_eq!(h.get(orphan).unwrap().depth, 0);
        assert_eq!(h.country(&recs, orphan), None);
    }

    #[test]
    fn cycles_are_reported_and_dropped_with_their_descendants() {
        let recs = fixture();
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        assert!(h.get(idx(&recs, "cyc-a")).is_none());
        assert!(h.get(idx(&recs, "victim")).is_none());
        assert_eq!(report.count("cycle"), 2);
        assert_eq!(report.count("unreachable_ancestor"), 1);
    }

    #[test]
    fn too_deep_chains_are_reported() {
        let mut recs = vec![rec("n0", None, "admin-unit", Some("X"))];
        for i in 1..20 {
            recs.push(rec(
                &format!("n{i}"),
                Some(&format!("n{}", i - 1)),
                "admin-unit",
                None,
            ));
        }
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);
        // MAX_DEPTH=12: Python's _chain raises _TooDeepError when
        // len(walked) > MAX_DEPTH, so a chain of 13 nodes (n0..n12) is
        // already too deep. The deepest resolvable node is n11 (chain of
        // 12 nodes, depth 11); n12..n19 (8 nodes) are too_deep. kiln's
        // Python test_max_depth_boundary pinned this exact boundary.
        assert!(h.get(idx(&recs, "n11")).is_some());
        assert!(h.get(idx(&recs, "n12")).is_none());
        assert_eq!(report.count("too_deep"), 8);
    }

    #[test]
    fn results_do_not_depend_on_record_order() {
        let fwd = fixture();
        let mut rev = fwd.clone();
        rev.reverse();

        let mut report_fwd = Report::default();
        let h_fwd = resolve_hierarchy(&fwd, &mut report_fwd);
        let mut report_rev = Report::default();
        let h_rev = resolve_hierarchy(&rev, &mut report_rev);

        for r in &fwd {
            let i_fwd = idx(&fwd, &r.id);
            let i_rev = idx(&rev, &r.id);
            let info_fwd = h_fwd.get(i_fwd);
            let info_rev = h_rev.get(i_rev);
            assert_eq!(
                info_fwd.is_some(),
                info_rev.is_some(),
                "resolved mismatch for {}",
                r.id
            );
            if let (Some(a), Some(b)) = (info_fwd, info_rev) {
                assert_eq!(a.depth, b.depth, "depth mismatch for {}", r.id);
                assert_eq!(
                    a.admin_level, b.admin_level,
                    "admin_level mismatch for {}",
                    r.id
                );
                assert_eq!(
                    h_fwd.path(&fwd, i_fwd),
                    h_rev.path(&rev, i_rev),
                    "path mismatch for {}",
                    r.id
                );
                assert_eq!(
                    h_fwd.ancestor_ids(&fwd, i_fwd),
                    h_rev.ancestor_ids(&rev, i_rev),
                    "ancestor_ids mismatch for {}",
                    r.id
                );
                assert_eq!(
                    h_fwd.admin_names(&fwd, i_fwd),
                    h_rev.admin_names(&rev, i_rev),
                    "admin_names mismatch for {}",
                    r.id
                );
                assert_eq!(
                    h_fwd.admin_codes(&fwd, i_fwd),
                    h_rev.admin_codes(&rev, i_rev),
                    "admin_codes mismatch for {}",
                    r.id
                );
                assert_eq!(
                    h_fwd.country(&fwd, i_fwd),
                    h_rev.country(&rev, i_rev),
                    "country mismatch for {}",
                    r.id
                );
            }
        }
        assert_eq!(report_fwd.counts(), report_rev.counts());
    }

    #[test]
    fn too_deep_is_order_independent() {
        let mut recs = vec![rec("n0", None, "admin-unit", Some("X"))];
        for i in 1..20 {
            recs.push(rec(
                &format!("n{i}"),
                Some(&format!("n{}", i - 1)),
                "admin-unit",
                None,
            ));
        }
        let mut rev = recs.clone();
        rev.reverse();

        let mut report_fwd = Report::default();
        let h_fwd = resolve_hierarchy(&recs, &mut report_fwd);
        let mut report_rev = Report::default();
        let h_rev = resolve_hierarchy(&rev, &mut report_rev);

        let too_deep_ids = |records: &[IndexRecord], h: &Hierarchy| -> BTreeSet<String> {
            records
                .iter()
                .enumerate()
                .filter(|(i, _)| h.get(*i).is_none())
                .map(|(_, r)| r.id.clone())
                .collect()
        };

        assert_eq!(too_deep_ids(&recs, &h_fwd), too_deep_ids(&rev, &h_rev));
        assert_eq!(report_fwd.count("too_deep"), 8);
        assert_eq!(report_rev.count("too_deep"), 8);
    }

    #[test]
    fn duplicate_ids_are_reported_and_first_wins() {
        let recs = vec![
            rec("dup", None, "admin-unit", Some("A")),
            rec("dup", None, "facility", None),
            rec("child", Some("dup"), "facility", None),
        ];
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        assert_eq!(report.count("duplicate_id"), 1);
        let child = idx(&recs, "child");
        assert_eq!(h.admin_codes(&recs, child)[0].as_deref(), Some("A"));
        // The second "dup" record (index 1) is the duplicate and has no info.
        assert!(h.get(1).is_none());
    }

    #[test]
    fn diamond_shares_a_prefix() {
        let recs = vec![
            rec("root", None, "admin-unit", Some("ROOT")),
            rec("a", Some("root"), "admin-unit", Some("A")),
            rec("b", Some("a"), "admin-unit", Some("B")),
            rec("leaf1", Some("b"), "facility", None),
            rec("c", Some("b"), "admin-unit", Some("C")),
            rec("leaf2", Some("c"), "facility", None),
        ];
        let mut report = Report::default();
        let h = resolve_hierarchy(&recs, &mut report);

        let leaf1 = idx(&recs, "leaf1");
        let leaf2 = idx(&recs, "leaf2");
        assert_eq!(h.get(leaf1).unwrap().depth, 3);
        assert_eq!(h.path(&recs, leaf1), "/root/a/b/leaf1");
        assert_eq!(h.get(leaf2).unwrap().depth, 4);
        assert_eq!(h.path(&recs, leaf2), "/root/a/b/c/leaf2");
        assert!(report.counts().is_empty());
    }

    #[test]
    fn size_of_hierarchy_info_is_small() {
        assert!(std::mem::size_of::<HierarchyInfo>() <= 40);
    }

    #[test]
    fn long_chain_and_long_cycle_report_quickly() {
        use std::time::Instant;

        // Long linear chain: n0..n19999, far past MAX_DEPTH. Reporting
        // every too_deep node used to reconstruct its full root..node
        // chain (O(n) per node, O(n^2) total); DETAIL_CAP bounds that.
        let mut chain = vec![rec("n0", None, "admin-unit", Some("X"))];
        for i in 1..20_000 {
            chain.push(rec(
                &format!("n{i}"),
                Some(&format!("n{}", i - 1)),
                "admin-unit",
                None,
            ));
        }
        let mut report = Report::default();
        let start = Instant::now();
        resolve_hierarchy(&chain, &mut report);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs() < 1, "too_deep reporting took {elapsed:?}");
        assert_eq!(report.count("too_deep"), 19_988);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.kind == "too_deep" && i.detail.contains("... ->")),
            "expected a truncated too_deep detail"
        );

        // Long cycle: c0 -> c1 -> ... -> c19999 -> c0. The naive full
        // rotation per member is O(cycle_len) each, O(cycle_len^2) total;
        // DETAIL_CAP bounds that too.
        let cyc: Vec<IndexRecord> = (0..20_000)
            .map(|i| {
                let parent = format!("c{}", (i + 1) % 20_000);
                rec(&format!("c{i}"), Some(&parent), "admin-unit", None)
            })
            .collect();
        let mut report = Report::default();
        let start = Instant::now();
        resolve_hierarchy(&cyc, &mut report);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs() < 1, "cycle reporting took {elapsed:?}");
        assert_eq!(report.count("cycle"), 20_000);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.kind == "cycle" && i.detail.contains("(cycle of 20000 nodes)")),
            "expected a cycle-count-capped detail"
        );
    }
}
