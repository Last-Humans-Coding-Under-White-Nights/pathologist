# Architectural Proposals for Scalable Memory Optimization

This document details two major architectural proposals for fundamentally scaling `trace` to very large multi-million-line codebases by shifting away from monolithic in-memory accumulation.

---

## Proposal 1: Streamed & Incremental PAG Construction

### 1. Problem Statement
Currently, `trace` operates on an in-memory accumulation model:
1. Translation units (TUs) are parsed and lowered in parallel.
2. All lowered `UnitIndex` structures merge into a single, global `Program` structure.
3. At the end of the merge phase, `Program` retains:
   - Full symbol tables (functions, local variables, parameters, globals).
   - All call sites (over 100,000 in medium projects).
   - All statement-level pointer flow constraints (`FlowConstraint::Copy`, `Load`, `Store`, `GepField`).
   - All function return summaries (`ReturnFlow`).
4. During PAG construction (`trace-analysis/src/pag.rs`), `build_pag` walks the entire `Program` and allocates the Pointer Assignment Graph (`Pag`) with hundreds of thousands of nodes and edges.

**Resulting Bottleneck**:
- `Program` and `Pag` coexist simultaneously in RAM during PAG construction, creating the peak memory spike of the entire run.
- Statement-level `FlowConstraint` facts are only used *once* to populate `PagEdge`s, and are never exported to SQLite (only `flow_nodes` and `flow_edges` are exported). Holding them in memory across the entire run is unnecessary.

### 2. Proposed Architecture: Two-Tier Execution

Split the IR into two decoupled tiers:

```
                  TU Lowering (Workers)
                  ┌────────────────────┐
                  │ Parse AST & Lower  │
                  └─────────┬──────────┘
                            │
          ┌─────────────────┴─────────────────┐
          ▼                                   ▼
 [Tier 1: Global Skeleton]           [Tier 2: Ephemeral Bodies]
 • Types                             • Local variables
 • Function prototypes               • Statement FlowConstraints
 • Class inheritance (CHA)           • CallSites & ReturnFlows
 • Global variables                            │
          │                                    ▼
          │                          [Append-Only Scratch File]
          │                          (Binary, streamed per TU)
          │                                    │
          └─────────────────┬──────────────────┘
                            ▼
               [Streaming PAG Construction]
               • Sequential stream read of constraints
               • Populates PagNodes & PagEdges
               • Resolves CallReturn via Skeleton
               • Drops scratch file on completion
                            │
                            ▼
                    [Andersen Solver]
                            │
                            ▼
                     [SQLite Export]
```

#### Tier 1: Global Program Skeleton (Resides in RAM, ~15–20 MiB)
Contains only global declarations:
- `TypeTable`: All canonical types.
- `SymbolTable`: Function signatures (`FnId`, names, parameter types, linkage), global variables, and link targets.
- Class hierarchy: `Program.inheritance`, `template_bases`, and virtual method tables.

#### Tier 2: Statement Flow & Call Sites (Streamed to Disk)
- As each translation unit lowers its function bodies, it writes its statement-level flow constraints (`Copy`, `Load`, `Store`, `GepField`), `CallSite` records, and `ReturnFlow` entries into an append-only binary scratch file via `BufWriter`.
- As soon as a TU is written to the scratch file, its local AST facts, temporary variables, and constraint vectors are immediately freed from RAM.
- In-flight memory during lowering is strictly bounded by active workers (e.g. 16 workers in flight) rather than the total number of translation units in the corpus.

#### Streaming PAG Build
- Once the global skeleton is finalized (prototypes merged, virtual dispatch overrides established):
  - `build_pag` sequentially streams the binary scratch file using a buffered reader (`BufReader`).
  - For each `Copy(dst, src)` opcode $\rightarrow$ directly inserts `PagEdge::Copy(dst, src)`.
  - For each `CallReturn(dst, callee)` opcode $\rightarrow$ resolves callee against the Tier 1 skeleton and wires return flow.
  - The scratch file is closed and immediately unlinked.

### 3. Expected Impact
- **Peak RSS Reduction**: **~150–250 MiB** on OpenHarmony corpora.
- **I/O Overhead**: Sequential streaming of 50–100 MB of compact binary bytecodes takes ~15–30 ms on modern SSDs or Linux tmpfs, adding zero noticeable latency.
- **Phase Boundary Compliance**: Preserves the rule from `AGENTS.md` that IR and Analysis do not depend on SQLite; uses a private binary tempfile.

---

## Proposal 2: Global String Interning & Identifier Compaction

### 1. Problem Statement
Currently, names and identifiers are represented as individual, owned `String` instances across all IR structures:
- `Variable.name: String`
- `Function.name: String`
- `CallSite.callee_name: String`
- `CallSite.receiver_class: Option<String>`
- `FlowConstraint.field_name: String`
- `FlowConstraint.callee_name: String`
- `FlowConstraint.value: String`
- Keys in `SymbolTable` maps (`fn_by_name`, `externals_by_name`, `base_by_name`, `fn_by_scope`, etc.)

**Resulting Bottlenecks**:
1. **Redundant Allocation**: Common identifiers like `"this"`, `"ret"`, `"size"`, `"push_back"`, and `"printf"` are allocated tens of thousands of times on the heap.
2. **Key Duplication**: Qualified C++ names (e.g., `OHOS::CameraStandard::CameraManager::CreateCaptureSession`) are cloned up to 5 times into different lookup maps in `SymbolTable`.
3. **Allocator Metadata**: Each `String` heap allocation carries glibc allocator chunk headers (8–16 bytes each), fragmenting heap arenas across parallel threads.
4. **Hashing Overhead**: String hashing and `strcmp` dominate symbol lookup profiles.

### 2. Proposed Architecture: Unified `StringInterner` & `IdentId`

```rust
/// 32-bit copyable token representing a deduplicated string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdentId(pub u32);

/// Append-only contiguous byte arena with hash lookup.
pub struct StringInterner {
    arena: String,                      // Contiguous string storage
    spans: Vec<(u32, u32)>,             // IdentId -> (offset, len) in arena
    lookup: rustc_hash::FxHashMap<&'static str, IdentId>,
}
```

#### IR Migration
- Replace `String` with `IdentId` in:
  - `Variable.name: IdentId` (24 bytes $\rightarrow$ 4 bytes)
  - `Function.name: IdentId` (24 bytes $\rightarrow$ 4 bytes)
  - `CallSite.callee_name: IdentId` (24 bytes $\rightarrow$ 4 bytes)
  - `CallSite.receiver_class: Option<IdentId>` (24 bytes $\rightarrow$ 4 bytes)
  - `FlowConstraint.field_name: IdentId` (24 bytes $\rightarrow$ 4 bytes)
- Map keys in `SymbolTable`:
  - `fn_by_name: IndexMap<IdentId, FnId>`
  - `externals_by_name: FxHashMap<IdentId, Vec<FnId>>`
  - `base_by_name: FxHashMap<IdentId, Vec<FnId>>`

#### Performance & Memory Benefits
1. **Heap Allocation Drop**: Eliminates over 300,000 individual heap allocations across a medium-sized codebase.
2. **Constant-Time Lookups**: Comparing `IdentId` is a single CPU register `cmp` instruction (`id1 == id2`), completely bypassing string hashing and character comparisons during symbol lookup.
3. **Cache Footprint**: Hash map buckets store 4-byte integers instead of 24-byte `String` fat pointers, increasing L1/L2 CPU cache utilization by 3–4×.
4. **Direct Memory Reduction**: Estimated **~80–120 MiB RSS** reduction across lowering, merge, and analysis.

---

## 3. Recommended Implementation Roadmap

1. **Phase 1 (Immediate - Low Risk)**:
   - Compact in-memory representations: replace `Vec<T>` with `Box<[T]>` in `CallSite` where vectors are rarely resized after creation.
   - Use `Box<str>` in `FlowConstraint` and `Diagnostic` to reduce enum discriminant padding.
2. **Phase 2 (Medium Effort)**:
   - Introduce `StringInterner` and convert `FlowConstraint` and `CallSite` to use `IdentId`.
   - Update `SymbolTable` maps to key on `IdentId`.
3. **Phase 3 (Major Architecture)**:
   - Implement Tier 1 (Skeleton) vs Tier 2 (Scratch Stream) lowering.
   - Streamline PAG construction to ingest the scratch stream directly.
