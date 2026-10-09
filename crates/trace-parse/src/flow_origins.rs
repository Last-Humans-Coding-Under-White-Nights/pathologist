//! Append-only origin membership without a second copy of source expressions.

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;
use std::hash::BuildHasher;
use std::sync::Arc;
use trace_ir::Span;

/// Membership indexes refer to the caller's ordered vector. That vector is
/// the source of truth and must only be appended to while this index is alive.
#[derive(Default)]
pub(crate) struct OriginMembership {
    indices: HashTable<usize>,
}

fn hash(span: Span, expression: &str) -> u64 {
    FxBuildHasher.hash_one((span, expression))
}

/// Handle the common zero/singleton list without allocating a membership table,
/// returning the appended or existing origin's stable index.
pub(crate) fn insert_small(
    origins: &mut Vec<(Span, Arc<str>)>,
    span: Span,
    expression: &Arc<str>,
) -> usize {
    debug_assert!(origins.len() < 2);
    if origins
        .first()
        .is_some_and(|(s, e)| *s == span && e == expression)
    {
        return 0;
    }
    // Vec::push starts at four entries; almost every origin list stays at one.
    if origins.is_empty() {
        origins.reserve_exact(1);
    }
    origins.push((span, Arc::clone(expression)));
    origins.len() - 1
}

impl OriginMembership {
    pub(crate) fn new(origins: &[(Span, Arc<str>)]) -> Self {
        let mut result = Self::default();
        if origins.len() > 1 {
            result.index_existing(origins);
        }
        result
    }

    fn index_existing(&mut self, origins: &[(Span, Arc<str>)]) {
        self.indices
            .reserve(origins.len(), |&i| hash(origins[i].0, &origins[i].1));
        for (i, (span, expression)) in origins.iter().enumerate() {
            self.indices
                .insert_unique(hash(*span, expression), i, |&i| {
                    hash(origins[i].0, &origins[i].1)
                });
        }
    }

    /// Return the existing or appended index without copying duplicate text.
    pub(crate) fn insert(
        &mut self,
        origins: &mut Vec<(Span, Arc<str>)>,
        span: Span,
        expression: &Arc<str>,
    ) -> usize {
        if origins.len() < 2 {
            return insert_small(origins, span, expression);
        }
        if self.indices.is_empty() {
            self.index_existing(origins);
        }
        let h = hash(span, expression);
        if let Some(&index) = self
            .indices
            .find(h, |&i| origins[i].0 == span && origins[i].1 == *expression)
        {
            return index;
        }
        self.indices
            .insert_unique(h, origins.len(), |&i| hash(origins[i].0, &origins[i].1));
        origins.push((span, Arc::clone(expression)));
        origins.len() - 1
    }
}
