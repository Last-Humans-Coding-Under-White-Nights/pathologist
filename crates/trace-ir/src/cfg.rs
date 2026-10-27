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
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEvent {
    pub block: BlockId,
    pub kind: EventKind,
    pub occurrence: CallOccurrence,
    pub sites: Vec<CallSiteId>,
}

/// A region whose order is not modeled; `events` is an inclusive range.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl FunctionCfg {
    /// Same body structure, whichever unit or variant slot it came from.
    pub fn structurally_equal(&self, other: &Self) -> bool {
        self.function == other.function
            && self.entry == other.entry
            && self.exit == other.exit
            && self.exceptional_exit == other.exceptional_exit
            && self.blocks == other.blocks
            && self.edges == other.edges
            && self.events == other.events
            && self.order == other.order
            && self.gaps == other.gaps
    }

    /// The graph in another program's ids: `function` for its function,
    /// sites through `map_site`, files through `map_file`. An event left
    /// with no site is spliced out of its block and the local order, and an
    /// `omitted_sites` gap marks where it was (docs/ANALYSIS.md, "Call
    /// sequencing"). `tu` and `variant` are kept.
    pub fn remap(
        &self,
        function: FnId,
        map_site: impl Fn(CallSiteId) -> Option<CallSiteId>,
        map_file: impl Fn(FileId) -> FileId,
    ) -> FunctionCfg {
        let cfg = self;
        let map_span = |span: Span| Span {
            file: map_file(span.file),
            ..span
        };
        let mut events = Vec::with_capacity(cfg.events.len());
        // Old event index -> new id, `None` once spliced out.
        let mut renumber: Vec<Option<EventId>> = Vec::with_capacity(cfg.events.len());
        let mut omitted = Vec::new();
        for event in &cfg.events {
            // Coalesced records of one invocation name one merged site.
            let mut sites: Vec<CallSiteId> = Vec::with_capacity(event.sites.len());
            for site in event.sites.iter().filter_map(|&s| map_site(s)) {
                if !sites.contains(&site) {
                    sites.push(site);
                }
            }
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
        let mut order = cfg.order.clone();
        // Join each spliced event's predecessors to its successors, so the
        // events around it keep their order.
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
        let order = order
            .into_iter()
            .filter_map(|(a, b)| Some((at(a)?, at(b)?)))
            .collect();
        let blocks = cfg
            .blocks
            .iter()
            .map(|b| Block {
                kind: b.kind,
                span: b.span.map(map_span),
                events: b.events.iter().filter_map(|&e| at(e)).collect(),
            })
            .collect();
        let mut gaps: Vec<Gap> = cfg
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
            tu: cfg.tu,
            variant: cfg.variant,
            entry: cfg.entry,
            exit: cfg.exit,
            exceptional_exit: cfg.exceptional_exit,
            blocks,
            edges: cfg.edges.clone(),
            events,
            order,
            gaps,
        }
    }

    /// Remove event-free ordinary blocks that only fall through to another
    /// block, redirecting their incoming edges; branch, loop, exit and
    /// landing structure is kept.
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
        let mut seen = rustc_hash::FxHashSet::default();
        let edges = std::mem::take(&mut self.edges);
        self.edges = edges
            .into_iter()
            .filter(|e| forward[e.from.0 as usize].is_none())
            .map(|e| CfgEdge {
                from: map(e.from),
                to: map(e.to),
                kind: e.kind,
            })
            .filter(|e| seen.insert(*e))
            .collect();
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
}
