#![cfg(test)]
use std::collections::BTreeMap;

/// Build a kind plan map from list of (path, kind) pairs.
/// Example: mk_kind_plan(&[(vec![], 0), (vec![0], 1)])
pub(crate) fn mk_kind_plan(entries: &[(Vec<u16>, u8)]) -> BTreeMap<Vec<u16>, u8> {
    let mut plan = BTreeMap::new();
    for (path, kind) in entries.iter().cloned() {
        plan.insert(path, kind);
    }
    plan
}
