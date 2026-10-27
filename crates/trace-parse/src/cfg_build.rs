//! Call-order control-flow graph of one function body (`--sequencing`).
//!
//! Lowering records which syntax node emitted which call sites
//! ([`CfgRecorder`]). Once a body is walked, [`build_function_cfg`] walks the
//! same tree again and places each node's sites, as events, in blocks and in
//! a block-local sequenced-before order. Semantics and limits:
//! docs/ANALYSIS.md, "Call sequencing (control-flow sidecar)".

use super::{node_span, LowerContext, MAX_AST_WALK_DEPTH};
use crate::node_metadata::NodeMetadata;
use crate::parse::node_text;
use rustc_hash::FxHashMap as HashMap;
use std::ops::Range;
use trace_ir::{
    Block, BlockId, BlockKind, CallEvent, CallOccurrence, CallSite, CallSiteId, CfgEdge, EdgeKind,
    EventId, EventKind, FnId, FunctionCfg, Gap, GapKind, Program,
};
use tree_sitter::Node;

/// Call sites by the syntax node whose lowering emitted them, as
/// `call_sites` index ranges in emission order.
#[derive(Default)]
pub(super) struct CfgRecorder {
    /// A node records after the nodes its lowering walked, so an earlier
    /// entry claims a site before a later one.
    owned: Vec<(usize, Range<usize>)>,
    /// Destructor sites of the automatic object a declarator declares;
    /// these claim before `owned`.
    destructors: Vec<(usize, Range<usize>)>,
}

/// Lengths of a recorder's lists, to drop what was recorded after.
#[derive(Clone, Copy)]
pub(super) struct RecorderMark(usize, usize);

impl CfgRecorder {
    pub(super) fn own(&mut self, node: Node, sites: Range<usize>) {
        if !sites.is_empty() {
            self.owned.push((node.id(), sites));
        }
    }

    pub(super) fn destructors(&mut self, node: Node, sites: Range<usize>) {
        if !sites.is_empty() {
            self.destructors.push((node.id(), sites));
        }
    }

    pub(super) fn mark(&self) -> RecorderMark {
        RecorderMark(self.owned.len(), self.destructors.len())
    }

    pub(super) fn truncate(&mut self, mark: RecorderMark) {
        self.owned.truncate(mark.0);
        self.destructors.truncate(mark.1);
    }
}

/// Build the graph of `function`'s body from the sites lowered for it since
/// `first_site`, and add it to `program.cfgs`. `prologue` holds the
/// member-initializer lists, which run before the body.
pub(super) fn build_function_cfg(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    function: FnId,
    first_site: usize,
    body: Option<Node>,
    prologue: &[Node],
) {
    let Some(recorder) = &ctx.cfg_recorder else {
        return;
    };
    let (mut cfg, spans) = {
        let mut builder = Builder::new(
            &program.symbols.call_sites,
            recorder,
            source,
            function,
            first_site,
            ctx.is_cpp,
            ctx.is_cpp && ctx.cpp_standard >= 201703,
        );
        // A lambda's body nests in the walk that lowered it.
        builder.depth = ctx.ast_depth;
        builder.run(body, prologue);
        (builder.cfg, builder.spans)
    };
    for (slot, node) in spans {
        let span = Some(node_span(program, ctx, node));
        match slot {
            SpanSlot::Block(b) => cfg.blocks[b.0 as usize].span = span,
            SpanSlot::Gap(g) => cfg.gaps[g].span = span,
        }
    }
    cfg.compact();
    program.cfgs.entry(function).or_default().push(cfg);
}

enum SpanSlot {
    Block(BlockId),
    Gap(usize),
}

enum ScopeKind {
    Block,
    Loop {
        exit: BlockId,
        next: BlockId,
        next_kind: EdgeKind,
    },
    Switch {
        exit: BlockId,
        head: BlockId,
        has_default: bool,
    },
    Try {
        landing: BlockId,
    },
}

struct Scope {
    kind: ScopeKind,
    /// The compound statement the scope is, for `goto` targets.
    node: Option<usize>,
    /// Destructor sites of the automatic objects declared so far, one entry
    /// per object in declaration order.
    objects: Vec<Vec<usize>>,
}

/// A label's block and the compound statements around it, innermost first.
struct Label {
    block: BlockId,
    enclosing: Vec<usize>,
}

struct Builder<'s, 'a> {
    sites: &'s [CallSite],
    source: &'a str,
    cpp: bool,
    /// C++17 sequencing rules for callees, assignments, shifts and
    /// subscripts.
    cpp17: bool,
    /// Sites by the node that owns them, taken as the node is visited.
    owned: HashMap<usize, Vec<usize>>,
    destructors: HashMap<usize, Vec<usize>>,
    /// The function's sites no record claims, in emission order.
    unclaimed: Vec<usize>,
    cfg: FunctionCfg,
    has_pred: Vec<bool>,
    /// The block events go to; `None` after a jump, until a block that can
    /// be reached starts.
    cur: Option<BlockId>,
    /// The latest events of `cur`, sequenced before the next event.
    frontier: Vec<EventId>,
    scopes: Vec<Scope>,
    labels: HashMap<&'a str, Label>,
    depth: u32,
    spans: Vec<(SpanSlot, Node<'a>)>,
}

impl<'s, 'a> Builder<'s, 'a> {
    fn new(
        sites: &'s [CallSite],
        recorder: &CfgRecorder,
        source: &'a str,
        function: FnId,
        first_site: usize,
        cpp: bool,
        cpp17: bool,
    ) -> Self {
        let first_site = first_site.min(sites.len());
        let mut claimed = vec![false; sites.len() - first_site];
        let mut claim = |records: &[(usize, Range<usize>)]| {
            let mut by_node: HashMap<usize, Vec<usize>> = HashMap::default();
            for (node, range) in records {
                for i in range.start.max(first_site)..range.end.min(sites.len()) {
                    if sites[i].caller == function && !claimed[i - first_site] {
                        claimed[i - first_site] = true;
                        by_node.entry(*node).or_default().push(i);
                    }
                }
            }
            by_node
        };
        let destructors = claim(&recorder.destructors);
        let owned = claim(&recorder.owned);
        let unclaimed = (first_site..sites.len())
            .filter(|&i| sites[i].caller == function && !claimed[i - first_site])
            .collect();
        let mut builder = Self {
            sites,
            source,
            cpp,
            cpp17,
            owned,
            destructors,
            unclaimed,
            cfg: FunctionCfg {
                function,
                tu: None,
                variant: 0,
                entry: BlockId(0),
                exit: BlockId(1),
                exceptional_exit: None,
                blocks: Vec::new(),
                edges: Vec::new(),
                events: Vec::new(),
                order: Vec::new(),
                gaps: Vec::new(),
            },
            has_pred: Vec::new(),
            cur: None,
            frontier: Vec::new(),
            scopes: Vec::new(),
            labels: HashMap::default(),
            depth: 0,
            spans: Vec::new(),
        };
        builder.new_block(BlockKind::Entry);
        builder.new_block(BlockKind::Exit);
        builder
    }

    fn run(&mut self, body: Option<Node<'a>>, prologue: &[Node<'a>]) {
        let entry = self.cfg.entry;
        // Filled last, once the body has taken the sites it owns.
        let head = self.new_block(BlockKind::Ordinary);
        self.edge(entry, head, EdgeKind::Fallthrough);
        let start = self.new_block(BlockKind::Ordinary);
        self.edge(head, start, EdgeKind::Fallthrough);
        self.start(start);
        if let Some(body) = body {
            self.collect_labels(body);
            self.visit(body);
        }
        let exit = self.cfg.exit;
        self.jump(exit, EdgeKind::Fallthrough);
        self.fill_prologue(head, prologue);
    }

    // ---- blocks, edges and events ----

    fn new_block(&mut self, kind: BlockKind) -> BlockId {
        let id = BlockId(self.cfg.blocks.len() as u32);
        self.cfg.blocks.push(Block {
            kind,
            span: None,
            events: Vec::new(),
        });
        self.has_pred.push(false);
        id
    }

    fn block_at(&mut self, node: Node<'a>) -> BlockId {
        let b = self.new_block(BlockKind::Ordinary);
        self.spans.push((SpanSlot::Block(b), node));
        b
    }

    fn edge(&mut self, from: BlockId, to: BlockId, kind: EdgeKind) {
        self.cfg.edges.push(CfgEdge { from, to, kind });
        self.has_pred[to.0 as usize] = true;
    }

    fn start(&mut self, b: BlockId) {
        self.cur = Some(b);
        self.frontier.clear();
    }

    /// Continue at `b` when something reaches it.
    fn enter(&mut self, b: BlockId) {
        if self.has_pred[b.0 as usize] {
            self.start(b);
        } else {
            self.cur = None;
        }
    }

    /// Leave the current block for `to`; what follows is unreachable until
    /// a reachable block starts.
    fn jump(&mut self, to: BlockId, kind: EdgeKind) {
        if let Some(from) = self.cur.take() {
            self.edge(from, to, kind);
        }
        self.frontier.clear();
    }

    fn current(&mut self) -> BlockId {
        match self.cur {
            Some(b) => b,
            None => {
                let b = self.new_block(BlockKind::Ordinary);
                self.start(b);
                b
            }
        }
    }

    fn emit(&mut self, kind: EventKind, occurrence: CallOccurrence, sites: Vec<CallSiteId>) {
        let block = self.current();
        let id = EventId(self.cfg.events.len() as u32);
        self.cfg.events.push(CallEvent {
            block,
            kind,
            occurrence,
            sites,
        });
        self.cfg.blocks[block.0 as usize].events.push(id);
        for &before in &self.frontier {
            self.cfg.order.push((before, id));
        }
        self.frontier.clear();
        self.frontier.push(id);
    }

    /// Emit the sites at `indices`, one event per invocation, unordered
    /// among themselves and after the current frontier.
    fn emit_unordered(&mut self, indices: &[usize], kind: Option<EventKind>) {
        let before = std::mem::take(&mut self.frontier);
        let mut sinks = Vec::new();
        for invocation in invocations(self.sites, indices) {
            let kind = kind.unwrap_or_else(|| event_kind(&self.sites[invocation.first]));
            self.frontier.clone_from(&before);
            self.emit(kind, invocation.occurrence, invocation.sites);
            sinks.push(EventId(self.cfg.events.len() as u32 - 1));
        }
        self.frontier = if sinks.is_empty() { before } else { sinks };
    }

    fn gap(&mut self, kind: GapKind, node: Option<Node<'a>>, first_event: usize) {
        let events = (self.cfg.events.len() > first_event).then(|| {
            (
                EventId(first_event as u32),
                EventId(self.cfg.events.len() as u32 - 1),
            )
        });
        if let Some(node) = node {
            self.spans.push((SpanSlot::Gap(self.cfg.gaps.len()), node));
        }
        self.cfg.gaps.push(Gap {
            kind,
            span: None,
            events,
        });
    }

    // ---- what nodes own ----

    /// The node's own events, after everything evaluated before them.
    fn own(&mut self, node: Node<'a>) {
        if let Some(indices) = self.owned.remove(&node.id()) {
            let kind = (node.cached_kind() == "delete_expression").then_some(EventKind::Delete);
            self.emit_unordered(&indices, kind);
        }
        if let Some(indices) = self.destructors.remove(&node.id()) {
            match self.scopes.last_mut() {
                Some(scope) => scope.objects.push(indices),
                None => self.unclaimed.extend(indices),
            }
        }
    }

    /// Destructor events of the objects of `scopes[from..]`, innermost
    /// first and in reverse declaration order.
    fn unwind(&mut self, from: usize) {
        if self.cur.is_none() {
            return;
        }
        let objects: Vec<Vec<usize>> = self.scopes[from..]
            .iter()
            .rev()
            .flat_map(|s| s.objects.iter().rev().cloned())
            .collect();
        for indices in objects {
            for invocation in invocations(self.sites, &indices) {
                self.emit(EventKind::Destroy, invocation.occurrence, invocation.sites);
            }
        }
    }

    fn push_scope(&mut self, kind: ScopeKind, node: Option<Node<'a>>) {
        self.scopes.push(Scope {
            kind,
            node: node.map(|n| n.id()),
            objects: Vec::new(),
        });
    }

    /// Leave the innermost scope normally.
    fn pop_scope(&mut self) {
        self.unwind(self.scopes.len() - 1);
        self.scopes.pop();
    }

    fn fill_prologue(&mut self, head: BlockId, prologue: &[Node<'a>]) {
        self.start(head);
        let first = self.cfg.events.len();
        for &node in prologue {
            self.flatten(node);
        }
        // Sites of nodes the walk never reached: default member
        // initializers, written in the class.
        let mut rest: Vec<usize> = std::mem::take(&mut self.unclaimed);
        rest.extend(self.owned.drain().flat_map(|(_, v)| v));
        rest.extend(self.destructors.drain().flat_map(|(_, v)| v));
        rest.sort_unstable();
        self.frontier.clear();
        self.emit_unordered(&rest, None);
        // Where an unreached site runs is not known either.
        if self.cfg.events.len() > first + 1 || !rest.is_empty() {
            self.gap(GapKind::InitializerOrder, prologue.first().copied(), first);
        }
    }

    /// Every event of `node`'s subtree, as one unordered group.
    fn flatten(&mut self, node: Node<'a>) {
        let before = self.frontier.clone();
        let mut sinks = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            self.frontier.clone_from(&before);
            let count = self.cfg.events.len();
            self.own(n);
            sinks.extend((count..self.cfg.events.len()).map(|e| EventId(e as u32)));
            let mut cursor = n.walk();
            let children: Vec<_> = n.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        self.frontier = if sinks.is_empty() { before } else { sinks };
    }

    /// A construct whose order is not modeled: its events as one unordered
    /// group, under a gap.
    fn opaque(&mut self, node: Node<'a>, kind: GapKind) {
        let first = self.cfg.events.len();
        self.flatten(node);
        self.gap(kind, Some(node), first);
    }
}

/// The alternative-target sites of one syntactic invocation.
struct Invocation {
    occurrence: CallOccurrence,
    /// Index of the first of `sites` in `call_sites`.
    first: usize,
    sites: Vec<CallSiteId>,
}

/// The sites at `indices` grouped by invocation, in first-seen order.
fn invocations(sites: &[CallSite], indices: &[usize]) -> Vec<Invocation> {
    let mut groups: Vec<Invocation> = Vec::new();
    let mut by_occurrence: HashMap<CallOccurrence, usize> = HashMap::default();
    for &i in indices {
        let occurrence = sites[i].occurrence();
        match by_occurrence.entry(occurrence) {
            std::collections::hash_map::Entry::Occupied(at) => {
                groups[*at.get()].sites.push(sites[i].id);
            }
            std::collections::hash_map::Entry::Vacant(at) => {
                at.insert(groups.len());
                groups.push(Invocation {
                    occurrence,
                    first: i,
                    sites: vec![sites[i].id],
                });
            }
        }
    }
    groups
}

/// What a site's callee name says it runs: `A::A` constructs, `A::~A`
/// destroys.
fn event_kind(site: &CallSite) -> EventKind {
    let name = site.callee_name.as_str();
    let Some((scope, member)) = name.rsplit_once("::") else {
        return EventKind::Call;
    };
    if member.starts_with('~') {
        return EventKind::Destroy;
    }
    let class = scope.rsplit("::").next().unwrap_or(scope);
    let class = class.split('<').next().unwrap_or(class);
    if class == member {
        EventKind::Construct
    } else {
        EventKind::Call
    }
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    node.named_children(&mut node.walk()).collect()
}

impl<'a> Builder<'_, 'a> {
    fn visit(&mut self, node: Node<'a>) {
        if self.depth >= MAX_AST_WALK_DEPTH {
            self.opaque(node, GapKind::DepthExceeded);
            return;
        }
        self.depth += 1;
        self.visit_node(node);
        self.depth -= 1;
    }

    fn visit_node(&mut self, node: Node<'a>) {
        if node.is_error() {
            self.opaque(node, GapKind::ParseError);
            return;
        }
        match node.cached_kind() {
            // A GNU statement expression.
            "compound_statement"
                if node
                    .parent()
                    .is_some_and(|p| p.cached_kind() == "parenthesized_expression") =>
            {
                self.opaque(node, GapKind::UnsupportedConstruct);
            }
            "compound_statement" => {
                self.push_scope(ScopeKind::Block, Some(node));
                self.sequence(node);
                self.own(node);
                self.pop_scope();
            }
            "co_return_statement"
            | "co_await_expression"
            | "co_yield_statement"
            | "seh_try_statement"
            | "seh_leave_statement" => self.opaque(node, GapKind::UnsupportedConstruct),
            "if_statement" => self.if_statement(node),
            "while_statement" => self.while_statement(node),
            "do_statement" => self.do_statement(node),
            "for_statement" => self.for_statement(node),
            "for_range_loop" => self.for_range_loop(node),
            "switch_statement" => self.switch_statement(node),
            "case_statement" => self.case_statement(node),
            "break_statement" => self.break_statement(),
            "continue_statement" => self.continue_statement(),
            "return_statement" => {
                self.sequence(node);
                self.own(node);
                self.unwind(0);
                let exit = self.cfg.exit;
                self.jump(exit, EdgeKind::Return);
            }
            "goto_statement" => self.goto_statement(node),
            "labeled_statement" => self.labeled_statement(node),
            "try_statement" => self.try_statement(node),
            "throw_statement" => {
                self.sequence(node);
                self.own(node);
                let target = self.throw_target();
                self.jump(target, EdgeKind::Exception);
            }
            "lambda_expression" => {
                // The body is the lambda's own graph; init-captures are this
                // function's.
                if let Some(captures) = node.cached_field("captures") {
                    self.unordered(&named_children(captures), node);
                }
                self.own(node);
            }
            "call_expression" => {
                let function = node.cached_field("function");
                let mut args = node
                    .cached_field("arguments")
                    .map(named_children)
                    .unwrap_or_default();
                if self.cpp17 {
                    if let Some(function) = function {
                        self.visit(function);
                    }
                } else if let Some(function) = function {
                    args.insert(0, function);
                }
                self.unordered(&args, node);
                let first = self.cfg.events.len();
                self.own(node);
                // Control may come back to a `setjmp` from any later call.
                if function.is_some_and(|f| is_nonlocal_jump(node_text(self.source, &f))) {
                    self.gap(GapKind::UnsupportedConstruct, Some(node), first);
                }
            }
            "binary_expression" => {
                let operator = node.cached_field("operator").map(|o| o.kind());
                match operator {
                    Some(op @ ("&&" | "||" | "and" | "or")) => {
                        self.short_circuit(node, matches!(op, "&&" | "and"));
                    }
                    Some("<<" | ">>") if self.cpp17 => self.sequence(node),
                    _ => self.unordered(&named_children(node), node),
                }
                self.own(node);
            }
            "assignment_expression" => {
                let (left, right) = (node.cached_field("left"), node.cached_field("right"));
                if self.cpp17 {
                    for side in [right, left].into_iter().flatten() {
                        self.visit(side);
                    }
                } else {
                    let sides: Vec<_> = [left, right].into_iter().flatten().collect();
                    self.unordered(&sides, node);
                }
                self.own(node);
            }
            "subscript_expression" if self.cpp17 => {
                self.sequence(node);
                self.own(node);
            }
            "initializer_list" if self.cpp => {
                self.sequence(node);
                self.own(node);
            }
            "conditional_expression" => {
                self.conditional(node);
                self.own(node);
            }
            kind if is_sequenced(kind) => {
                self.sequence(node);
                self.own(node);
            }
            _ => {
                self.unordered(&named_children(node), node);
                self.own(node);
            }
        }
    }

    /// Each named child in turn.
    fn sequence(&mut self, node: Node<'a>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child);
        }
    }

    /// Operands evaluated in no specified order: each starts after what
    /// precedes the group, and what follows waits for all of them. A branch
    /// inside one makes the others' order relative to it unknown.
    fn unordered(&mut self, operands: &[Node<'a>], node: Node<'a>) {
        if operands.len() < 2 {
            for &operand in operands {
                self.visit(operand);
            }
            return;
        }
        let block = Some(self.current());
        let before = self.frontier.clone();
        let first = self.cfg.events.len();
        let mut ends = Vec::new();
        for &operand in operands {
            if self.cur == block {
                self.frontier.clone_from(&before);
            } else {
                self.frontier.clear();
            }
            self.visit(operand);
            ends.push((self.cur, std::mem::take(&mut self.frontier)));
        }
        let mut sinks = Vec::new();
        for (end, frontier) in ends {
            if end == self.cur {
                for e in frontier {
                    if !sinks.contains(&e) {
                        sinks.push(e);
                    }
                }
            }
        }
        if self.cur != block {
            self.gap(GapKind::UnorderedBranch, Some(node), first);
        }
        self.frontier = sinks;
    }

    // ---- expressions that branch ----

    fn short_circuit(&mut self, node: Node<'a>, and: bool) {
        if let Some(left) = node.cached_field("left") {
            self.visit(left);
        }
        let test = self.current();
        let (right_kind, skip_kind) = if and {
            (EdgeKind::BranchTrue, EdgeKind::BranchFalse)
        } else {
            (EdgeKind::BranchFalse, EdgeKind::BranchTrue)
        };
        let right = self.new_block(BlockKind::Ordinary);
        let join = self.new_block(BlockKind::Ordinary);
        self.edge(test, right, right_kind);
        self.edge(test, join, skip_kind);
        self.start(right);
        if let Some(operand) = node.cached_field("right") {
            self.visit(operand);
        }
        self.jump(join, EdgeKind::Fallthrough);
        self.start(join);
    }

    fn conditional(&mut self, node: Node<'a>) {
        if let Some(condition) = node.cached_field("condition") {
            self.visit(condition);
        }
        let test = self.current();
        let join = self.new_block(BlockKind::Ordinary);
        for (field, kind) in [
            ("consequence", EdgeKind::BranchTrue),
            ("alternative", EdgeKind::BranchFalse),
        ] {
            let arm = self.new_block(BlockKind::Ordinary);
            self.edge(test, arm, kind);
            self.start(arm);
            if let Some(operand) = node.cached_field(field) {
                self.visit(operand);
            }
            self.jump(join, EdgeKind::Fallthrough);
        }
        self.enter(join);
    }

    // ---- statements ----

    fn if_statement(&mut self, node: Node<'a>) {
        // A condition's declaration lives until the statement ends.
        self.push_scope(ScopeKind::Block, None);
        if let Some(condition) = node.cached_field("condition") {
            self.visit(condition);
        }
        let test = self.current();
        let join = self.new_block(BlockKind::Ordinary);
        let then = self.new_block(BlockKind::Ordinary);
        self.edge(test, then, EdgeKind::BranchTrue);
        self.start(then);
        if let Some(consequence) = node.cached_field("consequence") {
            self.visit(consequence);
        }
        self.jump(join, EdgeKind::Fallthrough);
        match node.cached_field("alternative") {
            Some(alternative) => {
                let other = self.new_block(BlockKind::Ordinary);
                self.edge(test, other, EdgeKind::BranchFalse);
                self.start(other);
                self.visit(alternative);
                self.jump(join, EdgeKind::Fallthrough);
            }
            None => self.edge(test, join, EdgeKind::BranchFalse),
        }
        self.enter(join);
        self.pop_scope();
    }

    fn while_statement(&mut self, node: Node<'a>) {
        let head = self.block_at(node);
        self.jump(head, EdgeKind::Fallthrough);
        self.start(head);
        if let Some(condition) = node.cached_field("condition") {
            self.visit(condition);
        }
        let test = self.current();
        let body = self.new_block(BlockKind::Ordinary);
        let exit = self.new_block(BlockKind::Ordinary);
        self.edge(test, body, EdgeKind::BranchTrue);
        self.edge(test, exit, EdgeKind::BranchFalse);
        self.loop_body(node, body, exit, head, EdgeKind::BackEdge);
        self.jump(head, EdgeKind::BackEdge);
        self.enter(exit);
    }

    fn do_statement(&mut self, node: Node<'a>) {
        let body = self.block_at(node);
        self.jump(body, EdgeKind::Fallthrough);
        let test = self.new_block(BlockKind::Ordinary);
        let exit = self.new_block(BlockKind::Ordinary);
        self.loop_body(node, body, exit, test, EdgeKind::Fallthrough);
        self.jump(test, EdgeKind::Fallthrough);
        self.enter(test);
        if let Some(condition) = node.cached_field("condition") {
            self.visit(condition);
        }
        if let Some(end) = self.cur {
            self.edge(end, body, EdgeKind::BackEdge);
        }
        self.jump(exit, EdgeKind::BranchFalse);
        self.enter(exit);
    }

    fn for_statement(&mut self, node: Node<'a>) {
        // The init-statement's declarations live until the loop ends.
        self.push_scope(ScopeKind::Block, None);
        if let Some(init) = node.cached_field("initializer") {
            self.visit(init);
        }
        let head = self.block_at(node);
        self.jump(head, EdgeKind::Fallthrough);
        self.start(head);
        let condition = node.cached_field("condition");
        if let Some(condition) = condition {
            self.visit(condition);
        }
        let test = self.current();
        let body = self.new_block(BlockKind::Ordinary);
        let exit = self.new_block(BlockKind::Ordinary);
        let update = self.new_block(BlockKind::Ordinary);
        if condition.is_some() {
            self.edge(test, body, EdgeKind::BranchTrue);
            self.edge(test, exit, EdgeKind::BranchFalse);
        } else {
            self.edge(test, body, EdgeKind::Fallthrough);
        }
        self.loop_body(node, body, exit, update, EdgeKind::Fallthrough);
        self.jump(update, EdgeKind::Fallthrough);
        self.enter(update);
        if let Some(step) = node.cached_field("update") {
            self.visit(step);
        }
        self.jump(head, EdgeKind::BackEdge);
        self.enter(exit);
        self.pop_scope();
    }

    fn for_range_loop(&mut self, node: Node<'a>) {
        self.push_scope(ScopeKind::Block, None);
        for field in ["initializer", "right"] {
            if let Some(child) = node.cached_field(field) {
                self.visit(child);
            }
        }
        let head = self.block_at(node);
        self.jump(head, EdgeKind::Fallthrough);
        let body = self.new_block(BlockKind::Ordinary);
        let exit = self.new_block(BlockKind::Ordinary);
        self.edge(head, body, EdgeKind::BranchTrue);
        self.edge(head, exit, EdgeKind::BranchFalse);
        self.loop_body(node, body, exit, head, EdgeKind::BackEdge);
        self.jump(head, EdgeKind::BackEdge);
        self.enter(exit);
        self.pop_scope();
    }

    /// Walk a loop's body from `body`; `break` goes to `exit`, `continue`
    /// to `next`. A range-for variable lives in the body.
    fn loop_body(
        &mut self,
        node: Node<'a>,
        body: BlockId,
        exit: BlockId,
        next: BlockId,
        next_kind: EdgeKind,
    ) {
        self.push_scope(
            ScopeKind::Loop {
                exit,
                next,
                next_kind,
            },
            None,
        );
        self.enter(body);
        self.push_scope(ScopeKind::Block, None);
        if node.cached_kind() == "for_range_loop" {
            if let Some(declarator) = node.cached_field("declarator") {
                self.own(declarator);
            }
        }
        if let Some(statement) = node.cached_field("body") {
            self.visit(statement);
        }
        self.pop_scope();
        self.scopes.pop();
    }

    fn switch_statement(&mut self, node: Node<'a>) {
        self.push_scope(ScopeKind::Block, None);
        if let Some(condition) = node.cached_field("condition") {
            self.visit(condition);
        }
        let head = self.current();
        let exit = self.new_block(BlockKind::Ordinary);
        self.push_scope(
            ScopeKind::Switch {
                exit,
                head,
                has_default: false,
            },
            None,
        );
        // Only a case label reaches the statements of the body.
        self.cur = None;
        if let Some(body) = node.cached_field("body") {
            self.visit(body);
        }
        let Some(Scope {
            kind: ScopeKind::Switch { has_default, .. },
            ..
        }) = self.scopes.pop()
        else {
            unreachable!("switch scope");
        };
        if !has_default {
            self.edge(head, exit, EdgeKind::SwitchCase);
        }
        self.jump(exit, EdgeKind::Fallthrough);
        self.enter(exit);
        self.pop_scope();
    }

    fn case_statement(&mut self, node: Node<'a>) {
        let label = self.block_at(node);
        self.jump(label, EdgeKind::Fallthrough);
        let value = node.cached_field("value");
        let switch = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|s| match &mut s.kind {
                ScopeKind::Switch {
                    head, has_default, ..
                } => Some((head, has_default)),
                _ => None,
            });
        if let Some((head, has_default)) = switch {
            *has_default |= value.is_none();
            let head = *head;
            self.edge(head, label, EdgeKind::SwitchCase);
        }
        self.enter(label);
        for child in named_children(node) {
            if Some(child.id()) != value.map(|v| v.id()) {
                self.visit(child);
            }
        }
    }

    fn break_statement(&mut self) {
        let target = self
            .scopes
            .iter()
            .rposition(|s| matches!(s.kind, ScopeKind::Loop { .. } | ScopeKind::Switch { .. }));
        let Some(at) = target else {
            return;
        };
        self.unwind(at + 1);
        let exit = match self.scopes[at].kind {
            ScopeKind::Loop { exit, .. } | ScopeKind::Switch { exit, .. } => exit,
            _ => unreachable!(),
        };
        self.jump(exit, EdgeKind::Fallthrough);
    }

    fn continue_statement(&mut self) {
        let target = self
            .scopes
            .iter()
            .rposition(|s| matches!(s.kind, ScopeKind::Loop { .. }));
        let Some(at) = target else {
            return;
        };
        self.unwind(at + 1);
        let ScopeKind::Loop {
            next, next_kind, ..
        } = self.scopes[at].kind
        else {
            unreachable!();
        };
        self.jump(next, next_kind);
    }

    fn goto_statement(&mut self, node: Node<'a>) {
        let label = node
            .cached_field("label")
            .filter(|_| !node.has_error())
            .map(|l| node_text(self.source, &l));
        let Some(label) = label else {
            // `goto *p;`
            self.unsupported_exit(node, GapKind::UnsupportedConstruct);
            return;
        };
        let Some(target) = self.labels.get(label) else {
            self.unsupported_exit(node, GapKind::UnresolvedGoto);
            return;
        };
        let block = target.block;
        // Leave the scopes the label is not inside.
        let keep = self
            .scopes
            .iter()
            .rposition(|s| s.node.is_some_and(|n| target.enclosing.contains(&n)))
            .map_or(0, |at| at + 1);
        self.unwind(keep);
        self.jump(block, EdgeKind::Fallthrough);
    }

    fn unsupported_exit(&mut self, node: Node<'a>, kind: GapKind) {
        let first = self.cfg.events.len();
        self.gap(kind, Some(node), first);
        let exit = self.cfg.exit;
        self.jump(exit, EdgeKind::Unsupported);
    }

    fn labeled_statement(&mut self, node: Node<'a>) {
        let label = node.cached_field("label");
        let block = label
            .and_then(|l| self.labels.get(node_text(self.source, &l)))
            .map(|l| l.block);
        if let Some(block) = block {
            self.jump(block, EdgeKind::Fallthrough);
            self.start(block);
        }
        for child in named_children(node) {
            if Some(child.id()) != label.map(|l| l.id()) {
                self.visit(child);
            }
        }
    }

    /// Give every label of the body its block before the walk, so a `goto`
    /// may jump forward.
    fn collect_labels(&mut self, body: Node<'a>) {
        let mut stack = vec![body];
        while let Some(n) = stack.pop() {
            if n.cached_kind() == "lambda_expression" {
                continue;
            }
            if n.cached_kind() == "labeled_statement" {
                if let Some(label) = n.cached_field("label") {
                    let name = node_text(self.source, &label);
                    if !self.labels.contains_key(name) {
                        let block = self.block_at(n);
                        let mut enclosing = Vec::new();
                        let mut up = n.parent();
                        while let Some(p) = up {
                            if p.cached_kind() == "compound_statement" {
                                enclosing.push(p.id());
                            }
                            if p.id() == body.id() {
                                break;
                            }
                            up = p.parent();
                        }
                        self.labels.insert(name, Label { block, enclosing });
                    }
                }
            }
            stack.extend(named_children(n).into_iter().rev());
        }
    }

    fn throw_target(&mut self) -> BlockId {
        let landing = self.scopes.iter().rev().find_map(|s| match s.kind {
            ScopeKind::Try { landing } => Some(landing),
            _ => None,
        });
        if let Some(landing) = landing {
            return landing;
        }
        match self.cfg.exceptional_exit {
            Some(b) => b,
            None => {
                let b = self.new_block(BlockKind::ExceptionalExit);
                self.cfg.exceptional_exit = Some(b);
                b
            }
        }
    }

    fn try_statement(&mut self, node: Node<'a>) {
        let landing = self.new_block(BlockKind::Landing);
        let exit = self.new_block(BlockKind::Ordinary);
        let first_block = self.cfg.blocks.len();
        let body = self.block_at(node);
        self.jump(body, EdgeKind::Fallthrough);
        self.enter(body);
        self.push_scope(ScopeKind::Try { landing }, None);
        if let Some(statement) = node.cached_field("body") {
            self.visit(statement);
        }
        self.scopes.pop();
        // Any call in the body may throw.
        for b in first_block..self.cfg.blocks.len() {
            if !self.cfg.blocks[b].events.is_empty() {
                self.edge(BlockId(b as u32), landing, EdgeKind::Exception);
            }
        }
        self.jump(exit, EdgeKind::Fallthrough);
        let mut catch_all = false;
        for clause in named_children(node) {
            if clause.cached_kind() != "catch_clause" {
                continue;
            }
            let handler = self.block_at(clause);
            self.edge(landing, handler, EdgeKind::Fallthrough);
            self.start(handler);
            self.push_scope(ScopeKind::Block, None);
            if let Some(params) = clause.cached_field("parameters") {
                catch_all |= node_text(self.source, &params).contains("...");
                for param in named_children(params) {
                    self.own(param);
                    if let Some(declarator) = param.cached_field("declarator") {
                        self.own(declarator);
                    }
                }
            }
            if let Some(statement) = clause.cached_field("body") {
                self.visit(statement);
            }
            self.pop_scope();
            self.jump(exit, EdgeKind::Fallthrough);
        }
        if !catch_all {
            let outer = self.throw_target();
            self.edge(landing, outer, EdgeKind::Exception);
        }
        self.enter(exit);
    }
}

fn is_nonlocal_jump(callee: &str) -> bool {
    matches!(
        callee,
        "setjmp"
            | "_setjmp"
            | "sigsetjmp"
            | "__sigsetjmp"
            | "__builtin_setjmp"
            | "longjmp"
            | "_longjmp"
            | "siglongjmp"
            | "__builtin_longjmp"
    )
}

/// Node kinds whose named children are evaluated in order.
fn is_sequenced(kind: &str) -> bool {
    kind.ends_with("_statement")
        || matches!(
            kind,
            "declaration"
                | "init_declarator"
                | "condition_clause"
                | "init_statement"
                | "else_clause"
                | "parenthesized_expression"
                | "comma_expression"
                | "translation_unit"
        )
}
#[cfg(test)]
mod tests {
    use super::super::*;
    use std::collections::VecDeque;
    use trace_ir::{BlockId, EdgeKind, EventId, EventKind, FunctionCfg, GapKind};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/call_sequence")
            .join(name)
    }

    /// One fixture file lowered as its own unit.
    fn lower_with(name: &str, opts: PreprocessOptions) -> UnitIndex {
        let path = fixture(name);
        let root = path.parent().unwrap().to_path_buf();
        let headers: Vec<PathBuf> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "h"))
            .collect();
        let graph = IncludeGraph::build(&root, std::slice::from_ref(&path), &headers);
        let opts = project_preprocess_opts(&root, &opts, &graph);
        index_source_file(
            &path,
            &root,
            &graph,
            &opts,
            &IndexSourceCache::new(),
            None,
            &HeaderOrder::default(),
        )
    }

    fn lower(name: &str) -> UnitIndex {
        lower_with(name, PreprocessOptions::new().with_call_sequencing(true))
    }

    /// The graph of `function`, with the unit to read site names from.
    struct Graph<'u> {
        unit: &'u UnitIndex,
        cfg: &'u FunctionCfg,
    }

    fn graph<'u>(unit: &'u UnitIndex, function: &str) -> Graph<'u> {
        let id = unit
            .functions
            .iter()
            .find(|f| f.name == function && f.is_defined)
            .unwrap_or_else(|| panic!("no function {function}"))
            .id;
        let cfg = unit
            .cfgs
            .iter()
            .find(|c| c.function == id)
            .unwrap_or_else(|| panic!("no graph for {function}"));
        Graph { unit, cfg }
    }

    impl Graph<'_> {
        fn callee(&self, event: EventId) -> &str {
            let site = self.cfg.events[event.0 as usize].sites[0];
            let site = self.unit.call_sites.iter().find(|s| s.id == site).unwrap();
            site.callee_name.as_str()
        }

        /// Events, edges, order and gaps, for failure messages.
        fn dump(&self) -> String {
            let mut out = String::new();
            for (i, e) in self.cfg.events.iter().enumerate() {
                out += &format!(
                    "e{i} {} {:?} b{} line {}\n",
                    self.callee(EventId(i as u32)),
                    e.kind,
                    e.block.0,
                    e.occurrence.span.line
                );
            }
            for e in &self.cfg.edges {
                out += &format!("b{} -> b{} {:?}\n", e.from.0, e.to.0, e.kind);
            }
            out + &format!("order {:?}\ngaps {:?}\n", self.cfg.order, self.cfg.gaps)
        }

        fn all(&self, callee: &str) -> Vec<EventId> {
            (0..self.cfg.events.len() as u32)
                .map(EventId)
                .filter(|&e| self.callee(e) == callee)
                .collect()
        }

        /// The one event calling `callee`.
        fn ev(&self, callee: &str) -> EventId {
            let all = self.all(callee);
            assert_eq!(
                all.len(),
                1,
                "events calling {callee}: {all:?}\n{:#?}",
                self.cfg
            );
            all[0]
        }

        /// The one event calling `callee` written on `line`.
        fn ev_at(&self, callee: &str, line: u32) -> EventId {
            let all: Vec<_> = self
                .all(callee)
                .into_iter()
                .filter(|&e| self.cfg.events[e.0 as usize].occurrence.span.line == line)
                .collect();
            assert_eq!(all.len(), 1, "events calling {callee} at {line}: {all:?}");
            all[0]
        }

        fn block(&self, e: EventId) -> BlockId {
            self.cfg.events[e.0 as usize].block
        }

        fn kind(&self, e: EventId) -> EventKind {
            self.cfg.events[e.0 as usize].kind
        }

        /// A path of immediate sequenced-before pairs from `a` to `b`.
        fn sequenced(&self, a: EventId, b: EventId) -> bool {
            let mut seen = vec![a];
            let mut work = vec![a];
            while let Some(x) = work.pop() {
                for &(from, to) in &self.cfg.order {
                    if from == x && !seen.contains(&to) {
                        if to == b {
                            return true;
                        }
                        seen.push(to);
                        work.push(to);
                    }
                }
            }
            false
        }

        fn unordered(&self, a: EventId, b: EventId) -> bool {
            self.block(a) == self.block(b) && !self.sequenced(a, b) && !self.sequenced(b, a)
        }

        /// Blocks reachable in one or more steps from `from`, never entering
        /// `avoid`, over edges `keep` admits.
        fn reach(
            &self,
            from: BlockId,
            avoid: Option<BlockId>,
            keep: impl Fn(EdgeKind) -> bool,
        ) -> Vec<BlockId> {
            let mut seen = Vec::new();
            let mut work = VecDeque::from([from]);
            while let Some(x) = work.pop_front() {
                for e in &self.cfg.edges {
                    if e.from == x && keep(e.kind) && Some(e.to) != avoid && !seen.contains(&e.to) {
                        seen.push(e.to);
                        work.push_back(e.to);
                    }
                }
            }
            seen
        }

        fn block_path(&self, from: BlockId, to: BlockId, avoid: Option<BlockId>) -> bool {
            self.reach(from, avoid, |_| true).contains(&to)
        }

        /// Some execution may invoke `a` and later `b`.
        fn may_precede(&self, a: EventId, b: EventId) -> bool {
            (self.block(a) == self.block(b) && self.sequenced(a, b))
                || self.block_path(self.block(a), self.block(b), None)
        }

        fn exclusive(&self, a: EventId, b: EventId) -> bool {
            !self.may_precede(a, b) && !self.may_precede(b, a) && self.block(a) != self.block(b)
        }

        fn covered(&self, e: EventId) -> bool {
            self.cfg
                .gaps
                .iter()
                .any(|g| g.events.is_some_and(|(lo, hi)| lo <= e && e <= hi))
        }

        /// `b` runs only after `a` (when it runs, and nothing between
        /// throws): same-block order, or dominance over normal edges.
        fn guaranteed(&self, a: EventId, b: EventId) -> bool {
            if self.covered(a) || self.covered(b) {
                return false;
            }
            let (ba, bb) = (self.block(a), self.block(b));
            if ba == bb {
                return self.sequenced(a, b);
            }
            let normal = |k: EdgeKind| k != EdgeKind::Exception;
            let entry = self.cfg.entry;
            self.reach(entry, None, normal).contains(&bb)
                && !self.reach(entry, Some(ba), normal).contains(&bb)
        }

        fn edge(&self, from: BlockId, to: BlockId, kind: EdgeKind) -> bool {
            self.cfg
                .edges
                .iter()
                .any(|e| e.from == from && e.to == to && e.kind == kind)
        }

        fn gap(&self, kind: GapKind) -> bool {
            self.cfg.gaps.iter().any(|g| g.kind == kind)
        }
    }

    #[test]
    fn straight_line_calls_are_sequenced() {
        let unit = lower("straight.c");
        let g = graph(&unit, "run");
        let (prepare, work, finish) = (g.ev("prepare"), g.ev("work"), g.ev("finish"));
        assert!(g.sequenced(prepare, work));
        assert!(g.sequenced(work, finish));
        assert!(g.guaranteed(prepare, finish));
        assert!(!g.sequenced(finish, prepare));
        assert!(g.cfg.gaps.is_empty());
    }

    #[test]
    fn sequencing_off_records_no_graphs() {
        let unit = lower_with("straight.c", PreprocessOptions::new());
        assert!(unit.cfgs.is_empty());
    }

    #[test]
    fn if_else_arms_are_exclusive() {
        let unit = lower("branches.c");
        let g = graph(&unit, "run");
        let (prepare, send, recover, finish) = (
            g.ev("prepare"),
            g.ev("send"),
            g.ev("recover"),
            g.ev("finish"),
        );
        assert!(g.exclusive(send, recover));
        assert!(g.guaranteed(prepare, finish));
        assert!(g.guaranteed(prepare, send));
        assert!(g.may_precede(send, finish));
        assert!(!g.guaranteed(send, finish));
        assert!(!g.guaranteed(recover, finish));
        assert!(!g.block_path(g.block(send), g.block(recover), None));
        assert!(!g.cfg.order.contains(&(send, recover)));
    }

    #[test]
    fn short_circuit_branches_and_arguments_are_unordered() {
        let unit = lower("shortcircuit.c");
        let g = graph(&unit, "both");
        let (a, b) = (g.ev("a"), g.ev("b"));
        assert!(g.edge(g.block(a), g.block(b), EdgeKind::BranchTrue));
        assert!(g.guaranteed(a, b));
        assert!(!g.may_precede(b, a));

        let g = graph(&unit, "args");
        let (f, gg, h) = (g.ev("f"), g.ev("g"), g.ev("h"));
        assert!(g.unordered(gg, h));
        assert!(g.sequenced(gg, f));
        assert!(g.sequenced(h, f));

        let g = graph(&unit, "sum");
        let (f1, gg, k) = (g.ev("f1"), g.ev("g"), g.ev("k"));
        assert!(g.sequenced(gg, f1));
        assert!(g.unordered(f1, k));
        assert!(g.unordered(gg, k));

        let g = graph(&unit, "comma");
        assert!(g.sequenced(g.ev("p"), g.ev("q")));
    }

    #[test]
    fn cpp17_sequences_receivers_and_assignments() {
        let unit = lower("args_order.cpp");
        let g = graph(&unit, "receiver");
        let (make, arg) = (g.ev("make"), g.ev("arg"));
        assert!(g.sequenced(make, arg));
        let g = graph(&unit, "assign");
        let (lhs, rhs) = (g.ev("lhs"), g.ev("rhs"));
        assert!(g.sequenced(rhs, lhs));
        assert!(!g.sequenced(lhs, rhs));
    }

    #[test]
    fn c_and_older_cpp_leave_receivers_and_assignments_unordered() {
        let c = lower("args_order.c");
        let cpp14 = lower_with(
            "args_order.cpp",
            PreprocessOptions::new()
                .with_call_sequencing(true)
                .with_define("__cplusplus", "201402L"),
        );
        for unit in [&c, &cpp14] {
            let g = graph(unit, "receiver");
            assert!(g.unordered(g.ev("make"), g.ev("arg")));
            let g = graph(unit, "assign");
            assert!(g.unordered(g.ev("lhs"), g.ev("rhs")));
        }
    }

    #[test]
    fn loops_keep_back_edges_and_jump_targets() {
        let unit = lower("loops.c");
        let g = graph(&unit, "for_loop");
        let (init, more, step, body) = (g.ev("init"), g.ev("more"), g.ev("step"), g.ev("body"));
        let (again, stop, skip, after) =
            (g.ev("again"), g.ev("stop_now"), g.ev("skip"), g.ev("after"));
        assert!(g.guaranteed(init, more));
        assert!(!g.may_precede(more, init));
        assert!(g.edge(g.block(step), g.block(more), EdgeKind::BackEdge));
        assert!(g.may_precede(body, step));
        assert!(g.may_precede(body, body));
        assert!(!g.guaranteed(body, after));
        // `continue` reaches the update without the rest of the body.
        assert!(g.block_path(g.block(again), g.block(step), Some(g.block(skip))));
        // `break` leaves without the update or the condition.
        assert!(g.block_path(g.block(stop), g.block(after), Some(g.block(more))));

        let g = graph(&unit, "do_loop");
        let (body, more, after) = (g.ev("body"), g.ev("more"), g.ev("after"));
        assert!(g.guaranteed(body, more));
        assert!(g.edge(g.block(more), g.block(body), EdgeKind::BackEdge));
        assert!(g.guaranteed(body, after));

        let g = graph(&unit, "while_loop");
        let (more, body, after) = (g.ev("more"), g.ev("body"), g.ev("after"));
        assert!(g.guaranteed(more, body));
        assert!(g.may_precede(body, body));
        assert!(!g.guaranteed(body, after));
        assert!(g.guaranteed(more, after));
    }

    #[test]
    fn a_recursive_call_is_an_event_of_its_own_function() {
        let unit = lower("recursion.c");
        let g = graph(&unit, "walk");
        let (walk, leaf) = (g.ev("walk"), g.ev("leaf"));
        assert!(g.may_precede(walk, leaf));
        assert!(!g.guaranteed(walk, leaf));
    }

    #[test]
    fn return_switch_and_goto_route_control() {
        let unit = lower("early_exit.c");
        let g = graph(&unit, "early");
        let (check, fail, done) = (g.ev("check"), g.ev("fail"), g.ev("done"));
        assert!(g.exclusive(fail, done));
        assert!(g.guaranteed(check, done));
        assert!(g.edge(g.block(fail), g.cfg.exit, EdgeKind::Return));

        let g = graph(&unit, "cases");
        let (a, b, c, after) = (g.ev("a"), g.ev("b"), g.ev("c"), g.ev("after"));
        assert!(g.edge(g.block(a), g.block(b), EdgeKind::Fallthrough));
        assert!(g.may_precede(a, b));
        assert!(!g.guaranteed(a, b));
        assert!(g.exclusive(b, c));
        assert!(g.exclusive(a, c));
        assert!(!g.guaranteed(b, after));

        let g = graph(&unit, "jump");
        let (check, work, cleanup) = (g.ev("check"), g.ev("work"), g.ev("cleanup"));
        assert!(g.guaranteed(check, cleanup));
        assert!(g.may_precede(work, cleanup));
        assert!(!g.guaranteed(work, cleanup));
        assert!(g.block_path(g.block(check), g.block(cleanup), Some(g.block(work))));
    }

    #[test]
    fn a_switch_without_default_may_skip_every_case() {
        let unit = lower("early_exit.c");
        let g = graph(&unit, "cases");
        let (a, after) = (g.ev("a"), g.ev("after"));
        let head = g
            .cfg
            .edges
            .iter()
            .find(|e| e.to == g.block(a) && e.kind == EdgeKind::SwitchCase)
            .unwrap()
            .from;
        assert!(g.edge(head, g.block(after), EdgeKind::SwitchCase));
    }

    #[test]
    fn macro_expansions_are_distinct_sequenced_events() {
        let unit = lower("macro_calls.c");
        let g = graph(&unit, "twice");
        // An argument spelled once and substituted twice: two invocations,
        // each its own site, at one source position.
        let steps = g.all("step");
        assert_eq!(steps.len(), 2, "{:#?}", g.cfg);
        let (first, second) = (steps[0], steps[1]);
        assert_ne!(
            g.cfg.events[first.0 as usize].sites,
            g.cfg.events[second.0 as usize].sites
        );
        assert!(g.sequenced(first, second));

        let g = graph(&unit, "logs");
        let (a, b) = (g.ev("a"), g.ev("b"));
        assert!(g.guaranteed(a, b));
        let logs = g.all("log_it");
        assert_eq!(logs.len(), 2);
        assert!(g.sequenced(a, logs[0]));
        assert!(g.sequenced(logs[0], b));
    }

    #[test]
    fn automatic_objects_are_destroyed_at_every_scope_exit() {
        let unit = lower("lifecycle.cpp");
        let g = graph(&unit, "scoped");
        let (ctor, work, dtor, after) = (
            g.ev("Guard::Guard"),
            g.ev("work"),
            g.ev("Guard::~Guard"),
            g.ev("after"),
        );
        assert_eq!(g.kind(ctor), EventKind::Construct);
        assert_eq!(g.kind(dtor), EventKind::Destroy);
        assert!(g.guaranteed(ctor, work));
        assert!(g.guaranteed(work, dtor));
        assert!(g.guaranteed(dtor, after));

        let g = graph(&unit, "early");
        let (fail, work) = (g.ev("fail"), g.ev("work"));
        let dtors = g.all("Guard::~Guard");
        assert_eq!(dtors.len(), 2, "{:#?}", g.cfg);
        // One destructor event on the `return` path, one after `work`.
        let (on_return, at_end) = if g.may_precede(work, dtors[0]) {
            (dtors[1], dtors[0])
        } else {
            (dtors[0], dtors[1])
        };
        assert!(g.guaranteed(work, at_end));
        assert!(g.exclusive(work, on_return));
        assert!(g.guaranteed(fail, on_return));
        // Both events stand for the one destructor site.
        assert_eq!(
            g.cfg.events[dtors[0].0 as usize].sites,
            g.cfg.events[dtors[1].0 as usize].sites
        );

        let g = graph(&unit, "loop");
        let use_ = g.ev("use");
        let element = g.ev_at("Item::~Item", 37);
        assert!(g.sequenced(use_, element), "{}", g.dump());
        assert!(g.may_precede(element, use_));
        let array = g.ev_at("Item::~Item", 36);
        assert!(g.may_precede(element, array));
        assert!(!g.guaranteed(element, array));
        assert!(!g.may_precede(array, element));

        let g = graph(&unit, "handler");
        let (recover, caught) = (g.ev("recover"), g.ev("Item::~Item"));
        assert!(g.guaranteed(recover, caught));
    }

    #[test]
    fn each_declarator_constructs_before_the_next_is_evaluated() {
        let unit = lower("lifecycle.cpp");
        let g = graph(&unit, "two");
        let (first, second) = (g.ev("first"), g.ev("second"));
        let ctors = g.all("Pair::Pair");
        assert_eq!(ctors.len(), 2, "{}", g.dump());
        let (a, b) = (ctors[0], ctors[1]);
        assert!(g.sequenced(first, a), "{}", g.dump());
        assert!(g.sequenced(a, second));
        assert!(g.sequenced(second, b));
        // Destroyed in reverse order of construction.
        let dtors = g.all("Pair::~Pair");
        assert_eq!(dtors.len(), 2);
        assert!(g.sequenced(b, dtors[0]));
        assert!(g.sequenced(dtors[0], dtors[1]));
        assert_eq!(
            g.cfg.events[dtors[0].0 as usize].occurrence,
            g.cfg.events[b.0 as usize].occurrence
        );
    }

    #[test]
    fn try_bodies_have_exception_edges_to_their_landing() {
        let unit = lower("exceptions.cpp");
        let g = graph(&unit, "guarded");
        let (a, b, h, c) = (g.ev("a"), g.ev("b"), g.ev("h"), g.ev("c"));
        let landing = g
            .cfg
            .edges
            .iter()
            .find(|e| e.from == g.block(a) && e.kind == EdgeKind::Exception)
            .expect("exception edge")
            .to;
        assert_eq!(
            g.cfg.blocks[landing.0 as usize].kind,
            trace_ir::BlockKind::Landing
        );
        assert!(g.sequenced(a, b));
        assert!(g.may_precede(a, h));
        assert!(!g.may_precede(h, a));
        assert!(!g.guaranteed(a, h));
        assert!(g.may_precede(h, c));

        let g = graph(&unit, "thrower");
        let (before, dead) = (g.ev("before"), g.ev("dead"));
        assert!(g.exclusive(before, dead));
        let exceptional = g.cfg.exceptional_exit.expect("exceptional exit");
        assert!(g.edge(g.block(before), exceptional, EdgeKind::Exception));
    }

    #[test]
    fn a_parse_error_keeps_its_events_unordered_under_a_gap() {
        let unit = lower("parse_error.c");
        let g = graph(&unit, "broken");
        assert!(g.gap(GapKind::ParseError), "{:#?}", g.cfg);
        let (a, d, e, c) = (g.ev("a"), g.ev("d"), g.ev("e"), g.ev("c"));
        // The misparsed call is kept, under the gap.
        assert!(g.covered(d), "{}", g.dump());
        assert!(!g.guaranteed(d, e));
        assert!(!g.covered(a) && !g.covered(c));
        assert!(g.guaranteed(a, c));
    }

    #[test]
    fn a_recovered_statement_keeps_its_place() {
        let unit = lower("recovery.cpp");
        let g = graph(&unit, "declared");
        let (before, factory, value) = (g.ev("before"), g.ev("factory"), g.ev("value"));
        assert!(g.guaranteed(before, factory), "{}", g.dump());
        assert!(g.guaranteed(factory, value));
    }

    #[test]
    fn member_initializers_run_first_in_no_modeled_order() {
        let unit = lower("prologue.cpp");
        let g = graph(&unit, "Derived::Derived");
        let (f, gg, base, body) = (g.ev("f"), g.ev("g"), g.ev("Base::Base"), g.ev("body"));
        for e in [f, gg, base] {
            assert!(g.covered(e), "{}", g.dump());
            assert!(g.may_precede(e, body));
            assert!(!g.may_precede(body, e));
        }
        assert!(!g.covered(body));
        assert!(g.gap(GapKind::InitializerOrder));
    }

    #[test]
    fn a_default_member_initializer_runs_first_under_a_gap() {
        // Written in the class, outside the body and the initializer list.
        let unit = lower("prologue.cpp");
        let g = graph(&unit, "Owner::Owner");
        let (seed, body) = (g.ev("seed"), g.ev("body"));
        assert!(g.covered(seed), "{}", g.dump());
        assert!(g.may_precede(seed, body));
        assert!(!g.may_precede(body, seed));
        assert!(!g.guaranteed(seed, body));
    }

    fn covered_by(g: &Graph, e: EventId, kind: GapKind) -> bool {
        g.cfg
            .gaps
            .iter()
            .any(|gap| gap.kind == kind && gap.events.is_some_and(|(lo, hi)| lo <= e && e <= hi))
    }

    #[test]
    fn unsupported_constructs_are_gaps_not_order() {
        let unit = lower("unsupported.c");
        let g = graph(&unit, "statement_expression");
        let (inner, k) = (g.ev("g"), g.ev("k"));
        assert!(
            covered_by(&g, inner, GapKind::UnsupportedConstruct),
            "{}",
            g.dump()
        );
        assert!(!g.guaranteed(inner, k));

        let g = graph(&unit, "computed");
        let (a, b) = (g.ev("a"), g.ev("b"));
        assert!(g.gap(GapKind::UnsupportedConstruct), "{}", g.dump());
        assert!(g.edge(g.block(a), g.cfg.exit, EdgeKind::Unsupported));
        assert!(g.exclusive(a, b));

        let g = graph(&unit, "unresolved");
        let a = g.ev("a");
        assert!(g.gap(GapKind::UnresolvedGoto), "{}", g.dump());
        assert!(g.edge(g.block(a), g.cfg.exit, EdgeKind::Unsupported));
    }

    #[test]
    fn a_branch_inside_unordered_operands_is_a_gap() {
        let unit = lower("unsupported.c");
        let g = graph(&unit, "branchy");
        let (a, b, c, f) = (g.ev("a"), g.ev("b"), g.ev("c"), g.ev("f"));
        for e in [a, b, c] {
            assert!(covered_by(&g, e, GapKind::UnorderedBranch), "{}", g.dump());
        }
        assert!(!g.guaranteed(a, c));
        assert!(!g.covered(f) || !g.guaranteed(c, f));
    }

    #[test]
    fn setjmp_and_longjmp_are_unsupported() {
        let unit = lower("unsupported.c");
        let g = graph(&unit, "jumps");
        let (set, jump) = (g.ev("setjmp"), g.ev("longjmp"));
        assert!(
            covered_by(&g, set, GapKind::UnsupportedConstruct),
            "{}",
            g.dump()
        );
        assert!(covered_by(&g, jump, GapKind::UnsupportedConstruct));
        assert!(!g.guaranteed(set, jump));
    }

    #[test]
    fn jumps_out_of_a_scope_destroy_its_objects() {
        let unit = lower("scopes.cpp");
        let g = graph(&unit, "jumps");
        let (quit, work, after) = (g.ev("quit"), g.ev("work"), g.ev("after"));
        let dtors = g.all("Guard::~Guard");
        assert_eq!(dtors.len(), 2, "{}", g.dump());
        // One destructor on the `goto` path, one at the block's end.
        assert!(dtors
            .iter()
            .any(|&d| g.exclusive(d, work) && g.guaranteed(quit, d)));
        assert!(dtors.iter().any(|&d| g.guaranteed(work, d)));
        for &d in &dtors {
            assert!(g.may_precede(d, after));
        }

        let g = graph(&unit, "loops");
        let (skip, quit, work) = (g.ev("skip"), g.ev("quit"), g.ev("work"));
        let dtors = g.all("Guard::~Guard");
        // `continue`, `break` and the end of the body each leave the scope.
        assert_eq!(dtors.len(), 3, "{}", g.dump());
        let (more, after) = (g.ev("more"), g.ev("after"));
        let at_end: Vec<_> = dtors.iter().filter(|&&d| g.guaranteed(work, d)).collect();
        let on_continue: Vec<_> = dtors
            .iter()
            .filter(|&&d| g.guaranteed(skip, d) && !g.guaranteed(quit, d))
            .collect();
        let on_break: Vec<_> = dtors
            .iter()
            .filter(|&&d| g.guaranteed(quit, d) && !g.guaranteed(work, d))
            .collect();
        assert_eq!((at_end.len(), on_continue.len(), on_break.len()), (1, 1, 1));
        let (on_continue, on_break) = (*on_continue[0], *on_break[0]);
        assert!(g.block_path(g.block(on_continue), g.block(more), Some(g.block(work))));
        assert!(g.block_path(g.block(on_break), g.block(after), Some(g.block(more))));
    }

    #[test]
    fn new_constructs_and_delete_destroys() {
        let unit = lower("scopes.cpp");
        let g = graph(&unit, "heap");
        let (value, ctor, dtor) = (g.ev("value"), g.ev("Node::Node"), g.ev("Node::~Node"));
        assert_eq!(g.kind(ctor), EventKind::Construct);
        assert_eq!(g.kind(dtor), EventKind::Delete);
        assert!(g.sequenced(value, ctor), "{}", g.dump());
        assert!(g.sequenced(ctor, dtor));
    }

    #[test]
    fn a_lambda_body_is_its_own_graph() {
        let unit = lower("lambda.cpp");
        let g = graph(&unit, "outer");
        let (before, seed, after) = (g.ev("before"), g.ev("seed"), g.ev("after"));
        assert!(g.all("inside").is_empty(), "{}", g.dump());
        assert!(g.sequenced(before, seed));
        assert!(g.sequenced(seed, after));
        let lambda = unit
            .functions
            .iter()
            .find(|f| f.name.contains("$lambda"))
            .unwrap()
            .name
            .clone();
        let inner = graph(&unit, &lambda);
        inner.ev("inside");
        assert!(inner.all("seed").is_empty());
    }

    #[test]
    fn an_exception_no_handler_catches_leaves_the_function() {
        let unit = lower("exceptions.cpp");
        let g = graph(&unit, "typed");
        let a = g.ev("a");
        let landing = g
            .cfg
            .edges
            .iter()
            .find(|e| e.from == g.block(a) && e.kind == EdgeKind::Exception)
            .unwrap()
            .to;
        let exceptional = g.cfg.exceptional_exit.expect("exceptional exit");
        assert!(
            g.edge(landing, exceptional, EdgeKind::Exception),
            "{}",
            g.dump()
        );

        let g = graph(&unit, "guarded");
        assert!(g.cfg.exceptional_exit.is_none());
    }

    #[test]
    fn nesting_past_the_walk_depth_is_a_gap() {
        // Indexing runs on threads with this stack (`index_pool`).
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("deep.c");
                let depth = MAX_AST_WALK_DEPTH as usize + 10;
                let source = format!(
                    "int g(void);\nint k(void);\nint deep(void)\n{{\n    k();\n    return {}g(){};\n}}\n",
                    "(".repeat(depth),
                    ")".repeat(depth)
                );
                std::fs::write(&path, source).unwrap();
                let graph_ = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &[]);
                let unit = index_source_file(
                    &path,
                    dir.path(),
                    &graph_,
                    &PreprocessOptions::new().with_call_sequencing(true),
                    &IndexSourceCache::new(),
                    None,
                    &HeaderOrder::default(),
                );
                let g = graph(&unit, "deep");
                assert!(g.gap(GapKind::DepthExceeded), "{}", g.dump());
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
