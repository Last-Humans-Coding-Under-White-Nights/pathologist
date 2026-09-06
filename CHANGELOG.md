# Changelog

All notable changes to `trace` are documented in this file.

## Unreleased

### Added

- Source revision, dirty-state, and build-date metadata in `trace --version` and exported databases.
- Explicit database schema-version metadata.
- Validated, version-tagged GitHub releases alongside the rolling `master-latest` prerelease.
- OpenHarmony IPC proxy-to-stub call edges for matching `SendRequest` methods.

### Changed

- Database schema is now **v2**: `analysis_run` carries a `schema_version` column. Databases
  written by earlier versions have no such column and report themselves through the existing
  stale-schema errors.
- Database schema is now **v3**: `call_edges.call_site_id` is nullable for synthetic edges and
  `call_edges.caller_fn_id` records their caller independently of a source call site. Inspecting
  call data from an older database reports an actionable re-analysis message.

### Fixed

- Operator names containing `<` are no longer truncated to the bare keyword `operator`.
  `operator<`, `operator<=`, `operator<<` and `operator<=>` had their `<...>` stripped as if it
  were a template-argument list, so a class's comparison operators collapsed into one symbol and
  those spelled as declarations were dropped entirely. Only what directly follows the keyword is
  read as part of the operator, so a conversion to a template type drops its arguments the way
  every other name does. A conversion operator's target keeps its arguments, though — see
  below — so `operator Vec<int>` and `operator ns::Vec<int>` are one member by scope alone.
- C++ conversion operators (`operator T()`) are indexed as `Cls::operator T` instead of being
  dropped (declarations) or named after a declarator fragment such as `Cls::()const`
  (definitions). A target type that is itself qualified (`operator std::string`) keeps the
  member in its class, one that is a function pointer keeps its `(*)`, and a definition records
  the type it converts to as its return type — subject to the existing rule that a class
  prototype's return type wins the merge with an out-of-line definition. The target drops the
  scopes the member itself sits in, so how far the author had to spell it out no longer decides
  which member it is: `ns::Handle::operator ns::S` and the in-class `operator S` are one symbol,
  where before the out-of-line definition stranded its declaration as an undefined phantom.
  Scopes the member does *not* sit in are kept, since no spelling of the declaration could have
  elided them — `C::operator a::S` and `C::operator b::S` stay two members, as do
  `operator Vec<int>` and `operator Vec<double>`, whose target arguments are likewise kept.
- A conversion operator to a function pointer records a function type. `operator void (*)()`
  lowered to a bare `Ptr(Void)`, indistinguishable from a conversion to `void *`, so nothing
  downstream could see the target as callable; it is now `Ptr(FnPtr{..})` — the same descriptor
  the `typedef void (*FP)(); operator FP()` spelling of that type already produced, so the two
  spellings intern as one type.
- A function whose return type is preceded by an unknown attribute macro (`FFI_EXPORT T f();`,
  with no `#define` in the include path) is indexed under its own name rather than under the
  return type glued to it, which hid the function from every call site. tree-sitter recovers
  such a declaration as a qualified name with a missing `::` at namespace scope, and as an
  `ERROR` node holding the leftover type inside a class body. A definition whose own name is
  qualified (`FFI_EXPORT void C::M() {}`) recovers a third way — a real `::`, with the class
  segment parked in an `ERROR` node — and was indexed as `void C::M`, leaving the body
  unreachable behind a phantom `C::M` that every call site resolved to. A macro that *trails*
  the declarator (`int j() const NOEXCEPT;`, `virtual int m() OVERRIDE;`) puts the declarator
  itself in the `ERROR` node and the macro beside it, so the member was named after the macro
  and a class annotating all its members alike collapsed into one symbol. All four are handled.
- An unknown attribute macro in front of a *conversion* operator costs it the `operator_cast`
  the two paths above key on: the keyword is stranded in an `ERROR` and the type it converts to
  is left standing where the declared name belongs. In a class body
  (`MACRO operator ns::S() const;`) the member was indexed as `C::S`, a name that collides with
  the class `S` itself and matches no declaration of the real member; out of one
  (`EXPORT C::operator int() {}`) the leading `C::` was read as the leftover-type half of a
  fabricated qualification and cut off, so the definition escaped to global scope as
  `operator int` and left its declaration stranded and undefined. Both are now spelled
  `C::operator int` / `C::operator ns::S`, the same as every other path spells them — the
  `ERROR` swallows the target's own scope along with the keyword, so it is read back from there
  rather than from the declarator, which holds only the last segment. The repair covers the
  in-class *definition* too, whose declarator likewise names the target rather than the member.
  A globally-qualified target (`operator ::ns::S`) keeps the space the keyword needs, and drops
  the leading `::` when what follows re-spells a scope the member sits in, so it meets the same
  member. On a target naming no scope of the member's, the `::` is kept: inside a `namespace n`
  that declares its own `S`, `operator ::S` converts to the global type and `operator S` to
  `n::S`, and collapsing the two put two bodies under one symbol. A pointer target
  (`MACRO operator char *() const;`) keeps a real `operator_name` and never had the problem —
  though see the phantom a macro *trailing* that shape used to declare, below.
- Both attribute-macro repairs above are found however deep the name nests them. A qualified
  name nests one `qualified_identifier` per scope it carries, and recovery leaves its mark at
  whichever level the fabricated segment landed on, so every scope either half of the name
  spells pushes that mark a level further from the top: `FFI_EXPORT n::S C::M() {}` was still
  indexed as `n::S C::M`, and `EXPORT ns::C::operator ns::S() {}` split into a defined
  `ns::C::operator ns::S` beside the undefined `ns::C::operator S` it should have merged with.
- A definition annotated by a trailing attribute macro (`void C::M() OVERRIDE {}`,
  `void M() ACQUIRE(mu_) {}`) is indexed under its own name. Only a *nullary* declarator
  recovers this way — with parameters it parses and the macro is the leftover — because
  `C::M()` is as good a call as it is a declarator, so tree-sitter parks the real declarator in
  an `ERROR` and hands the `declarator` field to the macro. The definition landed on the macro:
  a *defined* function called `OVERRIDE`, one per class that annotates a nullary member and all
  merging into one symbol, while the real member stayed undefined and its body unreachable.
- A member wearing an unknown macro on *both* sides
  (`EXPORT_API int Get(long) GUARDED_BY(mu_);`) is named by its declarator. tree-sitter puts the
  leftover return type and the real declarator in the *same* `ERROR` rather than one in it and
  one beside it, so the rule that tells the leading repair from the trailing one — does this
  `ERROR` hold a declarator? — said yes and the walk read the whole node, taking the leftover
  type first. Every member sharing a return type collapsed into `Cls::int` / `Cls::void`, and
  the real members survived only as externals synthesized by their call sites. Only the
  `ERROR`'s declarators are read now. A *conversion* operator wearing both
  (`EXPORT_API operator int() const GUARDED_BY(m);`) has its target swallowed by the same
  `ERROR`, with the trailing macro left outside it, so it was named `Cls::operator GUARDED_BY`;
  a declarator inside the `ERROR` is the target whenever there is one.
- A macro-annotated conversion operator keeps the whole spelling of its target, not just the
  target's last segment. The declarator the `ERROR` parks the target in was *walked*, and
  walking a declarator yields the one identifier it is named by — so a qualified target lost its
  own scope (`Cls::operator S` where every unannotated spelling is `Cls::operator ns::S`, and
  `S` collides with the class of that name), a template target lost its arguments
  (`Cls::operator Vec`, merging `Vec<int>` with `Vec<double>` and meeting neither's plain
  declaration), and a function-pointer target lost its `(*)` (`Cls::operator int`, the name of
  the class's conversion *to* `int`, so one symbol held two unrelated members). Only conversions
  to a primitive came out right, the walk's answer coinciding with the full target there. The
  target is now read from the source text between the keyword and the end of the declarator the
  member's own parameter list hangs off, so every target kind spells the same under a leading
  macro, a trailing one, or both, and meets the unannotated declaration and the out-of-class
  definition of the same member. A multi-word primitive target
  (`MACRO operator unsigned long() const;`) is recovered as loose keywords with no declarator
  anywhere, so the member-vs-data test read it as a data field and dropped it, or — with a
  trailing macro to fall through to — named it `C::operator unsigned long()const GUARDED_BY`; the
  keyword opening an `ERROR` now marks a conversion operator whatever the `ERROR` holds, and the
  target still ends where the parameter list starts. A *globally* qualified target behind a
  leading macro (`MACRO operator ::ns::S() const;`) is the one shape still not repaired: its
  `ERROR` lands at class-body level rather than inside the member, out of reach of the member
  walk. Recorded in `docs/ANALYSIS.md`.
- A macro *trailing* a conversion operator to a pointer or reference
  (`EXPORT_API operator Payload *() const GUARDED_BY(m);`) no longer declares a phantom member.
  That target kind is the one recovery keeps a real `function_declarator` for, and the cost is
  paid elsewhere: the member's `;` goes *missing* and the trailing macro is parked after it as a
  class-body `declaration` of its own, which registered an undefined `Cls::GUARDED_BY` — one per
  class annotating such a member, and the symbol every call site on any annotated member resolved
  to. A member closed by a missing `;` is one the author wrote no `;` after, so a `declaration`
  following it is the rest of that member and declares nothing. A genuinely separate member after
  a missing `;` recovers as a `field_declaration`, not a `declaration`, so the rule does not reach
  it; the one shape it does swallow is a ctor declaration after a member whose `;` the author
  actually forgot, which is not valid C++ either way.
- A member carrying a standard or GNU attribute (`[[nodiscard]]`, `[[gnu::pure]]`,
  `__attribute__((pure))`) keeps its own name. These parse cleanly — no error recovery involved
  — but each holds an identifier in front of the declaration and the member walk took it, so
  every annotated member of a class collapsed into one `Cls::nodiscard`. Conversion operators
  made this reachable for the first time, their declarations having only begun to register
  above.
- Multi-word operator names (`operator new`, `operator delete`) keep the space that separates
  their words; they were exported as `operatornew` / `operatordelete`. Call sites still resolve
  to them: the guard that decides which unresolved callee becomes a synthesized `external`
  rejects names containing a space — a rule calibrated to the old invariant that no name had
  one — and now exempts the space an operator keyword carries, so `::operator new(n)` keeps its
  callee and its edge.
- A macro-annotated destructor (`MACRO ~D();`) is indexed as `D::~D`. Recovery strands the `~`
  alone in an `ERROR` and leaves `D` standing as the declarator, so the member was filed under
  the *constructor* name `D::D` and classified `MethodKind::Ctor` — which dropped it from the
  override set `delete p` expands over, silently losing virtual-destructor edges.
- A member behind `__declspec(dllexport)` / `__declspec(dllimport)` keeps its own name. MSVC's
  spelling of the attribute collapse above hides in `ms_declspec_modifier`, which the standard
  and GNU attribute guard did not cover.
- A conversion operator whose target is a template type survives a leading macro
  (`MACRO operator Vec<int>() const;`). Recovery leaves the target's argument list attached to
  the declarator, making it a `template_method` that the member-vs-data test did not recognise,
  so the member was read as a data field and dropped from the index entirely.
- A pointer- or reference-returning definition behind a trailing macro
  (`void *C::P() OVERRIDE {}`) is indexed under its own name. The return type's pointer wraps
  the declarator, so the parked-declarator repair — which looked only at the definition's own
  children — never saw the `ERROR` one level down, and the body stayed under the macro.
- `TypeTable::int()` returned the id of `Bool`, so every entity built on the default scalar type
  — a function with no readable return type, a synthesized temporary — carried `bool` internally.
  `TypeTable::resolve_type_id` had drifted the same way, falling back to a raw `TypeId(5)` for
  a type the table never interned: that named `Long`, where the function means the `Unknown`
  placeholder. Every one of these is a non-pointer scalar, so no points-to result changes; the
  descriptors are simply now the ones the accessors name. `unknown()` joins `void()` and
  `int()` as a named accessor, and a unit test pins each of the three to its own type — plus one
  that pins the fallback itself, which no accessor test can reach. None of the three names a raw
  index any more: each asks the intern table for the id its descriptor was interned under, so
  reordering or extending the prelude cannot silently re-point them the way it did these two.
