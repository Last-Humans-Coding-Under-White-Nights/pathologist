//! Per-function call-order control-flow graphs (`--sequencing`).
//!
//! Semantics, merge rules and limits: `docs/ANALYSIS.md`, "Call sequencing
//! (control-flow sidecar)". Nothing here feeds the pointer analysis.

use crate::{CallOccurrence, CallSiteId, FileId, FnId, Span};

/// Block index into [`FunctionCfg::blocks`]; local to one graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// Event index into [`FunctionCfg::events`]; local to one graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Entry,
    Exit,
    ExceptionalExit,
    Ordinary,
    /// Where a `try` body's exception edges meet before its handlers.
    Landing,
}

impl BlockKind {
    pub const ALL: [Self; 5] = [
        Self::Entry,
        Self::Exit,
        Self::ExceptionalExit,
        Self::Ordinary,
        Self::Landing,
    ];

    /// The kind's name in exports and query output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Exit => "exit",
            Self::ExceptionalExit => "exceptional_exit",
            Self::Ordinary => "ordinary",
            Self::Landing => "landing",
        }
    }

    /// The kind [`Self::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Fallthrough,
    BranchTrue,
    BranchFalse,
    SwitchCase,
    BackEdge,
    Return,
    Exception,
    /// Control leaves through a construct the graph does not model.
    Unsupported,
}

impl EdgeKind {
    pub const ALL: [Self; 8] = [
        Self::Fallthrough,
        Self::BranchTrue,
        Self::BranchFalse,
        Self::SwitchCase,
        Self::BackEdge,
        Self::Return,
        Self::Exception,
        Self::Unsupported,
    ];

    /// The kind's name in exports and query output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fallthrough => "fallthrough",
            Self::BranchTrue => "branch_true",
            Self::BranchFalse => "branch_false",
            Self::SwitchCase => "switch_case",
            Self::BackEdge => "back_edge",
            Self::Return => "return",
            Self::Exception => "exception",
            Self::Unsupported => "unsupported",
        }
    }

    /// The kind [`Self::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Call,
    Construct,
    Destroy,
    Delete,
}

impl EventKind {
    pub const ALL: [Self; 4] = [Self::Call, Self::Construct, Self::Destroy, Self::Delete];

    /// The kind's name in exports and query output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Construct => "construct",
            Self::Destroy => "destroy",
            Self::Delete => "delete",
        }
    }

    /// The kind [`Self::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GapKind {
    ParseError,
    DepthExceeded,
    UnsupportedConstruct,
    UnresolvedGoto,
    UnorderedBranch,
    InitializerOrder,
    /// Events whose sites the merge did not keep were spliced out.
    OmittedSites,
}

impl GapKind {
    pub const ALL: [Self; 7] = [
        Self::ParseError,
        Self::DepthExceeded,
        Self::UnsupportedConstruct,
        Self::UnresolvedGoto,
        Self::UnorderedBranch,
        Self::InitializerOrder,
        Self::OmittedSites,
    ];

    /// The kind's name in exports and query output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::DepthExceeded => "depth_exceeded",
            Self::UnsupportedConstruct => "unsupported_construct",
            Self::UnresolvedGoto => "unresolved_goto",
            Self::UnorderedBranch => "unordered_branch",
            Self::InitializerOrder => "initializer_order",
            Self::OmittedSites => "omitted_sites",
        }
    }

    /// The kind [`Self::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Block {
    pub kind: BlockKind,
    pub span: Option<Span>,
    /// The block's events in evaluation order.
    pub events: Vec<EventId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CfgEdge {
    pub from: BlockId,
    pub to: BlockId,
    pub kind: EdgeKind,
}

/// One syntactic invocation; `sites` are the alternative-target records of
/// that one invocation, never a sequence of calls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallEvent {
    pub block: BlockId,
    pub kind: EventKind,
    pub occurrence: CallOccurrence,
    pub sites: Vec<CallSiteId>,
}

/// A region whose order is not modeled; `events` is an inclusive range.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Gap {
    pub kind: GapKind,
    pub span: Option<Span>,
    pub events: Option<(EventId, EventId)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCfg {
    pub function: FnId,
    /// Unit the body was lowered in, recorded by merge.
    pub tu: Option<FileId>,
    /// Position among the function's configuration variants, set by merge.
    pub variant: u32,
    pub entry: BlockId,
    pub exit: BlockId,
    pub exceptional_exit: Option<BlockId>,
    pub blocks: Vec<Block>,
    pub edges: Vec<CfgEdge>,
    pub events: Vec<CallEvent>,
    /// Immediate sequenced-before pairs between events of one block.
    pub order: Vec<(EventId, EventId)>,
    pub gaps: Vec<Gap>,
}

/// Keep `cfg`, in another program's ids (see [`FunctionCfg::remap`]), as
/// one more configuration variant of `function` unless `variants` already
/// holds a structurally equal graph: the first copy of a body is kept, and
/// one a unit or configuration compiles differently is another variant,
/// numbered by position and never unioned. Returns the variant added.
pub fn add_variant<'v>(
    variants: &'v mut Vec<FunctionCfg>,
    cfg: &FunctionCfg,
    function: FnId,
    map_site: impl Fn(CallSiteId) -> Option<CallSiteId>,
    map_file: impl Fn(FileId) -> FileId,
) -> Option<&'v mut FunctionCfg> {
    let mut kept = if cfg.splices(&map_site) {
        // The remapped graph is built anyway; compare it, built once.
        let kept = cfg.remap(function, map_site, map_file);
        if variants.iter().any(|v| v.structurally_equal(&kept)) {
            return None;
        }
        kept
    } else {
        if variants
            .iter()
            .any(|v| v.equal_unspliced(cfg, function, &map_site, &map_file))
        {
            return None;
        }
        cfg.remap(function, map_site, map_file)
    };
    kept.variant = variants.len() as u32;
    variants.push(kept);
    variants.last_mut()
}

/// `sites` through `map_site`, dropping those it does not map; coalesced
/// records of one invocation name one merged site.
fn mapped_sites<'s>(
    sites: &'s [CallSiteId],
    map_site: &'s impl Fn(CallSiteId) -> Option<CallSiteId>,
) -> impl Iterator<Item = CallSiteId> + 's {
    sites.iter().enumerate().filter_map(move |(i, &s)| {
        let site = map_site(s)?;
        let repeat = sites[..i].iter().any(|&p| map_site(p) == Some(site));
        (!repeat).then_some(site)
    })
}

/// `order` with each event `renumber` drops (`None`) spliced out in
/// ascending id order: its predecessors are joined to its successors, so
/// the events around it keep their order. A pair is never added twice;
/// added pairs follow the kept ones, in the order the splices add them.
fn splice_order(
    order: &[(EventId, EventId)],
    renumber: &[Option<EventId>],
) -> Vec<(EventId, EventId)> {
    // Pairs in insertion order, `None` once removed.
    let mut pairs: Vec<Option<(EventId, EventId)>> = order.iter().copied().map(Some).collect();
    // Indices into `pairs` of each event's outgoing and incoming pairs, in
    // insertion order; removed pairs are skipped as they are met.
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); renumber.len()];
    let mut incoming: Vec<Vec<usize>> = vec![Vec::new(); renumber.len()];
    // The pairs not removed yet. Removing an event removes every copy of
    // its pairs, so a pair is live while any copy of it is.
    let mut live: rustc_hash::FxHashSet<(EventId, EventId)> = Default::default();
    for (i, &(a, b)) in order.iter().enumerate() {
        outgoing[a.0 as usize].push(i);
        incoming[b.0 as usize].push(i);
        live.insert((a, b));
    }
    let mut preds = Vec::new();
    let mut succs = Vec::new();
    for (old, new) in renumber.iter().enumerate() {
        if new.is_some() {
            continue;
        }
        preds.clear();
        succs.clear();
        preds.extend(incoming[old].iter().filter_map(|&i| pairs[i]).map(|p| p.0));
        succs.extend(outgoing[old].iter().filter_map(|&i| pairs[i]).map(|p| p.1));
        for &i in incoming[old].iter().chain(&outgoing[old]) {
            if let Some(pair) = pairs[i].take() {
                live.remove(&pair);
            }
        }
        incoming[old].clear();
        outgoing[old].clear();
        for &p in &preds {
            for &s in &succs {
                let pair = (p, s);
                // One still touching the spliced event (from a pair of the
                // event with itself) is dropped by the renumbering.
                if live.insert(pair) {
                    let i = pairs.len();
                    pairs.push(Some(pair));
                    outgoing[pair.0 .0 as usize].push(i);
                    incoming[pair.1 .0 as usize].push(i);
                }
            }
        }
    }
    pairs.into_iter().flatten().collect()
}

/// Drop repeated edges, keeping each edge's first position.
fn dedup_edges(edges: &mut Vec<CfgEdge>) {
    let mut seen = rustc_hash::FxHashSet::default();
    edges.retain(|e| seen.insert(*e));
}

impl FunctionCfg {
    /// Same body structure, whichever unit or variant slot it came from.
    pub fn structurally_equal(&self, other: &Self) -> bool {
        // Exhaustive, so a new field is a decision here.
        let FunctionCfg {
            function,
            tu: _,
            variant: _,
            entry,
            exit,
            exceptional_exit,
            blocks,
            edges,
            events,
            order,
            gaps,
        } = self;
        *function == other.function
            && *entry == other.entry
            && *exit == other.exit
            && *exceptional_exit == other.exceptional_exit
            && *blocks == other.blocks
            && *edges == other.edges
            && *events == other.events
            && *order == other.order
            && *gaps == other.gaps
    }

    /// A hash of what [`Self::structurally_equal`] compares: structurally
    /// equal graphs hash alike. Lets a caller keep a fingerprint per graph
    /// instead of a second copy to compare against.
    pub fn structural_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        // Exhaustive, as in `structurally_equal`.
        let FunctionCfg {
            function,
            tu: _,
            variant: _,
            entry,
            exit,
            exceptional_exit,
            blocks,
            edges,
            events,
            order,
            gaps,
        } = self;
        let mut hasher = rustc_hash::FxHasher::default();
        (function, entry, exit, exceptional_exit).hash(&mut hasher);
        (blocks, edges, events, order, gaps).hash(&mut hasher);
        hasher.finish()
    }

    /// Every file the graph names: its unit, and the files of its block,
    /// event and gap positions.
    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        let blocks = self.blocks.iter().filter_map(|b| b.span);
        let events = self
            .events
            .iter()
            .flat_map(|e| std::iter::once(e.occurrence.span).chain(e.occurrence.expansion_span));
        let gaps = self.gaps.iter().filter_map(|g| g.span);
        let spans = blocks.chain(events).chain(gaps).map(|s| s.file);
        self.tu.into_iter().chain(spans)
    }

    /// `self.structurally_equal(&other.remap(function, map_site, map_file))`,
    /// without building the remapped graph unless one of `other`'s events
    /// loses every site.
    pub fn equal_after_remap(
        &self,
        other: &FunctionCfg,
        function: FnId,
        map_site: impl Fn(CallSiteId) -> Option<CallSiteId>,
        map_file: impl Fn(FileId) -> FileId,
    ) -> bool {
        if other.splices(&map_site) {
            return self.structurally_equal(&other.remap(function, map_site, map_file));
        }
        self.equal_unspliced(other, function, map_site, map_file)
    }

    /// Whether [`Self::remap`] through `map_site` splices out an event.
    fn splices(&self, map_site: &impl Fn(CallSiteId) -> Option<CallSiteId>) -> bool {
        self.events
            .iter()
            .any(|e| mapped_sites(&e.sites, map_site).next().is_none())
    }

    /// [`Self::equal_after_remap`] when `map_site` splices out no event of
    /// `other`: ids, the order and gap ranges carry over.
    fn equal_unspliced(
        &self,
        other: &FunctionCfg,
        function: FnId,
        map_site: impl Fn(CallSiteId) -> Option<CallSiteId>,
        map_file: impl Fn(FileId) -> FileId,
    ) -> bool {
        let FunctionCfg {
            function: _,
            tu: _,
            variant: _,
            entry,
            exit,
            exceptional_exit,
            blocks,
            edges,
            events,
            order,
            gaps,
        } = other;
        let map_span = |span: Span| Span {
            file: map_file(span.file),
            ..span
        };
        self.function == function
            && self.entry == *entry
            && self.exit == *exit
            && self.exceptional_exit == *exceptional_exit
            && self.edges == *edges
            && self.order == *order
            && self.blocks.len() == blocks.len()
            && self.blocks.iter().zip(blocks).all(|(a, b)| {
                a.kind == b.kind && a.span == b.span.map(map_span) && a.events == b.events
            })
            && self.events.len() == events.len()
            && self.events.iter().zip(events).all(|(a, b)| {
                let mut occurrence = b.occurrence;
                occurrence.span = map_span(occurrence.span);
                occurrence.expansion_span = occurrence.expansion_span.map(map_span);
                a.block == b.block
                    && a.kind == b.kind
                    && a.occurrence == occurrence
                    && a.sites
                        .iter()
                        .copied()
                        .eq(mapped_sites(&b.sites, &map_site))
            })
            && self.gaps.len() == gaps.len()
            && self.gaps.iter().zip(gaps).all(|(a, b)| {
                a.kind == b.kind
                    && a.span == b.span.map(map_span)
                    // A reversed range keeps no event.
                    && a.events == b.events.filter(|(first, last)| first <= last)
            })
    }

    /// The graph in another program's ids: `function` for its function,
    /// sites through `map_site`, files (`tu` too) through `map_file`. An
    /// event left with no site is spliced out of its block and the local
    /// order, and an `omitted_sites` gap marks where it was
    /// (docs/ANALYSIS.md, "Call sequencing"). `variant` is kept.
    pub fn remap(
        &self,
        function: FnId,
        map_site: impl Fn(CallSiteId) -> Option<CallSiteId>,
        map_file: impl Fn(FileId) -> FileId,
    ) -> FunctionCfg {
        let map_span = |span: Span| Span {
            file: map_file(span.file),
            ..span
        };
        let mut events = Vec::with_capacity(self.events.len());
        // Old event index -> new id, `None` once spliced out.
        let mut renumber: Vec<Option<EventId>> = Vec::with_capacity(self.events.len());
        let mut omitted = Vec::new();
        for event in &self.events {
            let sites: Vec<CallSiteId> = mapped_sites(&event.sites, &map_site).collect();
            let mut occurrence = event.occurrence;
            occurrence.span = map_span(occurrence.span);
            occurrence.expansion_span = occurrence.expansion_span.map(map_span);
            if sites.is_empty() {
                renumber.push(None);
                omitted.push(occurrence.span);
                continue;
            }
            renumber.push(Some(EventId(events.len() as u32)));
            events.push(CallEvent {
                block: event.block,
                kind: event.kind,
                occurrence,
                sites,
            });
        }
        let order = if omitted.is_empty() {
            self.order.clone()
        } else {
            splice_order(&self.order, &renumber)
        };
        let at = |e: EventId| renumber[e.0 as usize];
        let order = order
            .into_iter()
            .filter_map(|(a, b)| Some((at(a)?, at(b)?)))
            .collect();
        let blocks = self
            .blocks
            .iter()
            .map(|b| Block {
                kind: b.kind,
                span: b.span.map(map_span),
                events: b.events.iter().filter_map(|&e| at(e)).collect(),
            })
            .collect();
        let mut gaps: Vec<Gap> = self
            .gaps
            .iter()
            .map(|g| Gap {
                kind: g.kind,
                span: g.span.map(map_span),
                // The range shrinks to the events still in it.
                events: g.events.and_then(|(first, last)| {
                    let kept = || (first.0..=last.0).filter_map(|e| at(EventId(e)));
                    Some((kept().next()?, kept().next_back()?))
                }),
            })
            .collect();
        gaps.extend(omitted.into_iter().map(|span| Gap {
            kind: GapKind::OmittedSites,
            span: Some(span),
            events: None,
        }));
        FunctionCfg {
            function,
            tu: self.tu.map(&map_file),
            variant: self.variant,
            entry: self.entry,
            exit: self.exit,
            exceptional_exit: self.exceptional_exit,
            blocks,
            edges: self.edges.clone(),
            events,
            order,
            gaps,
        }
    }

    /// Remove event-free ordinary blocks that only fall through to another
    /// block, redirecting their incoming edges; branch, loop, exit and
    /// landing structure is kept. Repeated edges are dropped either way.
    pub fn compact(&mut self) {
        let n = self.blocks.len();
        let mut out_degree = vec![0u32; n];
        for e in &self.edges {
            out_degree[e.from.0 as usize] += 1;
        }
        // The one successor of each removable block.
        let mut forward: Vec<Option<BlockId>> = vec![None; n];
        for e in &self.edges {
            let b = e.from.0 as usize;
            if e.kind == EdgeKind::Fallthrough
                && e.to != e.from
                && out_degree[b] == 1
                && self.blocks[b].kind == BlockKind::Ordinary
                && self.blocks[b].events.is_empty()
            {
                forward[b] = Some(e.to);
            }
        }
        if forward.iter().all(Option::is_none) {
            // Nothing to remove, but duplicates still go.
            dedup_edges(&mut self.edges);
            return;
        }
        // Resolve each chain to the block it ends at. A cycle of removable
        // blocks keeps the block the walk met it at.
        const UNRESOLVED: u32 = u32::MAX;
        const ON_PATH: u32 = u32::MAX - 1;
        let mut target = vec![UNRESOLVED; n];
        let mut path = Vec::new();
        for start in 0..n {
            let mut b = start;
            loop {
                match target[b] {
                    UNRESOLVED => {}
                    ON_PATH => {
                        forward[b] = None;
                        target[b] = b as u32;
                        break;
                    }
                    _ => break,
                }
                let Some(next) = forward[b] else {
                    target[b] = b as u32;
                    break;
                };
                target[b] = ON_PATH;
                path.push(b);
                b = next.0 as usize;
            }
            let end = target[b];
            for p in path.drain(..) {
                target[p] = end;
            }
        }
        let mut renumber = vec![UNRESOLVED; n];
        let mut kept = 0u32;
        for (b, slot) in renumber.iter_mut().enumerate() {
            if forward[b].is_none() {
                *slot = kept;
                kept += 1;
            }
        }
        let map = |b: BlockId| BlockId(renumber[target[b.0 as usize] as usize]);
        self.edges.retain(|e| forward[e.from.0 as usize].is_none());
        for e in &mut self.edges {
            e.from = map(e.from);
            e.to = map(e.to);
        }
        dedup_edges(&mut self.edges);
        let mut kept = forward.iter().map(Option::is_none);
        self.blocks.retain(|_| kept.next() == Some(true));
        for event in &mut self.events {
            event.block = map(event.block);
        }
        self.entry = map(self.entry);
        self.exit = map(self.exit);
        self.exceptional_exit = self.exceptional_exit.map(map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(line: u32) -> Span {
        Span::new(FileId(0), line, 1)
    }

    fn block(kind: BlockKind, events: &[u32]) -> Block {
        Block {
            kind,
            span: None,
            events: events.iter().map(|&e| EventId(e)).collect(),
        }
    }

    fn edge(from: u32, to: u32, kind: EdgeKind) -> CfgEdge {
        CfgEdge {
            from: BlockId(from),
            to: BlockId(to),
            kind,
        }
    }

    fn event(block: u32, line: u32, site: u32) -> CallEvent {
        CallEvent {
            block: BlockId(block),
            kind: EventKind::Call,
            occurrence: CallOccurrence {
                span: span(line),
                expansion_span: None,
                expansion_id: 0,
            },
            sites: vec![CallSiteId(site)],
        }
    }

    /// `blocks[0]` is the entry and `blocks[1]` the exit.
    fn cfg(blocks: Vec<Block>, edges: Vec<CfgEdge>, events: Vec<CallEvent>) -> FunctionCfg {
        FunctionCfg {
            function: FnId(7),
            tu: None,
            variant: 0,
            entry: BlockId(0),
            exit: BlockId(1),
            exceptional_exit: None,
            blocks,
            edges,
            events,
            order: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn straight() -> FunctionCfg {
        cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[0, 1]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Fallthrough),
            ],
            vec![event(2, 3, 10), event(2, 4, 11)],
        )
    }

    #[test]
    fn structural_equality_ignores_unit_and_variant() {
        let a = straight();
        let mut b = straight();
        b.tu = Some(FileId(3));
        b.variant = 2;
        assert!(a.structurally_equal(&b));
    }

    #[test]
    fn structural_hash_follows_structural_equality() {
        let a = straight();
        let mut b = straight();
        b.tu = Some(FileId(3));
        b.variant = 2;
        assert_eq!(a.structural_hash(), b.structural_hash());
        let mut sites = straight();
        sites.events[1].sites = vec![CallSiteId(12)];
        assert_ne!(a.structural_hash(), sites.structural_hash());
    }

    #[test]
    fn files_lists_every_position() {
        let mut g = straight();
        g.tu = Some(FileId(1));
        g.blocks[2].span = Some(Span::new(FileId(2), 1, 1));
        g.events[0].occurrence.expansion_span = Some(Span::new(FileId(3), 1, 1));
        g.gaps.push(Gap {
            kind: GapKind::ParseError,
            span: Some(Span::new(FileId(4), 1, 1)),
            events: None,
        });
        let mut files: Vec<u32> = g.files().map(|f| f.0).collect();
        files.sort_unstable();
        files.dedup();
        assert_eq!(files, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn structural_equality_sees_sites_order_and_gaps() {
        let a = straight();
        let mut sites = straight();
        sites.events[1].sites = vec![CallSiteId(12)];
        assert!(!a.structurally_equal(&sites));
        let mut order = straight();
        order.order.push((EventId(0), EventId(1)));
        assert!(!a.structurally_equal(&order));
        let mut gaps = straight();
        gaps.gaps.push(Gap {
            kind: GapKind::ParseError,
            span: None,
            events: None,
        });
        assert!(!a.structurally_equal(&gaps));
        let mut function = straight();
        function.function = FnId(8);
        assert!(!a.structurally_equal(&function));
    }

    #[test]
    fn compact_removes_event_free_fallthrough_chains() {
        // entry -> b2 -> b3 -> b4(event) -> exit
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[0]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 3, EdgeKind::Fallthrough),
                edge(3, 4, EdgeKind::Fallthrough),
                edge(4, 1, EdgeKind::Fallthrough),
            ],
            vec![event(4, 3, 10)],
        );
        g.compact();
        assert_eq!(g.blocks.len(), 3);
        assert_eq!(g.blocks[2].events, vec![EventId(0)]);
        assert_eq!(g.events[0].block, BlockId(2));
        assert_eq!((g.entry, g.exit), (BlockId(0), BlockId(1)));
        assert_eq!(
            g.edges,
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Fallthrough),
            ]
        );
    }

    #[test]
    fn compact_keeps_branches_and_the_kind_of_a_collapsed_arm() {
        // if (c) {} else x();  -- b2 is the empty condition block with two
        // successors, b3 the empty arm, b4 the else arm, b5 the empty join.
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[0]),
                block(BlockKind::Ordinary, &[]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 3, EdgeKind::BranchTrue),
                edge(2, 4, EdgeKind::BranchFalse),
                edge(3, 5, EdgeKind::Fallthrough),
                edge(4, 5, EdgeKind::Fallthrough),
                edge(5, 1, EdgeKind::Fallthrough),
            ],
            vec![event(4, 3, 10)],
        );
        g.compact();
        // The condition block stays (two successors); the empty arm and the
        // join collapse into the exit.
        assert_eq!(g.blocks.len(), 4);
        assert_eq!(g.events[0].block, BlockId(3));
        assert_eq!(
            g.edges,
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::BranchTrue),
                edge(2, 3, EdgeKind::BranchFalse),
                edge(3, 1, EdgeKind::Fallthrough),
            ]
        );
    }

    #[test]
    fn compact_keeps_exit_and_landing_blocks_and_back_edges() {
        // entry -> b2(empty loop header) -> b3(event) -back-> b2; b2 -> exit;
        // b4 is an empty landing.
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[0]),
                block(BlockKind::Landing, &[]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 3, EdgeKind::BranchTrue),
                edge(3, 2, EdgeKind::BackEdge),
                edge(2, 1, EdgeKind::BranchFalse),
                edge(3, 4, EdgeKind::Exception),
                edge(4, 1, EdgeKind::Fallthrough),
            ],
            vec![event(3, 3, 10)],
        );
        let before = g.clone();
        g.compact();
        assert_eq!(g, before);
    }

    #[test]
    fn compact_terminates_on_an_empty_fallthrough_cycle() {
        // entry -> b2 -> b3 -> b2: `for (;;) {}` with no calls.
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 3, EdgeKind::Fallthrough),
                edge(3, 2, EdgeKind::Fallthrough),
            ],
            Vec::new(),
        );
        g.compact();
        assert_eq!(g.blocks.len(), 3);
        assert_eq!(
            g.edges,
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 2, EdgeKind::Fallthrough),
            ]
        );
    }

    #[test]
    fn compact_drops_edges_made_duplicate() {
        // Two switch cases reaching the exit through empty blocks become one
        // edge.
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[0]),
                block(BlockKind::Ordinary, &[]),
                block(BlockKind::Ordinary, &[]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 3, EdgeKind::SwitchCase),
                edge(2, 4, EdgeKind::SwitchCase),
                edge(3, 1, EdgeKind::Fallthrough),
                edge(4, 1, EdgeKind::Fallthrough),
            ],
            vec![event(2, 3, 10)],
        );
        g.compact();
        assert_eq!(
            g.edges,
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::SwitchCase),
            ]
        );
    }

    #[test]
    fn compact_drops_duplicate_edges_when_no_block_goes() {
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[0]),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Exception),
                edge(2, 1, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Exception),
            ],
            vec![event(2, 3, 10)],
        );
        g.compact();
        assert_eq!(
            g.edges,
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Exception),
                edge(2, 1, EdgeKind::Fallthrough),
            ]
        );
    }

    /// An event whose every site the merge dropped leaves its block and the
    /// local order; its neighbours stay ordered and a gap marks the hole.
    #[test]
    fn remap_splices_events_whose_sites_vanished() {
        let event = |line: u32, sites: &[u32]| CallEvent {
            block: BlockId(2),
            kind: EventKind::Call,
            occurrence: CallOccurrence {
                span: Span::new(FileId(0), line, 5),
                expansion_span: None,
                expansion_id: 0,
            },
            sites: sites.iter().map(|&s| CallSiteId(s)).collect(),
        };
        let block = |kind, events: &[u32]| Block {
            kind,
            span: None,
            events: events.iter().map(|&e| EventId(e)).collect(),
        };
        let cfg = FunctionCfg {
            function: FnId(3),
            tu: None,
            variant: 0,
            entry: BlockId(0),
            exit: BlockId(1),
            exceptional_exit: None,
            blocks: vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &[0, 1, 2]),
            ],
            edges: vec![
                CfgEdge {
                    from: BlockId(0),
                    to: BlockId(2),
                    kind: EdgeKind::Fallthrough,
                },
                CfgEdge {
                    from: BlockId(2),
                    to: BlockId(1),
                    kind: EdgeKind::Fallthrough,
                },
            ],
            // `a(); m(); b();` with the second site of `a` gone too.
            events: vec![event(1, &[10, 11]), event(2, &[12]), event(3, &[13])],
            order: vec![(EventId(0), EventId(1)), (EventId(1), EventId(2))],
            gaps: vec![Gap {
                kind: GapKind::ParseError,
                span: None,
                events: Some((EventId(0), EventId(2))),
            }],
        };
        let call_map: rustc_hash::FxHashMap<CallSiteId, CallSiteId> = [
            (CallSiteId(10), CallSiteId(20)),
            (CallSiteId(13), CallSiteId(23)),
        ]
        .into_iter()
        .collect();
        let file = FileId(9);
        let merged = cfg.remap(FnId(30), |s| call_map.get(&s).copied(), |_| file);
        assert_eq!(merged.function, FnId(30));
        assert_eq!(merged.events.len(), 2);
        assert_eq!(merged.events[0].sites, [CallSiteId(20)]);
        assert_eq!(merged.events[1].sites, [CallSiteId(23)]);
        assert_eq!(merged.events[1].occurrence.span, Span::new(file, 3, 5));
        assert_eq!(merged.blocks[2].events, [EventId(0), EventId(1)]);
        assert_eq!(merged.order, [(EventId(0), EventId(1))]);
        assert_eq!(
            merged.gaps,
            [
                Gap {
                    kind: GapKind::ParseError,
                    span: None,
                    events: Some((EventId(0), EventId(1))),
                },
                Gap {
                    kind: GapKind::OmittedSites,
                    span: Some(Span::new(file, 2, 5)),
                    events: None,
                },
            ]
        );
    }

    #[test]
    fn kind_names_parse_back() {
        for kind in BlockKind::ALL {
            assert_eq!(BlockKind::parse(kind.as_str()), Some(kind));
        }
        for kind in EdgeKind::ALL {
            assert_eq!(EdgeKind::parse(kind.as_str()), Some(kind));
        }
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
        }
        for kind in GapKind::ALL {
            assert_eq!(GapKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(BlockKind::parse("nope"), None);
    }

    #[test]
    fn remap_maps_the_unit() {
        let mut g = straight();
        g.tu = Some(FileId(0));
        let merged = g.remap(FnId(7), Some, |_| FileId(4));
        assert_eq!(merged.tu, Some(FileId(4)));
    }

    /// The comparison without remapping agrees with remapping first, with
    /// and without an event spliced out.
    #[test]
    fn equal_after_remap_matches_remap() {
        let mut g = straight();
        g.blocks[2].span = Some(span(2));
        g.gaps.push(Gap {
            kind: GapKind::UnorderedBranch,
            span: Some(span(3)),
            events: Some((EventId(0), EventId(1))),
        });
        g.events[0].sites.push(CallSiteId(12));
        let file = |_| FileId(5);
        let maps: [&dyn Fn(CallSiteId) -> Option<CallSiteId>; 4] = [
            &Some,
            &|s: CallSiteId| Some(CallSiteId(s.0 + 100)),
            // Both records of the first invocation become one site.
            &|s: CallSiteId| Some(CallSiteId(if s.0 == 12 { 10 } else { s.0 })),
            // The second invocation loses its site.
            &|s: CallSiteId| (s.0 != 11).then_some(s),
        ];
        for map in maps {
            let merged = g.remap(FnId(9), map, file);
            assert!(merged.equal_after_remap(&g, FnId(9), map, file));
            assert!(!merged.equal_after_remap(&g, FnId(8), map, file));
            assert!(!merged.equal_after_remap(&g, FnId(9), map, |_| FileId(6)));
            for other in maps {
                let expected = merged.structurally_equal(&g.remap(FnId(9), other, file));
                assert_eq!(merged.equal_after_remap(&g, FnId(9), other, file), expected);
            }
        }
    }

    /// With an event spliced out, adding a variant remaps the new graph
    /// once, however many variants it is compared with.
    #[test]
    fn add_variant_remaps_once() {
        let mut variants = Vec::new();
        for n in 0..4u32 {
            let mut g = straight();
            g.events[0].sites = vec![CallSiteId(100 + n)];
            add_variant(&mut variants, &g, FnId(7), Some, |f| f).unwrap();
        }
        let mut g = straight();
        g.blocks[2].span = Some(span(2));
        // The second invocation loses its site.
        let drop = |s: CallSiteId| (s.0 != 11).then_some(s);
        let calls = std::cell::Cell::new(0usize);
        let file = |f: FileId| {
            calls.set(calls.get() + 1);
            f
        };
        g.remap(FnId(7), drop, file);
        let one = calls.replace(0);
        assert!(one > 0);
        add_variant(&mut variants, &g, FnId(7), drop, file).unwrap();
        assert_eq!(calls.get(), one);
        assert_eq!(variants.len(), 5);
        // And it is still recognised as a copy.
        assert!(add_variant(&mut variants, &g, FnId(7), drop, |f| f).is_none());
    }

    /// The splice in [`FunctionCfg::remap`] as first written: quadratic, the
    /// reference for the order the faster one must reproduce.
    fn reference_splice(
        order: &[(EventId, EventId)],
        renumber: &[Option<EventId>],
    ) -> Vec<(EventId, EventId)> {
        let mut order = order.to_vec();
        for (old, new) in renumber.iter().enumerate() {
            if new.is_some() {
                continue;
            }
            let x = EventId(old as u32);
            let preds: Vec<EventId> = order.iter().filter(|p| p.1 == x).map(|p| p.0).collect();
            let succs: Vec<EventId> = order.iter().filter(|p| p.0 == x).map(|p| p.1).collect();
            order.retain(|p| p.0 != x && p.1 != x);
            for &p in &preds {
                for &s in &succs {
                    if !order.contains(&(p, s)) {
                        order.push((p, s));
                    }
                }
            }
        }
        let at = |e: EventId| renumber[e.0 as usize];
        order
            .into_iter()
            .filter_map(|(a, b)| Some((at(a)?, at(b)?)))
            .collect()
    }

    /// One block of `n` events, each with its own site, ordered by `order`.
    fn ordered(n: u32, order: Vec<(EventId, EventId)>) -> FunctionCfg {
        let mut g = cfg(
            vec![
                block(BlockKind::Entry, &[]),
                block(BlockKind::Exit, &[]),
                block(BlockKind::Ordinary, &(0..n).collect::<Vec<_>>()),
            ],
            vec![
                edge(0, 2, EdgeKind::Fallthrough),
                edge(2, 1, EdgeKind::Fallthrough),
            ],
            (0..n).map(|e| event(2, e + 1, e)).collect(),
        );
        g.order = order;
        g
    }

    #[test]
    fn remap_splices_the_order_as_the_reference_does() {
        // xorshift: varied, reproducible graphs without a dependency.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |bound: u32| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % u64::from(bound)) as u32
        };
        for round in 0..500 {
            let n = 1 + next(12);
            let pairs = next(3 * n);
            let order: Vec<(EventId, EventId)> = (0..pairs)
                .map(|_| (EventId(next(n)), EventId(next(n))))
                .collect();
            let keep: Vec<bool> = (0..n).map(|_| next(3) != 0).collect();
            let mut renumber = Vec::new();
            let mut kept = 0;
            for &k in &keep {
                renumber.push(k.then(|| {
                    kept += 1;
                    EventId(kept - 1)
                }));
            }
            let g = ordered(n, order.clone());
            let merged = g.remap(FnId(7), |s| keep[s.0 as usize].then_some(s), |f| f);
            assert_eq!(
                merged.order,
                reference_splice(&order, &renumber),
                "round {round}: {order:?} keeping {keep:?}"
            );
        }
    }

    /// Splicing every other event of a long chain is linear, not quadratic.
    #[test]
    fn remap_splices_a_long_chain_quickly() {
        let n = 200_000u32;
        let order = (1..n).map(|e| (EventId(e - 1), EventId(e))).collect();
        let g = ordered(n, order);
        let merged = g.remap(FnId(7), |s| (s.0 % 2 == 0).then_some(s), |f| f);
        let expected: Vec<_> = (1..n / 2).map(|e| (EventId(e - 1), EventId(e))).collect();
        let mut got = merged.order;
        got.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn add_variant_keeps_distinct_bodies_numbered() {
        let mut variants = Vec::new();
        let a = straight();
        let kept = add_variant(&mut variants, &a, FnId(7), Some, |f| f).unwrap();
        kept.tu = Some(FileId(1));
        assert!(add_variant(&mut variants, &a, FnId(7), Some, |f| f).is_none());
        let mut b = straight();
        b.events.pop();
        b.blocks[2].events.pop();
        b.variant = 5;
        assert_eq!(
            add_variant(&mut variants, &b, FnId(7), Some, |f| f).map(|v| v.variant),
            Some(1)
        );
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].tu, Some(FileId(1)));
    }
}
