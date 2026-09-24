use crate::{FieldId, FnId, VarId};

/// Abstract value returned from a function body (may-analysis: union all return sites).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReturnFlow {
    AddrOfVar {
        src: VarId,
    },
    AddrOfFn {
        callee: FnId,
    },
    Copy {
        src: VarId,
    },
    /// Return value is whatever `callee` returns (expanded after all TUs are merged).
    Call {
        callee_name: String,
    },
}

/// Statement-level flow facts extracted during IR lowering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FlowConstraint {
    /// `dst = src` (pointer assignment)
    Copy { dst: VarId, src: VarId },
    /// `dst = &var`
    AddrOfVar { dst: VarId, src: VarId },
    /// `dst = &function`
    AddrOfFn { dst: VarId, callee: FnId },
    /// `dst = *src`
    Load { dst: VarId, src: VarId },
    /// `*dst = src` (store through pointer)
    Store { dst: VarId, src: VarId },
    /// `dst = src->field` or struct field address.
    /// `field_name` carries the human-readable name resolved during lowering;
    /// the solver uses it to reject pointees whose struct type has a different
    /// field at the same positional index.
    GepField {
        dst: VarId,
        base: VarId,
        field: FieldId,
        field_name: String,
    },
    /// Function-pointer array initializer: any subscript may target any listed callee.
    ArrayFnMember { array: VarId, callee: FnId },
    /// `dst = callee()` — callee resolved by name at PAG build in the scope
    /// of `caller`, the function the call is written in (`None` for a
    /// file-scope initializer). See `docs/ANALYSIS.md`, "Return-value flow".
    CallReturn {
        dst: VarId,
        callee_name: String,
        caller: Option<FnId>,
    },
    /// `dst = callee_var()` — callee resolved at analysis time from the
    /// function-pointer variable's points-to set (indirect / virtual calls).
    CallReturnIndirect { dst: VarId, callee_var: VarId },
    /// `dst = new T(...)` — allocate a fresh heap location and point `dst` to
    /// it, so the constructor's implicit `this` has concrete pointees.
    NewHeap { dst: VarId },
    /// `dst` points at the given string literal (interned; copies propagate).
    StringConst { dst: VarId, value: String },
    /// `sp->field`: step through a smart pointer's overloaded `->`/`*`. `src`
    /// holds the wrapper value; `dst` is the pointee-typed receiver, and its
    /// type is the one the solver admits locations by. See
    /// `docs/ANALYSIS.md`, "Smart-pointer unwrap".
    UnwrapPointer { dst: VarId, src: VarId },
}

impl FlowConstraint {
    /// Every variable the constraint names, destination first.
    pub fn vars(&self) -> impl Iterator<Item = VarId> {
        let (first, second) = match *self {
            FlowConstraint::Copy { dst, src }
            | FlowConstraint::Load { dst, src }
            | FlowConstraint::Store { dst, src }
            | FlowConstraint::AddrOfVar { dst, src }
            | FlowConstraint::UnwrapPointer { dst, src } => (dst, Some(src)),
            FlowConstraint::GepField { dst, base, .. } => (dst, Some(base)),
            FlowConstraint::CallReturnIndirect { dst, callee_var } => (dst, Some(callee_var)),
            FlowConstraint::ArrayFnMember { array, .. } => (array, None),
            FlowConstraint::AddrOfFn { dst, .. }
            | FlowConstraint::CallReturn { dst, .. }
            | FlowConstraint::NewHeap { dst }
            | FlowConstraint::StringConst { dst, .. } => (dst, None),
        };
        std::iter::once(first).chain(second)
    }
}

impl ReturnFlow {
    /// The variable the return names, if any.
    pub fn var(&self) -> Option<VarId> {
        match *self {
            ReturnFlow::AddrOfVar { src } | ReturnFlow::Copy { src } => Some(src),
            ReturnFlow::AddrOfFn { .. } | ReturnFlow::Call { .. } => None,
        }
    }
}
