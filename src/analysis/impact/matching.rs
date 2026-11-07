//! Conservative cross-snapshot matching. Structural identity and inferred candidates stay distinct.
use super::{
    DeclarationChange,
    baseline::{Catalog, Declaration},
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Pair {
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub basis: &'static str,
    pub uncertain: bool,
}

pub(super) fn pair(before: &Catalog, after: &Catalog) -> Vec<Pair> {
    let key = |d: &Declaration| {
        (
            d.location.path.clone(),
            d.location.language.clone(),
            d.location.kind.clone(),
            d.location.symbol.clone(),
        )
    };
    let mut old_keys: BTreeMap<_, Vec<i64>> = BTreeMap::new();
    let mut new_keys: BTreeMap<_, Vec<i64>> = BTreeMap::new();
    for (id, d) in &before.declarations {
        if d.location.symbol.is_some() {
            old_keys.entry(key(d)).or_default().push(*id);
        }
    }
    for (id, d) in &after.declarations {
        if d.location.symbol.is_some() {
            new_keys.entry(key(d)).or_default().push(*id);
        }
    }
    let mut used_old = BTreeSet::new();
    let mut used_new = BTreeSet::new();
    let mut result = Vec::new();
    for (key, old) in old_keys {
        let Some(new) = new_keys.get(&key) else {
            continue;
        };
        if old.len() == 1 && new.len() == 1 {
            used_old.insert(old[0]);
            used_new.insert(new[0]);
            result.push(Pair {
                before: Some(old[0]),
                after: Some(new[0]),
                basis: "unique_path_kind_symbol",
                uncertain: false,
            });
            continue;
        }
        // Overloads and repeated declarations can retain an unambiguous textual signature.
        let mut old_signatures: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
        let mut new_signatures: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
        for id in old {
            old_signatures
                .entry(&before.declarations[&id].signature_hash)
                .or_default()
                .push(id);
        }
        for id in new {
            new_signatures
                .entry(&after.declarations[id].signature_hash)
                .or_default()
                .push(*id);
        }
        for (signature, old) in old_signatures {
            if let Some(new) = new_signatures.get(signature)
                && old.len() == 1
                && new.len() == 1
            {
                used_old.insert(old[0]);
                used_new.insert(new[0]);
                result.push(Pair {
                    before: Some(old[0]),
                    after: Some(new[0]),
                    basis: "unique_path_kind_symbol_signature",
                    uncertain: false,
                });
            }
        }
    }
    let shape = |d: &Declaration| {
        d.shape_hash.as_ref().map(|h| {
            (
                d.location.language.clone(),
                d.location.kind.clone(),
                h.clone(),
            )
        })
    };
    let mut old_shapes: BTreeMap<_, Vec<i64>> = BTreeMap::new();
    let mut new_shapes: BTreeMap<_, Vec<i64>> = BTreeMap::new();
    for (id, d) in &before.declarations {
        if !used_old.contains(id)
            && let Some(shape) = shape(d)
        {
            old_shapes.entry(shape).or_default().push(*id);
        }
    }
    for (id, d) in &after.declarations {
        if !used_new.contains(id)
            && let Some(shape) = shape(d)
        {
            new_shapes.entry(shape).or_default().push(*id);
        }
    }
    for (shape, old) in old_shapes {
        if let Some(new) = new_shapes.get(&shape)
            && old.len() == 1
            && new.len() == 1
        {
            used_old.insert(old[0]);
            used_new.insert(new[0]);
            result.push(Pair {
                before: Some(old[0]),
                after: Some(new[0]),
                basis: "unique_source_with_own_name_masked",
                uncertain: true,
            });
        }
    }
    for id in before
        .declarations
        .keys()
        .filter(|id| !used_old.contains(id))
    {
        result.push(Pair {
            before: Some(*id),
            after: None,
            basis: "unmatched",
            uncertain: true,
        });
    }
    for id in after
        .declarations
        .keys()
        .filter(|id| !used_new.contains(id))
    {
        result.push(Pair {
            before: None,
            after: Some(*id),
            basis: "unmatched",
            uncertain: true,
        });
    }
    result
}

impl Pair {
    pub fn report(&self, before: &Catalog, after: &Catalog) -> DeclarationChange {
        let old = self.before.map(|id| &before.declarations[&id]);
        let new = self.after.map(|id| &after.declarations[&id]);
        let status = match (old, new) {
            (Some(a), Some(b))
                if self.uncertain
                    && a.location.path == b.location.path
                    && a.location.symbol == b.location.symbol =>
            {
                "uncertain_match"
            }
            (Some(_), Some(_)) if self.uncertain => "possible_rename_or_move",
            (Some(a), Some(b))
                if a.source_hash == b.source_hash && a.signature_hash == b.signature_hash =>
            {
                "unchanged"
            }
            (Some(_), Some(_)) => "modified",
            (Some(a), None) if after.invalid_files.contains(&a.location.path) => {
                "after_parse_unavailable"
            }
            (None, Some(b)) if before.invalid_files.contains(&b.location.path) => {
                "before_parse_unavailable"
            }
            (Some(_), None) => "removed",
            (None, Some(_)) => "added",
            (None, None) => unreachable!(),
        };
        DeclarationChange {
            before: old.map(|d| d.location.clone()),
            after: new.map(|d| d.location.clone()),
            status: status.into(),
            match_basis: self.basis.into(),
            uncertain: self.uncertain,
            signature_changed: old
                .zip(new)
                .map(|(a, b)| a.signature_hash != b.signature_hash),
            source_changed: old.zip(new).map(|(a, b)| a.source_hash != b.source_hash),
        }
    }
}
