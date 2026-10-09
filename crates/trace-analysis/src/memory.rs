//! Which memory cells each load reads and each store writes, read off the
//! converged points-to sets before they are released (`docs/ANALYSIS.md`,
//! "Memory access edges").

use rustc_hash::{FxHashMap, FxHashSet};
use trace_ir::{LocId, PagNodeId};

use crate::constraints::{ConstraintId, ConstraintKind, LocKind};
use crate::pag::Pag;

/// The most cells one access keeps. Past it, instance field cells fold into
/// their field summaries; an access still wider is recorded without cells:
/// its pointer's points-to set is too coarse to say which memory it touches.
pub const MEMORY_ACCESS_CAP: usize = 16;

/// One memory cell a `Load` reads or a `Store` writes. `constraint` is the
/// load or store in `Pag::constraints`; its kind says which. `loc` is `None` for an access
/// whose cells are not recorded: past [`MEMORY_ACCESS_CAP`], or a pointer
/// that points at no memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryAccess {
    pub constraint: ConstraintId,
    pub loc: Option<LocId>,
}

/// The cells of every load and store, in constraint order and by location
/// within one constraint; one cell-less access for a load or store whose
/// cells are not recorded.
///
/// A cell is a location the access's pointer points to, less function and
/// string-literal locations (a load through a function pointer yields the
/// function itself, not memory). A field access's pointer (a `Gep`
/// destination) also holds the contents of the cells it designates, so the
/// solver's later uses of the field lvalue see them; its cells are instead
/// the ones the solver's `Gep` step designated (`designated`): the field's
/// cell in each object its base points to, the field's summary, and the
/// summary the solver falls back to for the base variable's declared type. A
/// store into a field cell also writes the field's summary, as the solver
/// does. Past [`MEMORY_ACCESS_CAP`] cells an instance cell is replaced by its
/// summary, which every store into the cell also writes; an access still past
/// the cap keeps no cell.
pub(crate) fn memory_accesses(
    pag: &Pag,
    pts: &FxHashMap<PagNodeId, FxHashSet<LocId>>,
    designated: &FxHashMap<PagNodeId, FxHashSet<LocId>>,
) -> Vec<MemoryAccess> {
    let field_pointers: FxHashSet<PagNodeId> = pag
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::Gep && c.field().is_some())
        .map(|c| c.dst)
        .collect();
    let mut out = Vec::new();
    let mut cells: Vec<LocId> = Vec::new();
    for (index, c) in pag.constraints.iter().enumerate() {
        let pointer = match c.kind {
            ConstraintKind::Load => c.src,
            ConstraintKind::Store => c.dst,
            _ => continue,
        };
        let constraint = ConstraintId(u32::try_from(index).expect("constraint index fits u32"));
        // The solver points a field pointer only at cells it designates, so
        // its designated cells are a subset of its points-to.
        let targets = if field_pointers.contains(&pointer) {
            designated.get(&pointer)
        } else {
            pts.get(&pointer)
        };
        cells.clear();
        let fits = collect_cells(pag, c.kind, targets.into_iter().flatten(), &mut cells);
        if !fits || cells.is_empty() {
            out.push(MemoryAccess {
                constraint,
                loc: None,
            });
            continue;
        }
        out.extend(cells.iter().map(|&loc| MemoryAccess {
            constraint,
            loc: Some(loc),
        }));
    }
    out
}

/// The cells of one access whose pointer points to `targets`, sorted, into
/// `cells`; false when they do not fit [`MEMORY_ACCESS_CAP`].
fn collect_cells<'a>(
    pag: &Pag,
    kind: ConstraintKind,
    targets: impl Iterator<Item = &'a LocId>,
    cells: &mut Vec<LocId>,
) -> bool {
    // Distinct cells folding cannot remove: every one but an instance field
    // cell. Past the cap the access keeps no cell whatever folds.
    let mut unfoldable = 0usize;
    for &loc in targets {
        let loc_kind = pag.locations[loc.0 as usize].kind;
        if matches!(loc_kind, LocKind::Function | LocKind::StringLit) {
            continue;
        }
        cells.push(loc);
        if loc_kind == LocKind::Field {
            if kind == ConstraintKind::Store {
                cells.extend(pag.summary_for_field_loc(loc));
            }
        } else {
            unfoldable += 1;
            if unfoldable > MEMORY_ACCESS_CAP {
                return false;
            }
        }
    }
    cells.sort_unstable();
    cells.dedup();
    if cells.len() > MEMORY_ACCESS_CAP {
        for cell in cells.iter_mut() {
            if pag.locations[cell.0 as usize].kind == LocKind::Field {
                if let Some(summary) = pag.summary_for_field_loc(*cell) {
                    *cell = summary;
                }
            }
        }
        cells.sort_unstable();
        cells.dedup();
        if cells.len() > MEMORY_ACCESS_CAP {
            return false;
        }
    }
    true
}
