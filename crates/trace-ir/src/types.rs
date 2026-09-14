use crate::{FieldId, TypeId};
use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeDesc {
    Void,
    Char,
    Bool,
    Short,
    Int,
    Long,
    LongLong,
    Float,
    Double,
    SizeT,
    Unknown,
    Ptr(Box<TypeDesc>),
    Array {
        elem: Box<TypeDesc>,
        size: Option<u64>,
    },
    Struct {
        name: String,
        fields: Vec<(String, TypeDesc)>,
    },
    Union {
        name: String,
        fields: Vec<(String, TypeDesc)>,
    },
    FnPtr {
        ret: Box<TypeDesc>,
        params: Vec<TypeDesc>,
    },
}

impl TypeDesc {
    pub fn is_pointer_like(&self) -> bool {
        matches!(self, TypeDesc::Ptr(_) | TypeDesc::FnPtr { .. })
    }

    pub fn pointee(&self) -> Option<&TypeDesc> {
        match self {
            TypeDesc::Ptr(inner) => Some(inner),
            _ => None,
        }
    }
}

/// Numeric-category classification used by C++ overload-table construction
/// and call-site ranking. Kept coarser than [`TypeDesc`] so that only the
/// properties overload resolution cares about leak into symbol merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarKind {
    /// `bool`
    Bool,
    /// `char` / `signed char` / `unsigned char`
    Char,
    /// `short` / `unsigned short`
    Short,
    /// `int` / `unsigned int`
    Int,
    /// `long` / `unsigned long`
    Long,
    /// `long long` / `unsigned long long`
    LongLong,
    /// `float`
    Float,
    /// `double` / `long double`
    Double,
    /// Aggregate (struct/union) or any non-numeric type.
    Aggregate,
}

impl TypeDesc {
    /// Numeric category, for distinguishing same-arity overloads.
    pub fn scalar_kind(&self) -> ScalarKind {
        match self {
            TypeDesc::Bool => ScalarKind::Bool,
            TypeDesc::Char => ScalarKind::Char,
            TypeDesc::Short => ScalarKind::Short,
            TypeDesc::Int => ScalarKind::Int,
            TypeDesc::Long => ScalarKind::Long,
            TypeDesc::LongLong => ScalarKind::LongLong,
            TypeDesc::Float => ScalarKind::Float,
            TypeDesc::Double => ScalarKind::Double,
            _ => ScalarKind::Aggregate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeKind {
    Void,
    Char,
    Bool,
    Short,
    Int,
    Long,
    LongLong,
    Float,
    Double,
    SizeT,
    Unknown,
    Ptr,
    Array,
    Struct,
    Union,
    FnPtr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    pub id: TypeId,
    pub desc: TypeDesc,
    pub size: u64,
    pub align: u64,
    pub layout: TypeLayout,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeLayout {
    pub fields: IndexMap<FieldId, FieldLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldLayout {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub type_id: TypeId,
}

#[derive(Debug, Clone)]
pub struct TypeTable {
    /// Every C++ class name a specifier declared, forward declarations
    /// included, distinct from tags merely referenced by a type.
    declared_structs: FxHashSet<std::sync::Arc<str>>,
    /// The subset of [`declared_structs`](Self::declared_structs) whose
    /// specifier carried a body: a forward declaration says a name is a
    /// class, a definition says what its members are.
    defined_structs: FxHashSet<std::sync::Arc<str>>,
    types: Vec<TypeInfo>,
    // Header merging repeatedly hashes nested descriptors and alias names.
    // Preserve insertion order while using the same fast hasher as the tag
    // indexes.
    intern: IndexMap<TypeDesc, TypeId, FxBuildHasher>,
    /// Typedef alias name → resolved descriptor. Alias resolution is needed
    /// because lowering sees bare identifiers (`fn_t`, `SHandle`) whose
    /// pointer-ness is otherwise lost (they degrade to `Int`).
    aliases: IndexMap<String, TypeDesc, FxBuildHasher>,
    /// Named `struct` tag → richest interned [`TypeId`] (most fields).
    struct_tags: FxHashMap<String, TypeId>,
    /// Named `union` tag → richest interned [`TypeId`] (most fields).
    union_tags: FxHashMap<String, TypeId>,
    /// Set when a named struct/union is interned. [`complete_nested_tags`]
    /// clears it after a walk so a second call with no new tags is free.
    needs_tag_completion: bool,
}

impl Default for TypeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeTable {
    pub fn new() -> Self {
        let mut table = Self {
            declared_structs: FxHashSet::default(),
            defined_structs: FxHashSet::default(),
            types: Vec::new(),
            intern: IndexMap::default(),
            aliases: IndexMap::default(),
            struct_tags: FxHashMap::default(),
            union_tags: FxHashMap::default(),
            needs_tag_completion: false,
        };
        table.intern(TypeDesc::Void);
        table.intern(TypeDesc::Char);
        table.intern(TypeDesc::Bool);
        table.intern(TypeDesc::Short);
        table.intern(TypeDesc::Int);
        table.intern(TypeDesc::Long);
        table.intern(TypeDesc::LongLong);
        table.intern(TypeDesc::Float);
        table.intern(TypeDesc::Double);
        table.intern(TypeDesc::SizeT);
        table.intern(TypeDesc::Unknown);
        table
    }

    pub fn intern(&mut self, mut desc: TypeDesc) -> TypeId {
        if let Some(id) = self.interned_as_is(&desc) {
            return id;
        }
        if let Some(id) = self.interned_as_tag(&desc) {
            return id;
        }
        self.canonicalize_in_place(&mut desc);
        if desc_has_named_tag(&desc) {
            self.needs_tag_completion = true;
        }
        if let Some(id) = self.intern.get(&desc) {
            return *id;
        }
        let (size, align, layout) = compute_layout(&desc, self);
        let id = TypeId(self.types.len() as u32);
        self.types.push(TypeInfo {
            id,
            desc: desc.clone(),
            size,
            align,
            layout,
        });
        self.intern.insert(desc, id);
        self.note_named_tag(id);
        id
    }

    /// Rewrite empty `struct Foo` / `union Foo` tags (and pointers to them)
    /// to the most complete interned layout of that tag, when one exists.
    ///
    /// Deliberately shallow: a named aggregate that carries fields keeps them
    /// as the unit that built it saw them, so one C type can still hold more
    /// than one id when two units disagreed on how complete a nested tag was.
    /// Rewriting nested tags here was measured and rejected -- see the #83
    /// section of `docs/EVAL_REPORT.md`.
    fn canonicalize_desc(&self, mut desc: TypeDesc) -> TypeDesc {
        self.canonicalize_in_place(&mut desc);
        desc
    }

    /// [`canonicalize_desc`](Self::canonicalize_desc) without rebuilding the
    /// pointer and array boxes on the way down: on the common path nothing
    /// is rewritten, and merging re-interns every header type per unit.
    fn canonicalize_in_place(&self, desc: &mut TypeDesc) {
        let complete = match &*desc {
            TypeDesc::Struct { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(name, TypeKind::Struct)
            }
            TypeDesc::Union { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(name, TypeKind::Union)
            }
            _ => None,
        };
        if let Some(id) = complete {
            *desc = self.get(id).desc.clone();
            return;
        }
        match desc {
            TypeDesc::Ptr(inner) => self.canonicalize_in_place(inner),
            TypeDesc::Array { elem, .. } => self.canonicalize_in_place(elem),
            _ => {}
        }
    }

    /// The id `desc` is interned under when canonicalization cannot rewrite
    /// it, so the table can be asked before anything is cloned or rebuilt.
    /// Answers exactly as [`intern`](Self::intern) would, side effects
    /// included; `None` means the full path has to run.
    fn interned_as_is(&mut self, desc: &TypeDesc) -> Option<TypeId> {
        if canonicalize_may_rewrite(desc) {
            return None;
        }
        let id = *self.intern.get(desc)?;
        if desc_has_named_tag(desc) {
            self.needs_tag_completion = true;
        }
        Some(id)
    }

    /// The id an empty named tag interns under when the tag is already known:
    /// canonicalization rewrites the tag to the richest layout interned under
    /// that name, and that layout's descriptor is interned under the same id
    /// (every entry is, including one widened in place by
    /// [`union_aggregate_layout`](Self::union_aggregate_layout)). Answers
    /// exactly as [`intern`](Self::intern) would, side effects included,
    /// without cloning the layout into the tag first; `None` means the full
    /// path has to run.
    fn interned_as_tag(&mut self, desc: &TypeDesc) -> Option<TypeId> {
        let id = self.lookup_tag_ref(desc)?;
        self.needs_tag_completion = true;
        Some(id)
    }

    /// [`intern`](Self::intern) by reference: clones `desc` only when the
    /// table does not hold it yet. Merging a unit interns every one of its
    /// types into a table that usually has them already.
    pub fn intern_ref(&mut self, desc: &TypeDesc) -> TypeId {
        if let Some(id) = self.interned_as_is(desc) {
            return id;
        }
        if let Some(id) = self.interned_as_tag(desc) {
            return id;
        }
        self.intern(desc.clone())
    }

    /// After merging another unit, point nested incomplete tag fields at the
    /// most complete layout interned so far (PCH headers are parsed in
    /// isolation; a later header may supply `IDeviceIoService` after
    /// `StreamHost { struct IDeviceIoService service; }` was interned).
    /// Rewrite empty `struct Foo` / `union Foo` tags (and pointers to them)
    /// to the most complete interned layout of that tag, when one exists.
    /// No-op when no named aggregate has been interned since the last walk.
    pub fn complete_nested_tags(&mut self) {
        if !self.needs_tag_completion {
            return;
        }
        let n = self.types.len();
        for i in 0..n {
            let fids: Vec<FieldId> = self.types[i].layout.fields.keys().copied().collect();
            for fid in fids {
                let old = self.types[i].layout.fields[&fid].type_id;
                let new_id = self.complete_type_id(old);
                if new_id != old {
                    if let Some(fl) = self.types[i].layout.fields.get_mut(&fid) {
                        fl.type_id = new_id;
                    }
                }
            }
        }
        self.needs_tag_completion = false;
    }

    fn complete_type_id(&mut self, id: TypeId) -> TypeId {
        match self.get(id).desc.clone() {
            TypeDesc::Struct { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(&name, TypeKind::Struct).unwrap_or(id)
            }
            TypeDesc::Union { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(&name, TypeKind::Union).unwrap_or(id)
            }
            TypeDesc::Ptr(inner) => {
                let completed = self.canonicalize_desc(*inner);
                let richer = match &completed {
                    TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. } => {
                        !fields.is_empty()
                    }
                    _ => false,
                };
                if richer {
                    self.intern(TypeDesc::Ptr(Box::new(completed)))
                } else {
                    id
                }
            }
            _ => id,
        }
    }

    pub fn get(&self, id: TypeId) -> &TypeInfo {
        &self.types[id.0 as usize]
    }

    /// The id `new` interned `desc` under. Asking the intern table beats
    /// naming the id: a hard-coded index silently re-points when the prelude
    /// is reordered or extended, which is how `int()` came to return `Bool`'s
    /// id and `resolve_type_id` came to fall back to `Long`. Pinned by
    /// `primitive_accessors_name_their_own_type` either way.
    fn prelude_id(&self, desc: &TypeDesc) -> TypeId {
        *self
            .intern
            .get(desc)
            .expect("prelude descriptor interned by `TypeTable::new`")
    }

    pub fn void(&self) -> TypeId {
        self.prelude_id(&TypeDesc::Void)
    }

    pub fn int(&self) -> TypeId {
        self.prelude_id(&TypeDesc::Int)
    }

    /// The placeholder a lookup that resolves nothing falls back to.
    pub fn unknown(&self) -> TypeId {
        self.prelude_id(&TypeDesc::Unknown)
    }

    pub fn ptr_to(&mut self, inner: TypeDesc) -> TypeId {
        self.intern(TypeDesc::Ptr(Box::new(inner)))
    }

    /// Record a typedef alias (`typedef void (*fn_t)(int);` → `fn_t`) so that
    /// later declarations using the bare alias keep their pointer-ness.
    pub fn register_alias(&mut self, alias: &str, desc: TypeDesc) {
        if !alias.is_empty() {
            self.aliases.insert(alias.to_string(), desc);
        }
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<&TypeDesc> {
        self.aliases.get(alias)
    }

    pub fn all_aliases(&self) -> &IndexMap<String, TypeDesc, FxBuildHasher> {
        &self.aliases
    }

    pub fn all(&self) -> &[TypeInfo] {
        &self.types
    }

    pub fn compute_struct_layout(
        &mut self,
        name: String,
        fields: Vec<(String, TypeDesc)>,
    ) -> TypeId {
        self.intern(TypeDesc::Struct { name, fields })
    }

    pub fn compute_union_layout(
        &mut self,
        name: String,
        fields: Vec<(String, TypeDesc)>,
    ) -> TypeId {
        self.intern(TypeDesc::Union { name, fields })
    }

    #[must_use]
    pub fn union_struct_layout(&mut self, name: String, fields: Vec<(String, TypeDesc)>) -> TypeId {
        self.union_aggregate_layout(name, fields, TypeKind::Struct)
    }

    #[must_use]
    pub fn union_union_layout(&mut self, name: String, fields: Vec<(String, TypeDesc)>) -> TypeId {
        self.union_aggregate_layout(name, fields, TypeKind::Union)
    }

    fn aggregate_desc(kind: TypeKind, name: String, fields: Vec<(String, TypeDesc)>) -> TypeDesc {
        match kind {
            TypeKind::Struct => TypeDesc::Struct { name, fields },
            TypeKind::Union => TypeDesc::Union { name, fields },
            // Only `union_struct_layout` and `union_union_layout` call in.
            _ => unreachable!("{kind:?} is not an aggregate kind"),
        }
    }

    /// Whether the tag layout of `id` already carries this member.
    ///
    /// Named members are identified by name. Anonymous members
    /// (`struct { ... };` with no declarator) all share the empty name, so
    /// matching on the name alone would let the first one swallow every other
    /// anonymous member of the variant.
    fn tag_layout_has_field(&self, id: TypeId, fname: &str, fdesc: &TypeDesc) -> bool {
        self.get(id).layout.fields.values().any(|fl| {
            fl.name == fname && (!fname.is_empty() || self.get(fl.type_id).desc == *fdesc)
        })
    }

    fn union_aggregate_layout(
        &mut self,
        name: String,
        incoming_fields: Vec<(String, TypeDesc)>,
        kind: TypeKind,
    ) -> TypeId {
        if name.is_empty() {
            return self.intern(Self::aggregate_desc(kind, name, incoming_fields));
        }
        let Some(existing_id) = self.type_id_by_tag(&name, kind) else {
            return self.intern(Self::aggregate_desc(kind, name, incoming_fields));
        };

        // Every variant re-merges every aggregate its configuration shares with
        // the base, so the overwhelmingly common case is an incoming layout that
        // adds nothing. Answer that against the stored layout, before cloning a
        // descriptor per existing field — those clones are deep.
        if incoming_fields
            .iter()
            .all(|(n, d)| self.tag_layout_has_field(existing_id, n, d))
        {
            return existing_id;
        }

        let mut merged_fields: Vec<(String, TypeDesc)> = self
            .get(existing_id)
            .layout
            .fields
            .values()
            .map(|fl| (fl.name.clone(), self.get(fl.type_id).desc.clone()))
            .collect();
        for (in_name, in_desc) in incoming_fields {
            // Deduplicate against what the variant itself has already added,
            // not only against the base layout.
            let already_present = merged_fields
                .iter()
                .any(|(n, d)| n == &in_name && (!in_name.is_empty() || d == &in_desc));
            if !already_present {
                merged_fields.push((in_name, in_desc));
            }
        }
        let new_desc = Self::aggregate_desc(kind, name, merged_fields);
        let (size, align, layout) = compute_layout(&new_desc, self);
        self.types[existing_id.0 as usize].desc = new_desc.clone();
        self.types[existing_id.0 as usize].size = size;
        self.types[existing_id.0 as usize].align = align;
        self.types[existing_id.0 as usize].layout = layout;
        // The pre-union descriptor stays interned on purpose: it still names
        // this type, and dropping it would let a later unit spelling the
        // narrower layout intern a *second* id and lose the union.
        self.intern.insert(new_desc, existing_id);
        // The entry just gained fields, so the tag maps have to reconsider it:
        // they point at the richest layout for a name, and this one changed
        // under them (#59 review).
        self.note_named_tag(existing_id);
        self.needs_tag_completion = true;
        existing_id
    }

    pub fn field_id_by_name(&self, type_id: TypeId, fname: &str) -> Option<FieldId> {
        let info = self.get(type_id);
        info.layout
            .fields
            .iter()
            .find(|(_, fl)| fl.name == fname)
            .map(|(id, _)| *id)
    }

    fn lookup_tag_ref(&self, desc: &TypeDesc) -> Option<TypeId> {
        match desc {
            TypeDesc::Struct { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(name, TypeKind::Struct)
            }
            TypeDesc::Union { name, fields } if fields.is_empty() && !name.is_empty() => {
                self.type_id_by_tag(name, TypeKind::Union)
            }
            _ => None,
        }
    }

    pub fn type_id_by_tag(&self, name: &str, kind: TypeKind) -> Option<TypeId> {
        match kind {
            TypeKind::Struct => self.struct_tags.get(name).copied(),
            TypeKind::Union => self.union_tags.get(name).copied(),
            _ => None,
        }
    }

    /// Record that `name` is a class: any `class` / `struct` specifier,
    /// with or without a body.
    pub fn declare_struct(&mut self, name: &str) {
        if !self.declared_structs.contains(name) {
            self.declared_structs.insert(name.into());
        }
    }

    /// Record that `name`'s class body was seen; implies
    /// [`declare_struct`](Self::declare_struct).
    pub fn define_struct(&mut self, name: &str) {
        if self.defined_structs.contains(name) {
            return;
        }
        let tag = match self.declared_structs.get(name) {
            Some(tag) => tag.clone(),
            None => {
                let tag: std::sync::Arc<str> = name.into();
                self.declared_structs.insert(tag.clone());
                tag
            }
        };
        self.defined_structs.insert(tag);
    }

    pub fn is_struct_declared(&self, name: &str) -> bool {
        self.declared_structs.contains(name)
    }

    pub fn is_struct_defined(&self, name: &str) -> bool {
        self.defined_structs.contains(name)
    }

    pub fn merge_struct_declarations(&mut self, other: &Self) {
        self.declared_structs
            .extend(other.declared_structs.iter().cloned());
        self.defined_structs
            .extend(other.defined_structs.iter().cloned());
    }

    fn note_named_tag(&mut self, id: TypeId) {
        let (kind, name, richness) = {
            let info = &self.types[id.0 as usize];
            let kind = match &info.desc {
                TypeDesc::Struct { name, .. } if !name.is_empty() => TypeKind::Struct,
                TypeDesc::Union { name, .. } if !name.is_empty() => TypeKind::Union,
                _ => return,
            };
            let name = match &info.desc {
                TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. } => name.clone(),
                _ => return,
            };
            (kind, name, tag_richness(info))
        };
        let old = match kind {
            TypeKind::Struct => self.struct_tags.get(name.as_str()).copied(),
            TypeKind::Union => self.union_tags.get(name.as_str()).copied(),
            _ => return,
        };
        if let Some(old) = old {
            if richness <= tag_richness(&self.types[old.0 as usize]) {
                return;
            }
        }
        match kind {
            TypeKind::Struct => {
                self.struct_tags.insert(name, id);
            }
            TypeKind::Union => {
                self.union_tags.insert(name, id);
            }
            _ => {}
        }
    }

    pub fn resolve_type_id(&self, desc: &TypeDesc) -> TypeId {
        if let Some(id) = self.lookup_tag_ref(desc) {
            return id;
        }
        if let Some(id) = self.intern.get(desc) {
            return *id;
        }
        if let TypeDesc::Ptr(inner) = desc {
            let pointee = self.lookup_tag_ref(inner).unwrap_or_else(|| {
                self.intern
                    .get(inner.as_ref())
                    .copied()
                    .unwrap_or(self.unknown())
            });
            if pointee != self.unknown() {
                let pointee_desc = self.get(pointee).desc.clone();
                let ptr_desc = TypeDesc::Ptr(Box::new(pointee_desc));
                if let Some(id) = self.intern.get(&ptr_desc) {
                    return *id;
                }
            }
        }
        self.unknown()
    }
}

fn tag_richness(info: &TypeInfo) -> usize {
    let layout_n = info.layout.fields.len();
    let desc_n = match &info.desc {
        TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. } => fields.len(),
        _ => 0,
    };
    layout_n.max(desc_n)
}

/// Whether `canonicalize_in_place` could rewrite `desc`: an empty named
/// tag at the top or under pointers and arrays, which it replaces with the
/// tag's most complete layout when one is interned. Conservative: the tag
/// may have no such layout yet, in which case nothing changes either.
fn canonicalize_may_rewrite(desc: &TypeDesc) -> bool {
    match desc {
        TypeDesc::Struct { name, fields } | TypeDesc::Union { name, fields } => {
            fields.is_empty() && !name.is_empty()
        }
        TypeDesc::Ptr(inner) | TypeDesc::Array { elem: inner, .. } => {
            canonicalize_may_rewrite(inner)
        }
        _ => false,
    }
}

fn desc_has_named_tag(desc: &TypeDesc) -> bool {
    match desc {
        TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. } => !name.is_empty(),
        TypeDesc::Ptr(inner) | TypeDesc::Array { elem: inner, .. } => desc_has_named_tag(inner),
        TypeDesc::FnPtr { ret, params } => {
            desc_has_named_tag(ret) || params.iter().any(desc_has_named_tag)
        }
        _ => false,
    }
}

fn compute_layout(desc: &TypeDesc, table: &mut TypeTable) -> (u64, u64, TypeLayout) {
    match desc {
        TypeDesc::Void => (0, 1, TypeLayout::default()),
        TypeDesc::Char | TypeDesc::Bool => (1, 1, TypeLayout::default()),
        TypeDesc::Short => (2, 2, TypeLayout::default()),
        TypeDesc::Int | TypeDesc::Float => (4, 4, TypeLayout::default()),
        TypeDesc::Long
        | TypeDesc::LongLong
        | TypeDesc::Double
        | TypeDesc::SizeT
        | TypeDesc::Unknown => (8, 8, TypeLayout::default()),
        TypeDesc::Ptr(_) | TypeDesc::FnPtr { .. } => (8, 8, TypeLayout::default()),
        TypeDesc::Array { elem, size } => {
            let (elem_size, elem_align, _) = compute_layout(elem, table);
            let count = size.unwrap_or(0);
            (elem_size * count, elem_align, TypeLayout::default())
        }
        TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. } => {
            let mut layout = TypeLayout::default();
            let mut offset = 0u64;
            let mut max_align = 1u64;
            let mut total_size = 0u64;
            for (idx, (name, field_desc)) in fields.iter().enumerate() {
                let fid = FieldId(idx as u32);
                let field_type_id = table.intern(field_desc.clone());
                let (field_size, field_align, _) = compute_layout(field_desc, table);
                max_align = max_align.max(field_align);
                offset = align_up(offset, field_align);
                layout.fields.insert(
                    fid,
                    FieldLayout {
                        name: name.clone(),
                        offset,
                        size: field_size,
                        type_id: field_type_id,
                    },
                );
                offset += field_size;
                total_size = total_size.max(offset);
            }
            total_size = align_up(total_size, max_align);
            (total_size, max_align, layout)
        }
    }
}

/// Whether `a` and `b` describe the same parameter type.
///
/// Compared by shape rather than by `TypeId`: two units, or two configurations
/// of one unit, intern their own copy of a type, so the same parameter can
/// arrive under different ids and an id comparison would split a function from
/// itself. Tags compare by name because the units may legitimately have
/// contributed different field sets -- one saw a nested tag complete and
/// another did not, and reconciling those is what the layout union is for --
/// and reconciling those is what the layout union is for. A tag name is
/// compared with any leading `::` trimmed, so a qualified name spelled
/// `::ns::X` in one unit and `ns::X` in another is one type rather than two
/// overloads.
///
/// An unresolvable type (`TypeDesc::Unknown`) is a type here, matching only
/// another unresolvable one. See
/// [`same_param_type_or_unresolved`] for the caller that wants it as a
/// wildcard, and why this one must not.
pub fn same_param_type(types: &TypeTable, a: TypeId, b: TypeId) -> bool {
    same_param_type_inner(types, a, b, false)
}

/// [`same_param_type`], with an unresolvable type as a wildcard rather than a
/// type of its own.
///
/// A caller may only ask for this if an ambiguous answer is safe for it. The
/// variant merge qualifies: it pairs the arms of ONE function in one file and
/// takes a candidate only when exactly one matches, so a wildcard that reaches
/// several candidates falls back to the ordinary merge path instead of
/// choosing by insertion order. `SymbolTable::register_function` does not: it
/// takes the first compatible candidate out of an overload bucket, so a
/// wildcard there merges `f(int)` into `f(<unresolved>)` and then hands
/// `f(double)`'s body to the same entry.
pub fn same_param_type_or_unresolved(types: &TypeTable, a: TypeId, b: TypeId) -> bool {
    same_param_type_inner(types, a, b, true)
}

fn same_param_type_inner(
    types: &TypeTable,
    a: TypeId,
    b: TypeId,
    unresolved_matches: bool,
) -> bool {
    if a == b {
        return true;
    }
    let (da, db) = (&types.get(a).desc, &types.get(b).desc);
    // A parameter written as an array IS a pointer: `int a[]` and `int *a`
    // declare the same function, so a prototype spelling one and a definition
    // spelling the other are one function, not two overloads. The decay is
    // top-level only -- nested, `int (*)[10]` and `int **` are different types,
    // which is why `same_type_shape` does not do this itself.
    same_decayed_param(da, db, unresolved_matches)
}

/// [`same_param_type`] on two descriptors: the parameter rule, so it can also
/// be applied to the parameter list inside a [`TypeDesc::FnPtr`].
///
/// A parameter of array type is a pointer at every depth the language spells a
/// parameter list, not only the outermost one: `void (*cb)(int a[])` and
/// `void (*cb)(int *a)` declare the same function-pointer type, so the decay
/// has to reach the parameters of a pointed-to function too.
fn same_decayed_param(da: &TypeDesc, db: &TypeDesc, unresolved_matches: bool) -> bool {
    match (da, db) {
        (TypeDesc::Ptr(x), TypeDesc::Array { elem: y, .. })
        | (TypeDesc::Array { elem: x, .. }, TypeDesc::Ptr(y))
        | (TypeDesc::Array { elem: x, .. }, TypeDesc::Array { elem: y, .. }) => {
            same_type_shape(x, y, unresolved_matches)
        }
        _ => same_type_shape(da, db, unresolved_matches),
    }
}

/// Whether two array extents can belong to one type.
///
/// An unstated bound matches any bound, so a `int (*)[]` parameter still meets
/// the `int (*)[10]` it was declared against. Two stated bounds must agree:
/// `int (*)[10]` and `int (*)[20]` are different types and, nested inside a
/// parameter, different overloads.
fn same_array_extent(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Whether a lowered tag name marks an anonymous tag instead of naming one.
///
/// `lower_tag` names an unnamed struct or union `anon_<n>` off a counter each
/// unit seeds from the program's, so two units both call their first anonymous
/// tag `anon_1` however unrelated those tags are, and one shared tag can be
/// `anon_1` in one unit and `anon_7` in another. Such a name carries no
/// identity between units and must not be compared as one. `lower.rs` already
/// reads the prefix this way when it asks whether a tag is really named, and
/// `extract_tag_name` falls back to a bare `anon`.
///
/// The digits are required: `anon_<n>` is the whole of what lowering
/// synthesizes, while `anon_vma`, `anon_inode` and friends are ordinary tags a
/// C tree really declares. Treating those as anonymous would compare them
/// structurally, so any two of them with one matching field became the same
/// type -- the opposite of the fault this predicate exists to fix.
///
/// The test is applied to the LEAF, because a C++ anonymous class or struct
/// reaches the type table qualified: `lower_tag` registers one under
/// `ctx.qualify(&name)`, so an anonymous tag inside `namespace ns` is
/// `ns::anon_1`. Asking about the whole spelling called that named, and two
/// units numbering the same tag differently could then never match.
pub fn is_anonymous_tag(name: &str) -> bool {
    let leaf = name.rsplit("::").next().unwrap_or(name);
    if leaf.is_empty() || leaf == "anon" {
        return true;
    }
    match leaf.strip_prefix("anon_") {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Structural comparison behind [`same_param_type`].
///
/// `Unknown` against anything else answers `unresolved_matches`; against
/// another `Unknown` it falls to the discriminant arm and matches either way.
fn same_type_shape(a: &TypeDesc, b: &TypeDesc, unresolved_matches: bool) -> bool {
    match (a, b) {
        (TypeDesc::Unknown, _) | (_, TypeDesc::Unknown) if unresolved_matches => true,
        (TypeDesc::Ptr(x), TypeDesc::Ptr(y)) => same_type_shape(x, y, unresolved_matches),
        (TypeDesc::Array { elem: x, size: sx }, TypeDesc::Array { elem: y, size: sy }) => {
            same_array_extent(*sx, *sy) && same_type_shape(x, y, unresolved_matches)
        }
        (
            TypeDesc::Struct {
                name: x,
                fields: fx,
            },
            TypeDesc::Struct {
                name: y,
                fields: fy,
            },
        )
        | (
            TypeDesc::Union {
                name: x,
                fields: fx,
            },
            TypeDesc::Union {
                name: y,
                fields: fy,
            },
        ) => {
            let (x, y) = (x.trim_start_matches("::"), y.trim_start_matches("::"));
            if is_anonymous_tag(x) || is_anonymous_tag(y) {
                // Anonymous tags have no name to compare, and every anonymous
                // tag would otherwise match every other one -- by emptiness
                // before lowering names them, and by a colliding `anon_<n>`
                // after. Compare them structurally instead; a named tag never
                // matches an anonymous one, since only one of the two has an
                // identity to share.
                is_anonymous_tag(x)
                    && is_anonymous_tag(y)
                    && fx.len() == fy.len()
                    && fx.iter().zip(fy).all(|((n1, t1), (n2, t2))| {
                        n1 == n2 && same_type_shape(t1, t2, unresolved_matches)
                    })
            } else {
                x == y
            }
        }
        (
            TypeDesc::FnPtr {
                ret: r1,
                params: p1,
            },
            TypeDesc::FnPtr {
                ret: r2,
                params: p2,
            },
        ) => {
            p1.len() == p2.len()
                && same_type_shape(r1, r2, unresolved_matches)
                // The parameters of the pointed-to function are parameters,
                // so they decay like any other; the RETURN type does not.
                && p1
                    .iter()
                    .zip(p2)
                    .all(|(x, y)| same_decayed_param(x, y, unresolved_matches))
        }
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    value.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_accessors_name_their_own_type() {
        // These named raw indices into the primitives `new` interns, so a
        // reordered or extended prelude silently repointed them: `int()` named
        // `Bool` for as long as `Bool` has sat between `Char` and `Short`. They
        // ask the intern table for their descriptor's id now; this still pins
        // the answers, so a lookup keyed on the wrong descriptor cannot pass.
        let t = TypeTable::new();
        assert_eq!(t.get(t.void()).desc, TypeDesc::Void);
        assert_eq!(t.get(t.int()).desc, TypeDesc::Int);
        assert_eq!(t.get(t.unknown()).desc, TypeDesc::Unknown);
    }

    #[test]
    fn resolving_an_unknown_tag_yields_unknown() {
        // `resolve_type_id` fell back to a raw `TypeId(5)`, which the prelude
        // had long since grown past: it named `Long`, so every dereference
        // through a type the table never interned came back a signed integer
        // rather than the placeholder the function means.
        let t = TypeTable::new();
        let missing = TypeDesc::Struct {
            name: "NeverInterned".to_string(),
            fields: Vec::new(),
        };
        assert_eq!(t.get(t.resolve_type_id(&missing)).desc, TypeDesc::Unknown);
    }

    #[test]
    fn struct_declarations_distinguish_forward_from_defined_and_merge() {
        let mut header = TypeTable::new();
        header.declare_struct("ns::Fwd");
        header.define_struct("ns::Full");
        assert!(header.is_struct_declared("ns::Fwd"));
        assert!(!header.is_struct_defined("ns::Fwd"));
        assert!(header.is_struct_declared("ns::Full"));
        assert!(header.is_struct_defined("ns::Full"));
        assert!(!header.is_struct_declared("ns::Absent"));

        let mut unit = TypeTable::new();
        unit.declare_struct("ns::Full");
        unit.merge_struct_declarations(&header);
        assert!(unit.is_struct_declared("ns::Fwd"));
        assert!(!unit.is_struct_defined("ns::Fwd"));
        assert!(unit.is_struct_defined("ns::Full"));
        // A definition seen after a forward declaration promotes the name.
        unit.define_struct("ns::Fwd");
        assert!(unit.is_struct_defined("ns::Fwd"));
    }

    #[test]
    fn empty_tag_interns_to_the_richest_layout_without_a_rewrite() {
        // Merging re-interns every header tag per unit, so an empty tag whose
        // name the table already knows must answer from the tag map -- and it
        // must answer what the rewrite path answers: the richest layout's id,
        // whether that layout was built by `intern`, widened in place by a
        // union, or is itself still empty.
        let mut t = TypeTable::new();
        let empty = TypeDesc::Struct {
            name: "Tag".into(),
            fields: Vec::new(),
        };
        let first = t.intern(empty.clone());
        assert_eq!(t.intern_ref(&empty), first);
        let full = t.compute_struct_layout("Tag".into(), vec![("a".into(), TypeDesc::Int)]);
        assert_ne!(full, first);
        assert_eq!(t.intern_ref(&empty), full);
        assert_eq!(t.intern(empty.clone()), full);
        let widened = t.union_struct_layout("Tag".into(), vec![("b".into(), TypeDesc::Char)]);
        assert_eq!(widened, full);
        assert_eq!(t.intern_ref(&empty), full);
        assert_eq!(t.get(full).layout.fields.len(), 2);
        let ptr = TypeDesc::Ptr(Box::new(empty));
        let by_ref = t.intern_ref(&ptr);
        let by_value = t.intern(ptr);
        assert_eq!(by_ref, by_value);
    }

    #[test]
    fn complete_nested_tags_rewrites_empty_embedded_struct() {
        let mut t = TypeTable::new();
        let host = t.compute_struct_layout(
            "StreamHost".into(),
            vec![(
                "service".into(),
                TypeDesc::Struct {
                    name: "IDeviceIoService".into(),
                    fields: Vec::new(),
                },
            )],
        );
        t.compute_struct_layout(
            "IDeviceIoService".into(),
            vec![(
                "Dispatch".into(),
                TypeDesc::FnPtr {
                    ret: Box::new(TypeDesc::Int),
                    params: Vec::new(),
                },
            )],
        );
        t.complete_nested_tags();
        let service_tid = t.get(host).layout.fields.get(&FieldId(0)).unwrap().type_id;
        assert!(
            t.field_id_by_name(service_tid, "Dispatch").is_some(),
            "embedded IDeviceIoService must expose Dispatch after completion"
        );
    }

    fn tag(name: &str, fields: Vec<(String, TypeDesc)>) -> TypeDesc {
        TypeDesc::Struct {
            name: name.into(),
            fields,
        }
    }

    /// A parameter written as an array IS a pointer, so a prototype spelling
    /// `int a[]` and a definition spelling `int *a` are one function. Without
    /// the decay the two read as C++ overloads and every caller stayed bound
    /// to the undefined prototype -- the #83 symptom, a different cause.
    #[test]
    fn a_parameter_written_as_an_array_matches_the_same_pointer() {
        let mut t = TypeTable::new();
        let ptr = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Int)));
        let arr = t.intern(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: None,
        });
        assert!(same_param_type(&t, ptr, arr));
        assert!(same_param_type(&t, arr, ptr));

        // The decay is top-level only: `int (*)[10]` is not `int **`.
        let ptr_to_arr = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: Some(10),
        })));
        let ptr_to_ptr = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Ptr(Box::new(
            TypeDesc::Int,
        )))));
        assert!(!same_param_type(&t, ptr_to_arr, ptr_to_ptr));

        // A nested extent is part of the type: `int (*)[10]` and
        // `int (*)[20]` are distinct overloads. An unstated bound still
        // matches a stated one.
        let ptr_to_arr20 = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: Some(20),
        })));
        assert!(!same_param_type(&t, ptr_to_arr, ptr_to_arr20));
        let ptr_to_arr_open = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: None,
        })));
        assert!(same_param_type(&t, ptr_to_arr, ptr_to_arr_open));

        // A top-level extent is not: a parameter's array bound is discarded,
        // so `int a[10]` and `int a[20]` declare one function.
        let arr10 = t.intern(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: Some(10),
        });
        let arr20 = t.intern(TypeDesc::Array {
            elem: Box::new(TypeDesc::Int),
            size: Some(20),
        });
        assert!(same_param_type(&t, arr10, arr20));
        assert!(same_param_type(&t, arr10, ptr));
    }

    /// Tags compare by name, and an anonymous tag has none -- so every
    /// anonymous struct matched every other one, folding unrelated parameter
    /// types together. They compare structurally instead.
    #[test]
    fn an_unresolvable_type_is_a_wildcard_only_where_it_is_asked_for() {
        let mut t = TypeTable::new();
        let unknown = t.unknown();
        let int_ty = t.intern(TypeDesc::Int);
        let ptr_unknown = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Unknown)));
        let ptr_int = t.intern(TypeDesc::Ptr(Box::new(TypeDesc::Int)));

        assert!(!same_param_type(&t, unknown, int_ty));
        assert!(same_param_type(&t, unknown, unknown));
        // Nested, too: `T *` for an unresolvable `T` is not every pointer.
        assert!(!same_param_type(&t, ptr_unknown, ptr_int));
        assert!(same_param_type(&t, ptr_unknown, ptr_unknown));

        assert!(same_param_type_or_unresolved(&t, unknown, int_ty));
        assert!(same_param_type_or_unresolved(&t, ptr_unknown, ptr_int));
        // Neither spelling makes two resolved and different types match.
        assert!(!same_param_type_or_unresolved(&t, int_ty, ptr_int));
    }

    #[test]
    fn two_anonymous_tags_are_not_the_same_type() {
        let mut t = TypeTable::new();
        let anon_int = t.intern(TypeDesc::Struct {
            name: String::new(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        let anon_double = t.intern(TypeDesc::Struct {
            name: String::new(),
            fields: vec![("a".into(), TypeDesc::Double)],
        });
        assert!(!same_param_type(&t, anon_int, anon_double));

        // Two anonymous tags of the same shape still match, and a named tag
        // never matches an anonymous one.
        let anon_int_again = t.intern(TypeDesc::Struct {
            name: String::new(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(same_param_type(&t, anon_int, anon_int_again));
        let named = t.intern(TypeDesc::Struct {
            name: "S".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(!same_param_type(&t, anon_int, named));

        // The names lowering actually produces. `anon_<n>` comes off a
        // per-unit counter, so unrelated tags collide on a number and one
        // shared tag disagrees about its number; neither may be decided by
        // comparing the name. Written with `String::new()` alone, this test
        // exercised a spelling `lower_tag` never emits.
        let anon1_int = t.intern(TypeDesc::Struct {
            name: "anon_1".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        let anon1_double = t.intern(TypeDesc::Struct {
            name: "anon_1".into(),
            fields: vec![("a".into(), TypeDesc::Double)],
        });
        assert!(
            !same_param_type(&t, anon1_int, anon1_double),
            "two units' first anonymous tags share a number, not a type"
        );
        let anon7_int = t.intern(TypeDesc::Struct {
            name: "anon_7".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(
            same_param_type(&t, anon1_int, anon7_int),
            "one tag numbered differently by two units is still one type"
        );
        // `extract_tag_name`'s fallback spelling, and a real tag that merely
        // starts the same way.
        let bare_anon = t.intern(TypeDesc::Struct {
            name: "anon".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(same_param_type(&t, anon1_int, bare_anon));
        // Ordinary tags that merely start the same way. `anonymizer` was the
        // only one this test used, and it does not even carry the `anon_`
        // prefix, so it never exercised the predicate: `anon_vma` and
        // `anon_data` are real tags a C tree declares, and calling them
        // anonymous made any two of them with one matching field the same
        // type.
        for real in ["anonymizer", "anon_vma", "anon_data", "anon_"] {
            let named = t.intern(TypeDesc::Struct {
                name: real.into(),
                fields: vec![("a".into(), TypeDesc::Int)],
            });
            assert!(
                !same_param_type(&t, anon1_int, named),
                "`{real}` is a named tag, not an anonymous one"
            );
        }
        let anon_vma_2 = t.intern(TypeDesc::Struct {
            name: "anon_vma".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        let anon_data = t.intern(TypeDesc::Struct {
            name: "anon_data".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(
            !same_param_type(&t, anon_vma_2, anon_data),
            "two unrelated named tags sharing a field shape are still two types"
        );

        // A C++ anonymous tag is registered qualified (`ctx.qualify`), so the
        // predicate has to read the leaf: asking about `ns::anon_1` whole
        // called it named, and two units numbering one tag differently could
        // then never match.
        let ns_anon1 = t.intern(TypeDesc::Struct {
            name: "ns::anon_1".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        let ns_anon4 = t.intern(TypeDesc::Struct {
            name: "ns::anon_4".into(),
            fields: vec![("a".into(), TypeDesc::Int)],
        });
        assert!(same_param_type(&t, ns_anon1, ns_anon4));
        let ns_anon4_double = t.intern(TypeDesc::Struct {
            name: "ns::anon_4".into(),
            fields: vec![("a".into(), TypeDesc::Double)],
        });
        assert!(!same_param_type(&t, ns_anon1, ns_anon4_double));
    }

    #[test]
    fn a_function_pointer_parameter_written_as_an_array_matches_the_pointer() {
        // `void (*)(int a[])` and `void (*)(int *a)` are one type: the decay
        // applies to a parameter list wherever the language spells one, not
        // only to the outermost parameter. The `FnPtr` arm compared its
        // parameters with the plain shape rule, so a callback prototype
        // spelling an array rejected the definition spelling a pointer.
        let mut t = TypeTable::new();
        let cb_arr = t.intern(TypeDesc::FnPtr {
            ret: Box::new(TypeDesc::Void),
            params: vec![TypeDesc::Array {
                elem: Box::new(TypeDesc::Int),
                size: None,
            }],
        });
        let cb_ptr = t.intern(TypeDesc::FnPtr {
            ret: Box::new(TypeDesc::Void),
            params: vec![TypeDesc::Ptr(Box::new(TypeDesc::Int))],
        });
        assert!(same_param_type(&t, cb_arr, cb_ptr));

        // The return type is not a parameter and does not decay.
        let ret_arr = t.intern(TypeDesc::FnPtr {
            ret: Box::new(TypeDesc::Array {
                elem: Box::new(TypeDesc::Int),
                size: None,
            }),
            params: Vec::new(),
        });
        let ret_ptr = t.intern(TypeDesc::FnPtr {
            ret: Box::new(TypeDesc::Ptr(Box::new(TypeDesc::Int))),
            params: Vec::new(),
        });
        assert!(!same_param_type(&t, ret_arr, ret_ptr));
    }

    #[test]
    fn a_self_referential_tag_does_not_grow_its_own_descriptor() {
        // `struct Node { struct Node *next; }`. Substituting the tag's own
        // layout for the empty `struct Node` inside it would nest one more
        // copy on every intern, so the field keeps its tag-reference form.
        let mut t = TypeTable::new();
        let node = tag(
            "Node",
            vec![(
                "next".into(),
                TypeDesc::Ptr(Box::new(tag("Node", Vec::new()))),
            )],
        );
        let first = t.intern(node.clone());
        let again = t.intern(node);
        assert_eq!(first, again, "interning the same tag twice is idempotent");
        let TypeDesc::Struct { fields, .. } = &t.get(first).desc else {
            panic!("expected a struct");
        };
        assert_eq!(
            fields[0].1,
            TypeDesc::Ptr(Box::new(tag("Node", Vec::new()))),
            "the self-reference stays a tag reference, not an inlined copy"
        );
    }

    #[test]
    fn type_id_by_tag_prefers_richer_layout() {
        let mut t = TypeTable::new();
        let empty = t.intern(TypeDesc::Struct {
            name: "Foo".into(),
            fields: Vec::new(),
        });
        assert_eq!(t.type_id_by_tag("Foo", TypeKind::Struct), Some(empty));
        let rich = t.compute_struct_layout("Foo".into(), vec![("x".into(), TypeDesc::Int)]);
        assert_eq!(t.type_id_by_tag("Foo", TypeKind::Struct), Some(rich));
        assert_ne!(empty, rich);
        let still = t.intern(TypeDesc::Struct {
            name: "Foo".into(),
            fields: Vec::new(),
        });
        assert_eq!(still, rich, "empty tag must rewrite to the complete layout");
        assert_eq!(t.type_id_by_tag("Foo", TypeKind::Struct), Some(rich));
    }

    #[test]
    fn unioning_a_layout_appends_without_moving_base_fields() {
        // A variant that inserts a member in the MIDDLE of the base struct.
        // Appending is what keeps the base configuration's `FieldId`s stable;
        // the variant's own indices drift, which is what the solver's
        // `expected_name` fallback exists to repair (#59).
        let mut t = TypeTable::new();
        let base = t.compute_struct_layout(
            "S".into(),
            vec![("a".into(), TypeDesc::Int), ("b".into(), TypeDesc::Int)],
        );
        let a = t.field_id_by_name(base, "a").unwrap();
        let b = t.field_id_by_name(base, "b").unwrap();

        let merged = t.union_struct_layout(
            "S".into(),
            vec![
                ("a".into(), TypeDesc::Int),
                ("mid".into(), TypeDesc::Int),
                ("b".into(), TypeDesc::Int),
            ],
        );
        assert_eq!(merged, base, "the tag keeps its identity across the union");
        assert_eq!(t.field_id_by_name(merged, "a"), Some(a));
        assert_eq!(t.field_id_by_name(merged, "b"), Some(b));
        assert_eq!(t.get(merged).layout.fields.len(), 3);
        assert_eq!(
            t.field_id_by_name(merged, "mid"),
            Some(FieldId(2)),
            "a variant-only field is appended, not inserted"
        );
    }

    #[test]
    fn unioning_a_layout_keeps_the_tag_on_the_richer_entry() {
        // The union mutates the stored type in place, so the tag maps — which
        // point at the richest layout for a name — have to reconsider it
        // afterwards. They were left holding whatever they had before (#59
        // review).
        let mut t = TypeTable::new();
        let empty = t.compute_struct_layout("S".into(), Vec::new());
        assert_eq!(t.type_id_by_tag("S", TypeKind::Struct), Some(empty));

        let merged = t.union_struct_layout(
            "S".into(),
            vec![("a".into(), TypeDesc::Int), ("b".into(), TypeDesc::Int)],
        );
        assert_eq!(
            t.type_id_by_tag("S", TypeKind::Struct),
            Some(merged),
            "the tag resolves to the layout that now carries the fields"
        );
        assert_eq!(t.get(merged).layout.fields.len(), 2);
    }

    #[test]
    fn re_unioning_the_same_layout_changes_nothing() {
        // Every variant re-merges every aggregate it shares with the base.
        let mut t = TypeTable::new();
        let base = t.compute_struct_layout(
            "S".into(),
            vec![("a".into(), TypeDesc::Int), ("b".into(), TypeDesc::Int)],
        );
        let size = t.get(base).size;
        for _ in 0..3 {
            let again = t.union_struct_layout(
                "S".into(),
                vec![("a".into(), TypeDesc::Int), ("b".into(), TypeDesc::Int)],
            );
            assert_eq!(again, base);
            assert_eq!(t.get(base).layout.fields.len(), 2);
            assert_eq!(t.get(base).size, size);
        }
    }

    #[test]
    fn anonymous_members_are_matched_on_their_type() {
        // Anonymous members all share the empty name, so name-only matching
        // would let the first one swallow every other one.
        let mut t = TypeTable::new();
        let base = t.compute_struct_layout(
            "S".into(),
            vec![(
                String::new(),
                TypeDesc::Union {
                    name: "U1".into(),
                    fields: vec![("x".into(), TypeDesc::Int)],
                },
            )],
        );
        let merged = t.union_struct_layout(
            "S".into(),
            vec![
                (
                    String::new(),
                    TypeDesc::Union {
                        name: "U1".into(),
                        fields: vec![("x".into(), TypeDesc::Int)],
                    },
                ),
                (
                    String::new(),
                    TypeDesc::Union {
                        name: "U2".into(),
                        fields: vec![("y".into(), TypeDesc::Int)],
                    },
                ),
            ],
        );
        assert_eq!(merged, base);
        assert_eq!(
            t.get(merged).layout.fields.len(),
            2,
            "the identical anonymous member is shared, the differing one is added"
        );
    }
}
