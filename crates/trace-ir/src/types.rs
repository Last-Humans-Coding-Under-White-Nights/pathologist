use crate::{FieldId, TypeId};
use indexmap::IndexMap;
use rustc_hash::FxHashMap;
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
    types: Vec<TypeInfo>,
    intern: IndexMap<TypeDesc, TypeId>,
    /// Typedef alias name → resolved descriptor. Alias resolution is needed
    /// because lowering sees bare identifiers (`fn_t`, `SHandle`) whose
    /// pointer-ness is otherwise lost (they degrade to `Int`).
    aliases: IndexMap<String, TypeDesc>,
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
            types: Vec::new(),
            intern: IndexMap::new(),
            aliases: IndexMap::new(),
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

    pub fn intern(&mut self, desc: TypeDesc) -> TypeId {
        let desc = self.canonicalize_desc(desc);
        if desc_has_named_tag(&desc) {
            self.needs_tag_completion = true;
        }
        if let Some(id) = self.lookup_tag_ref(&desc) {
            return id;
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
    fn canonicalize_desc(&self, desc: TypeDesc) -> TypeDesc {
        match desc {
            TypeDesc::Ptr(inner) => TypeDesc::Ptr(Box::new(self.canonicalize_desc(*inner))),
            TypeDesc::Array { elem, size } => TypeDesc::Array {
                elem: Box::new(self.canonicalize_desc(*elem)),
                size,
            },
            TypeDesc::Struct { name, fields } if fields.is_empty() && !name.is_empty() => {
                if let Some(id) = self.type_id_by_tag(&name, TypeKind::Struct) {
                    return self.get(id).desc.clone();
                }
                TypeDesc::Struct { name, fields }
            }
            TypeDesc::Union { name, fields } if fields.is_empty() && !name.is_empty() => {
                if let Some(id) = self.type_id_by_tag(&name, TypeKind::Union) {
                    return self.get(id).desc.clone();
                }
                TypeDesc::Union { name, fields }
            }
            other => other,
        }
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

    pub fn all_aliases(&self) -> &IndexMap<String, TypeDesc> {
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
