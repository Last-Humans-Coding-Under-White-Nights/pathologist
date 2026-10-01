//! Ordered, versioned snapshots of lowered units before the global merge.
use crate::link_commands::LinkDatabase;
use crate::merge::{self, UnitIndex};
use crate::IncludeGraph;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use trace_ir::{Diagnostic, IdlInterface, Program, TestPartition};

const FORMAT_VERSION: u32 = 2;
const MAGIC: &[u8; 8] = b"\x89TRIDX\r\n";

#[derive(Serialize, Deserialize)]
struct Header {
    frontend_version: String,
    root: PathBuf,
    inference_root: PathBuf,
    dep_roots: Vec<PathBuf>,
    test_partition: TestPartition,
    include_paths: Vec<PathBuf>,
    defines: IndexMap<String, String>,
    idl_interfaces: Vec<IdlInterface>,
    diagnostics: Vec<Diagnostic>,
    explore: bool,
    explore_budget: usize,
    ignored_macros: Vec<String>,
}

impl Header {
    fn capture(program: &Program, graph: &IncludeGraph) -> Self {
        Self {
            frontend_version: env!("CARGO_PKG_VERSION").into(),
            root: program.root.clone(),
            inference_root: graph.root.clone(),
            dep_roots: program.dep_roots().to_vec(),
            test_partition: graph.test_partition.clone(),
            include_paths: program.include_paths.clone(),
            defines: program.defines.clone(),
            idl_interfaces: program.idl_interfaces.clone(),
            diagnostics: program.diagnostics.clone(),
            explore: program.explore,
            explore_budget: program.explore_budget,
            ignored_macros: program.ignored_macros.clone(),
        }
    }

    fn into_program(self) -> Program {
        let mut program = Program::new(self.root);
        program.symbols.set_dep_roots(self.dep_roots);
        program
            .symbols
            .set_inference_root(&self.inference_root, self.test_partition);
        program.include_paths = self.include_paths;
        program.defines = self.defines;
        program.idl_interfaces = self.idl_interfaces;
        program.diagnostics = self.diagnostics;
        program.explore = self.explore;
        program.explore_budget = self.explore_budget;
        program.ignored_macros = self.ignored_macros;
        program
    }
}

// U is a borrowed slice when writing and owned units when reading. A family
// stays together for configuration deduplication and image/weak selection.
#[derive(Serialize, Deserialize)]
enum Record<U> {
    Header(Box<Header>),
    Units {
        mode: UnitMode,
        units: U,
    },
    Linked {
        links: LinkDatabase,
        units: U,
    },
    LinkTargets(LinkDatabase),
    CompleteTypes,
    VariantCount(usize),
    Diagnostics {
        files: Vec<PathBuf>,
        diagnostics: Vec<Diagnostic>,
    },
    End {
        include_deps: Vec<(PathBuf, PathBuf)>,
        extra_dirs: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum UnitMode {
    Full,
    Symbols,
    HeaderPreamble,
    Family,
}

pub(crate) struct SnapshotWriter {
    output: PathBuf,
    file: BufWriter<tempfile::NamedTempFile>,
    started: bool,
    error: Option<String>,
}

impl SnapshotWriter {
    fn write(&mut self, record: &Record<&[UnitIndex]>) {
        self.write_record(record);
    }

    fn write_record(&mut self, record: &impl Serialize) {
        if self.error.is_some() {
            return;
        }
        let result = (|| -> Result<(), String> {
            let prefix = self.file.stream_position().map_err(|e| e.to_string())?;
            self.file
                .write_all(&0u64.to_le_bytes())
                .map_err(|e| e.to_string())?;
            record
                .serialize(&mut rmp_serde::Serializer::new(&mut self.file))
                .map_err(|e| e.to_string())?;
            let end = self.file.stream_position().map_err(|e| e.to_string())?;
            self.file
                .seek(SeekFrom::Start(prefix))
                .map_err(|e| e.to_string())?;
            self.file
                .write_all(&(end - prefix - 8).to_le_bytes())
                .map_err(|e| e.to_string())?;
            self.file
                .seek(SeekFrom::Start(end))
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(format!("write index {}: {error}", self.output.display()));
        }
    }

    fn start(&mut self, program: &Program, graph: &IncludeGraph) {
        if !self.started {
            self.write(&Record::Header(Box::new(Header::capture(program, graph))));
            self.started = true;
        }
    }

    fn publish(mut self) -> Result<(), String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.file.flush().map_err(|e| e.to_string())?;
        let file = self.file.into_inner().map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&self.output)
            .map_err(|e| format!("publish index {}: {e}", self.output.display()))?;
        Ok(())
    }
}

/// The frontend shares one merge schedule between direct execution and disk
/// output. Lowering's own per-unit/header merges are unaffected.
pub(crate) enum IndexOutput {
    Program,
    Snapshot(SnapshotWriter),
}

impl IndexOutput {
    pub(crate) fn snapshot(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| format!("create index {}: {e}", path.display()))?;
        let mut file = BufWriter::new(file);
        file.write_all(MAGIC).map_err(|e| e.to_string())?;
        file.write_all(&FORMAT_VERSION.to_le_bytes())
            .map_err(|e| e.to_string())?;
        Ok(Self::Snapshot(SnapshotWriter {
            output: path.to_path_buf(),
            file,
            started: false,
            error: None,
        }))
    }

    pub(crate) fn begin(&mut self, program: &Program, graph: &IncludeGraph) {
        if let Self::Snapshot(writer) = self {
            writer.start(program, graph);
        }
    }

    pub(crate) fn units(
        &mut self,
        program: &mut Program,
        graph: &IncludeGraph,
        mode: UnitMode,
        units: &[UnitIndex],
    ) {
        match self {
            Self::Program => merge_units(program, mode, units),
            Self::Snapshot(writer) => {
                writer.start(program, graph);
                writer.write(&Record::Units { mode, units });
            }
        }
    }

    pub(crate) fn family(
        &mut self,
        program: &mut Program,
        graph: &IncludeGraph,
        base: &UnitIndex,
        variants: &[UnitIndex],
    ) {
        match self {
            Self::Program => merge::merge_unit_variants(program, base, variants),
            Self::Snapshot(writer) => {
                writer.start(program, graph);
                let units: Vec<_> = std::iter::once(base).chain(variants).collect();
                writer.write_record(&Record::Units {
                    mode: UnitMode::Family,
                    units,
                });
            }
        }
    }

    pub(crate) fn linked(
        &mut self,
        program: &mut Program,
        graph: &IncludeGraph,
        units: &[UnitIndex],
        links: &LinkDatabase,
    ) {
        match self {
            Self::Program => merge::merge_linked_units(program, units, links),
            Self::Snapshot(writer) => {
                writer.start(program, graph);
                writer.write(&Record::Linked {
                    links: links.clone(),
                    units,
                });
            }
        }
    }

    pub(crate) fn link_targets(
        &mut self,
        program: &mut Program,
        graph: &IncludeGraph,
        links: &LinkDatabase,
    ) {
        match self {
            Self::Program => merge::record_link_targets(program, links),
            Self::Snapshot(writer) => {
                writer.start(program, graph);
                writer.write(&Record::LinkTargets(links.clone()));
            }
        }
    }

    pub(crate) fn complete_types(&mut self, program: &mut Program) {
        match self {
            Self::Program => program.types.complete_nested_tags(),
            Self::Snapshot(writer) => writer.write(&Record::CompleteTypes),
        }
    }

    pub(crate) fn variant_count(&mut self, program: &mut Program, count: usize) {
        match self {
            Self::Program => program.variants_merged = count,
            Self::Snapshot(writer) => writer.write(&Record::VariantCount(count)),
        }
    }

    pub(crate) fn diagnostics(&mut self, program: &mut Program, unit: Program) {
        let files: Vec<_> = unit.symbols.files.iter().map(|f| f.path.clone()).collect();
        match self {
            Self::Program => merge_diagnostics(program, &files, unit.diagnostics),
            Self::Snapshot(writer) => writer.write(&Record::Diagnostics {
                files,
                diagnostics: unit.diagnostics,
            }),
        }
    }

    pub(crate) fn finish(
        self,
        program: &mut Program,
        graph: &IncludeGraph,
        extra_dirs: Vec<PathBuf>,
    ) -> Result<(), String> {
        match self {
            Self::Program => crate::lower::finalize_program(program, graph, extra_dirs),
            Self::Snapshot(mut writer) => {
                writer.start(program, graph);
                writer.write(&Record::End {
                    include_deps: graph.edge_list(),
                    extra_dirs,
                });
                writer.publish()?;
            }
        }
        Ok(())
    }
}

fn merge_units(program: &mut Program, mode: UnitMode, units: &[UnitIndex]) {
    if let UnitMode::Family = mode {
        if let Some((base, variants)) = units.split_first() {
            merge::merge_unit_variants(program, base, variants);
        }
    } else {
        for unit in units {
            match mode {
                UnitMode::Full => merge::merge_unit_index(program, unit),
                UnitMode::Symbols => merge::merge_unit_symbols(program, unit),
                UnitMode::HeaderPreamble => merge::merge_unit_header_preamble(program, unit),
                UnitMode::Family => unreachable!(),
            }
        }
    }
}

fn merge_diagnostics(program: &mut Program, files: &[PathBuf], diagnostics: Vec<Diagnostic>) {
    let remap: Vec<_> = files
        .iter()
        .map(|p| program.symbols.add_file_interned(p))
        .collect();
    for mut diagnostic in diagnostics {
        diagnostic.file = diagnostic.file.map(|id| remap[id.0 as usize]);
        if program.dedup.insert_diagnostic(
            diagnostic.file,
            diagnostic.line,
            &diagnostic.message,
            &diagnostic.stage,
        ) {
            program.add_diagnostic(diagnostic);
        }
    }
}

fn prepare_units(units: &mut [UnitIndex]) -> Result<(), String> {
    for unit in units {
        let valid_file = |id: trace_ir::FileId| (id.0 as usize) < unit.files.len();
        let functions: rustc_hash::FxHashSet<_> = unit.functions.iter().map(|f| f.id).collect();
        let variables: rustc_hash::FxHashSet<_> = unit.variables.iter().map(|v| v.id).collect();
        let sites: rustc_hash::FxHashSet<_> = unit.call_sites.iter().map(|c| c.id).collect();
        let valid_fn = |id: trace_ir::FnId| functions.contains(&id);
        let valid_var = |id: trace_ir::VarId| variables.contains(&id);
        let valid_type = |id: trace_ir::TypeId| (id.0 as usize) < unit.types.all().len();
        let valid_span = |s: trace_ir::Span| valid_file(s.file);
        let valid_range = |r: &std::ops::Range<usize>| r.start <= r.end && r.end <= unit.flow.len();
        let valid_return = |r: &trace_ir::ReturnFlow| {
            r.var().is_none_or(valid_var)
                && match r {
                    trace_ir::ReturnFlow::AddrOfFn { callee } => valid_fn(*callee),
                    _ => true,
                }
        };
        if functions.len() != unit.functions.len()
            || variables.len() != unit.variables.len()
            || sites.len() != unit.call_sites.len()
            || unit.functions.iter().any(|f| {
                !valid_file(f.file)
                    || !valid_span(f.span)
                    || !valid_type(f.return_type)
                    || f.param_type_ids.iter().any(|t| !valid_type(*t))
                    || f.tu.is_some_and(|file| !valid_file(file))
                    || f.params.iter().chain(&f.locals).any(|v| !valid_var(*v))
            })
            || unit.variables.iter().any(|v| {
                !valid_span(v.span)
                    || !valid_type(v.type_id)
                    || v.fn_id.is_some_and(|f| !valid_fn(f))
            })
            || unit.call_sites.iter().any(|c| {
                !valid_fn(c.caller)
                    || !valid_span(c.span)
                    || c.expansion_span.is_some_and(|s| !valid_span(s))
                    || c.occurrence.is_some_and(|o| {
                        !valid_span(o.span) || o.expansion_span.is_some_and(|s| !valid_span(s))
                    })
                    || c.callee_fn_id.is_some_and(|f| !valid_fn(f))
                    || c.callee_var.is_some_and(|v| !valid_var(v))
                    || c.return_dst.is_some_and(|v| !valid_var(v))
                    || c.tu.is_some_and(|f| !valid_file(f))
                    || c.var_args.iter().any(|(_, v)| !valid_var(*v))
                    || c.fn_args.iter().any(|(_, f)| !valid_fn(*f))
            })
            || unit.flow.iter().any(|flow| {
                flow.vars().any(|v| !valid_var(v))
                    || match flow {
                        trace_ir::FlowConstraint::AddrOfFn { callee, .. }
                        | trace_ir::FlowConstraint::ArrayFnMember { callee, .. } => {
                            !valid_fn(*callee)
                        }
                        trace_ir::FlowConstraint::CallReturn { caller, .. } => {
                            caller.is_some_and(|f| !valid_fn(f))
                        }
                        _ => false,
                    }
            })
            || unit
                .fn_returns
                .iter()
                .any(|(f, flows)| !valid_fn(*f) || flows.iter().any(|r| !valid_return(r)))
            || unit
                .function_flow_ranges
                .iter()
                .any(|(f, ranges)| !valid_fn(*f) || ranges.iter().any(|r| !valid_range(r)))
            || unit
                .global_initializer_ranges
                .iter()
                .any(|(v, ranges)| !valid_var(*v) || ranges.iter().any(|r| !valid_range(r)))
            || unit
                .member_declarations
                .iter()
                .any(|(f, files)| !valid_fn(*f) || files.iter().any(|f| !valid_file(*f)))
            || unit.internal_definitions.keys().any(|f| !valid_fn(*f))
            || unit
                .diagnostics
                .iter()
                .any(|d| d.file.is_some_and(|f| !valid_file(f)))
            || unit
                .template_bases
                .iter()
                .any(|b| b.anonymous_in.is_some_and(|f| !valid_file(f)))
            || unit
                .anonymous_classes
                .values()
                .chain(unit.anonymous_final_classes.values())
                .flatten()
                .any(|f| !valid_file(*f))
            || unit
                .anonymous_bases
                .values()
                .flatten()
                .any(|(f, _)| !valid_file(*f))
            || unit
                .anonymous_class_templates
                .values()
                .flat_map(|v| v.keys())
                .any(|f| !valid_file(*f))
            || (!unit.merge_descs.is_empty() && unit.merge_descs.len() != unit.types.all().len())
        {
            return Err(format!(
                "invalid unit-local IDs or flow ranges in {}",
                unit.path.display()
            ));
        }
        for desc in &mut unit.merge_descs {
            *desc = unit.types.share_desc(desc);
        }
    }
    Ok(())
}

fn validate_links(links: &LinkDatabase) -> Result<(), String> {
    if links
        .targets
        .iter()
        .flat_map(|t| &t.dependencies)
        .any(|i| *i >= links.targets.len())
    {
        return Err("invalid link-target dependency".into());
    }
    Ok(())
}

/// Read lowered units, merge them with the ordinary policies, and finalize a
/// Program. No preprocessing, parsing, or source-file reads are performed.
pub fn read_index(path: &Path) -> Result<Program, String> {
    let result = read_inner(path);
    trace_ir::release_thread_path_caches();
    crate::memory::reclaim_unused_pages();
    result.map_err(|e| format!("read index {}: {e}", path.display()))
}

fn read_inner(path: &Path) -> Result<Program, String> {
    trace_ir::start_file_probe_epoch();
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut remaining = file.metadata().map_err(|e| e.to_string())?.len();
    let mut reader = BufReader::new(file);
    let mut preamble = [0u8; 12];
    reader
        .read_exact(&mut preamble)
        .map_err(|e| format!("incomplete index header: {e}"))?;
    if &preamble[..8] != MAGIC
        || u32::from_le_bytes(preamble[8..].try_into().unwrap()) != FORMAT_VERSION
    {
        return Err("unsupported index format; rebuild the index".into());
    }
    remaining -= 12;
    let header = match read_record::<Record<Vec<UnitIndex>>>(&mut reader, &mut remaining)? {
        Some(Record::Header(header)) => header,
        _ => return Err("missing index header; rebuild the index".into()),
    };
    let mut program = header.into_program();
    while let Some(record) = read_record::<Record<Vec<UnitIndex>>>(&mut reader, &mut remaining)? {
        match record {
            Record::Units { mode, mut units } => {
                prepare_units(&mut units)?;
                merge_units(&mut program, mode, &units);
            }
            Record::Linked { links, mut units } => {
                prepare_units(&mut units)?;
                validate_links(&links)?;
                merge::merge_linked_units(&mut program, &units, &links);
            }
            Record::LinkTargets(links) => {
                validate_links(&links)?;
                merge::record_link_targets(&mut program, &links);
            }
            Record::CompleteTypes => program.types.complete_nested_tags(),
            Record::VariantCount(count) => program.variants_merged = count,
            Record::Diagnostics { files, diagnostics } => {
                if diagnostics
                    .iter()
                    .any(|d| d.file.is_some_and(|f| f.0 as usize >= files.len()))
                {
                    return Err("invalid diagnostic file ID".into());
                }
                merge_diagnostics(&mut program, &files, diagnostics);
            }
            Record::End {
                include_deps,
                extra_dirs,
            } => {
                if remaining != 0 {
                    return Err("data after index end marker".into());
                }
                crate::lower::finalize_index_metadata(&mut program, include_deps, extra_dirs);
                crate::lower::finalize_merged_program(&mut program);
                return Ok(program);
            }
            Record::Header(_) => return Err("duplicate index header".into()),
        }
    }
    Err("incomplete index: missing end marker".into())
}

// Frame lengths bound the decoder to one record and detect both truncation and
// extra payload bytes. The decoder reads from the file, without a frame buffer.
fn read_record<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
    remaining: &mut u64,
) -> Result<Option<T>, String> {
    if *remaining == 0 {
        return Ok(None);
    }
    if *remaining < 8 {
        return Err("incomplete index frame length".into());
    }
    let mut prefix = [0u8; 8];
    reader.read_exact(&mut prefix).map_err(|e| e.to_string())?;
    *remaining -= 8;
    let length = u64::from_le_bytes(prefix);
    if length == 0 || length > *remaining {
        return Err("invalid or truncated index frame".into());
    }
    let mut frame = reader.take(length);
    let mut decoder = rmp_serde::Deserializer::new(&mut frame);
    // Each descriptor level may occupy multiple MessagePack containers.
    // Leave room for the frontend's deepest supported types and record wrappers.
    decoder.set_max_depth(4096);
    let record = T::deserialize(&mut decoder).map_err(|e| format!("invalid index record: {e}"))?;
    if frame.limit() != 0 {
        return Err("extra bytes in index frame".into());
    }
    *remaining -= length;
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_records_reject_invalid_unit_references_before_merge() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("invalid.index");
        let IndexOutput::Snapshot(mut writer) = IndexOutput::snapshot(&path).unwrap() else {
            unreachable!();
        };
        let program = Program::new(scratch.path().to_path_buf());
        writer.start(&program, &IncludeGraph::default());
        let mut unit = UnitIndex::default();
        unit.fn_returns.insert(trace_ir::FnId(999), Vec::new());
        writer.write(&Record::Units {
            mode: UnitMode::Full,
            units: &[unit],
        });
        writer.write(&Record::End {
            include_deps: Vec::new(),
            extra_dirs: Vec::new(),
        });
        writer.publish().unwrap();
        assert!(read_index(&path)
            .unwrap_err()
            .contains("invalid unit-local IDs"));
    }
}
