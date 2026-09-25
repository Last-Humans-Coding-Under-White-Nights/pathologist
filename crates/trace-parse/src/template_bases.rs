//! Subclasses through template-parameter bases (docs/ANALYSIS.md,
//! "Template-parameter bases"): `class S : public IRemoteStub<IFoo>` with
//! `template<class I> class IRemoteStub : public I` derives from `IFoo`.

use std::collections::{HashSet, VecDeque};

use trace_ir::{ClassTemplate, FileId, Program};

use crate::lower::{
    held_class_in_declaration_scope, normalize_qualified, receiver_lookup_name, sanitize_type_name,
    template_arguments, template_tail,
};

/// Nesting depth followed through dependent bases; deeper chains stop here.
const MAX_TEMPLATE_BASE_DEPTH: usize = 8;

/// `spelling` with each whole-identifier parameter replaced by its argument.
///
/// An identifier after `::` names a member of a scope, never the parameter.
/// A pack expansion (`Ts...`, `X<Ts>...`) whose pattern mentions parameter
/// `pack` is one copy of the pattern per element, `arguments[pack..]`,
/// comma-joined; an empty pack drops it with its separator.
/// `None` when a parameter the spelling uses has no argument.
pub(crate) fn substitute_parameters(
    spelling: &str,
    parameters: &[Option<String>],
    pack: Option<usize>,
    arguments: &[String],
) -> Option<String> {
    let expansion = pack.and_then(|pack| {
        let name = parameters.get(pack)?.as_deref()?;
        pack_expansions(spelling)
            .find(|&(start, dots)| mentions_word(&spelling[start..dots], name))
            .map(|found| (pack, found))
    });
    let Some((pack, (start, dots))) = expansion else {
        return substitute_words(spelling, parameters, arguments);
    };
    let pattern = &spelling[start..dots];
    let copies = arguments
        .get(pack..)?
        .iter()
        .map(|element| {
            let mut list = arguments[..pack].to_vec();
            list.push(element.clone());
            substitute_words(pattern, parameters, &list)
        })
        .collect::<Option<Vec<_>>>()?;
    let (mut head, mut tail) = (&spelling[..start], &spelling[dots + 3..]);
    if copies.is_empty() {
        // `B<X, Ts...>` or `B<Ts..., X>` with no `Ts`.
        match head.trim_end().strip_suffix(',') {
            Some(before) => head = before,
            None => tail = tail.trim_start().trim_start_matches(',').trim_start(),
        }
    }
    let mut out = substitute_words(head, parameters, arguments)?;
    out.push_str(&copies.join(", "));
    out.push_str(&substitute_parameters(
        tail,
        parameters,
        Some(pack),
        arguments,
    )?);
    Some(out)
}

/// [`substitute_parameters`] for a spelling with no pack expansion.
fn substitute_words(
    spelling: &str,
    parameters: &[Option<String>],
    arguments: &[String],
) -> Option<String> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(spelling.len());
    let mut rest = spelling;
    while let Some(start) = rest.find(is_ident) {
        let (before, tail) = rest.split_at(start);
        out.push_str(before);
        let end = tail.find(|c: char| !is_ident(c)).unwrap_or(tail.len());
        let word = &tail[..end];
        let qualified = out.trim_end().ends_with("::");
        match parameters.iter().position(|p| p.as_deref() == Some(word)) {
            Some(i) if !qualified => out.push_str(arguments.get(i)?),
            _ => out.push_str(word),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    Some(out)
}

/// Each pack expansion in `spelling`, as the byte range of its pattern and
/// the offset of its `...`: the pattern is the template argument (or whole
/// spelling) the `...` closes, `X<Ts>` in `B<A, X<Ts>...>`.
fn pack_expansions(spelling: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    spelling.match_indices("...").map(move |(dots, _)| {
        let mut depth = 0i32;
        let mut start = 0;
        for (i, c) in spelling[..dots].char_indices().rev() {
            match c {
                '>' | ')' => depth += 1,
                '<' | '(' if depth > 0 => depth -= 1,
                '<' | '(' | ',' if depth == 0 => {
                    start = i + 1;
                    break;
                }
                _ => {}
            }
        }
        let start =
            start + (spelling[start..dots].len() - spelling[start..dots].trim_start().len());
        (start, dots)
    })
}

/// Whether `spelling` mentions `name` as a whole identifier.
fn mentions_word(spelling: &str, name: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    spelling.match_indices(name).any(|(at, _)| {
        !spelling[..at].ends_with(is_ident) && !spelling[at + name.len()..].starts_with(is_ident)
    })
}

/// Add `derived → base` for every class `spelling`, a concrete templated base
/// of `derived` declared in `scope`, reaches through a parameter. Lowering
/// calls it as it records the base, so member lookups later in the unit see
/// the edge. `file` is the file `derived` is defined in, `unit` the sorted
/// files of the translation unit lowering it (its `#include` closure), whose
/// file-local templates it sees, and `anonymous` says `derived` is
/// file-local too.
pub(crate) fn add_parameter_bases(
    program: &mut Program,
    derived: &str,
    spelling: &str,
    scope: &str,
    (file, unit): (FileId, &[FileId]),
    anonymous: bool,
) {
    if program.class_templates.is_empty() && program.anonymous_class_templates.is_empty() {
        return;
    }
    for base in parameter_bases(program, spelling, scope, Templates::Unit { file, unit }) {
        if anonymous {
            program.add_anonymous_base(derived, file, &base);
        }
        program.add_inheritance(derived, &base);
    }
}

/// [`add_parameter_bases`] for every concrete templated base once merged,
/// for a class template another unit defines. A file-local template is only
/// ever expanded while lowering its own file, so here none is consulted and
/// none hides a linked template of its name.
pub(crate) fn expand_template_parameter_bases(program: &mut Program) {
    if program.class_templates.is_empty() {
        return;
    }
    let mut edges = Vec::new();
    for fact in program.template_bases.iter().filter(|f| !f.is_dependent) {
        let reached = parameter_bases(
            program,
            &fact.spelling,
            &fact.declaration_scope,
            Templates::Merged,
        );
        edges.extend(reached.into_iter().map(|base| (fact.derived.clone(), base)));
    }
    for (derived, base) in edges {
        program.add_inheritance(&derived, &base);
    }
}

/// Which class templates a lookup sees.
#[derive(Clone, Copy)]
enum Templates<'u> {
    /// While lowering a class defined in `file` of a translation unit whose
    /// files are `unit` (sorted): a file-local (anonymous-namespace) template
    /// is visible to its whole unit, so the class's own file's template of
    /// a name comes first, then one from another file of the unit, then the
    /// linked one. The unit's program may hold templates of files it never
    /// includes (header facts reach it through the include graph, which
    /// ignores `#if`); they are not seen.
    Unit { file: FileId, unit: &'u [FileId] },
    /// After merge: only templates with linkage. A file-local template's name
    /// says nothing about which file's template a class derives from, and the
    /// class's own file expanded it while lowering.
    Merged,
}

/// The classes `spelling`'s template reaches through parameter bases,
/// following dependent template bases with their arguments substituted.
/// Arguments as written resolve in `scope`, the derived class's declaration
/// scope; defaults were qualified in the template's.
///
/// Breadth first, so each instantiation is first reached, and expanded once,
/// at its least depth: a self-referential template stops, and one reached
/// deep first is still expanded from a shallower path.
fn parameter_bases(
    program: &Program,
    spelling: &str,
    scope: &str,
    templates: Templates<'_>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut reached = HashSet::from([spelling.to_owned()]);
    let mut pending = VecDeque::from([(spelling.to_owned(), 0)]);
    while let Some((spelling, depth)) = pending.pop_front() {
        let Some(template) = class_template(program, &spelling, scope, templates) else {
            continue;
        };
        let written = innermost_arguments(&spelling);
        for base in &template.dependent_bases {
            for arguments in argument_lists(template, base, written.clone()) {
                let Some(substituted) =
                    substitute_parameters(base, &template.parameters, template.pack, &arguments)
                else {
                    continue;
                };
                if names_parameter(base, &template.parameters) {
                    // `: public I` or `: public B<T>` — the argument,
                    // instantiated where the base is a template template
                    // parameter, is the base.
                    let name = sanitize_type_name(&substituted);
                    if let Some(class) = held_class_in_declaration_scope(program, scope, &name) {
                        let class = receiver_lookup_name(&class).into_owned();
                        if !out.contains(&class) {
                            out.push(class);
                        }
                    }
                }
                if substituted.contains('<')
                    && depth + 1 < MAX_TEMPLATE_BASE_DEPTH
                    && reached.insert(substituted.clone())
                {
                    pending.push_back((substituted, depth + 1));
                }
            }
        }
    }
    out
}

/// Whether `base`'s class is itself a parameter (`I`, or `B` in `B<T>`).
fn names_parameter(base: &str, parameters: &[Option<String>]) -> bool {
    let head = normalize_qualified(base);
    parameters
        .iter()
        .any(|p| p.as_deref() == Some(head.as_str()))
}

/// The class template `spelling` instantiates: by the name lowering recorded,
/// else as `scope` sees it (`Outer<A>::In<B>` is recorded as written).
fn class_template<'p>(
    program: &'p Program,
    spelling: &str,
    scope: &str,
    templates: Templates<'_>,
) -> Option<&'p ClassTemplate> {
    let named = |name: &str| {
        let file_local = match templates {
            Templates::Unit { file, unit } => {
                program
                    .anonymous_class_templates
                    .get(name)
                    .and_then(|files| {
                        // Well-formed C++ has one per name in a unit; several
                        // (ill-formed) resolve to the lowest `FileId`, as the
                        // map orders them.
                        files.get(&file).or_else(|| {
                            files
                                .iter()
                                .find(|(f, _)| unit.binary_search(f).is_ok())
                                .map(|(_, template)| template)
                        })
                    })
            }
            Templates::Merged => None,
        };
        file_local.or_else(|| program.class_templates.get(name))
    };
    let name = normalize_qualified(spelling);
    let name = name.strip_prefix("::").unwrap_or(&name);
    named(name).or_else(|| named(&held_class_in_declaration_scope(program, scope, name)?))
}

/// The arguments of the class `spelling` names: its last argument list, so
/// `Outer<A>::In<B>` gives `In`'s `B`, not `Outer`'s `A`.
fn innermost_arguments(spelling: &str) -> Vec<String> {
    let mut list = spelling;
    while template_tail(list).contains('<') {
        list = template_tail(list);
    }
    template_arguments(list)
}

/// The arguments `template`'s parameters bind for `base`, by position: an
/// omitted argument takes its default. A parameter pack that `base` expands
/// inside its arguments (`B<Ts...>`, `B<X<Ts>...>`) binds every remaining
/// argument in one list, for [`substitute_parameters`] to expand; one that is
/// a base itself (`Is...`) binds each in turn, one list per element.
fn argument_lists(template: &ClassTemplate, base: &str, written: Vec<String>) -> Vec<Vec<String>> {
    let mut arguments = written;
    for default in template.defaults.iter().skip(arguments.len()) {
        match default {
            Some(default) => arguments.push(default.clone()),
            None => break,
        }
    }
    let in_place = |pack: usize| {
        !names_parameter(base, &template.parameters)
            && template.parameters[pack].as_deref().is_some_and(|name| {
                pack_expansions(base).any(|(start, dots)| mentions_word(&base[start..dots], name))
            })
    };
    match template.pack {
        Some(pack) if pack < template.parameters.len() && in_place(pack) => vec![arguments],
        Some(pack) if arguments.len() > pack => arguments[pack..]
            .iter()
            .map(|element| {
                let mut list = arguments[..pack].to_vec();
                list.push(element.clone());
                list
            })
            .collect(),
        _ => vec![arguments],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_ir::Program;

    fn params(names: &[&str]) -> Vec<Option<String>> {
        names.iter().map(|n| Some(n.to_string())).collect()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn parameter_bases_in_unit(program: &Program, spelling: &str, scope: &str) -> Vec<String> {
        parameter_bases(
            program,
            spelling,
            scope,
            Templates::Unit {
                file: FileId(0),
                unit: &[],
            },
        )
    }

    fn add_template(program: &mut Program, class: &str, parameters: &[&str], bases: &[&str]) {
        let mut fact = ClassTemplate {
            parameters: params(parameters),
            defaults: vec![None; parameters.len()],
            ..ClassTemplate::default()
        };
        bases.iter().for_each(|base| fact.add_dependent_base(base));
        program.add_class_template_fact(class, None, &fact);
    }

    #[test]
    fn substitutes_whole_identifiers_only() {
        assert_eq!(
            substitute_parameters("I", &params(&["I"]), None, &args(&["IFoo"])).as_deref(),
            Some("IFoo")
        );
        assert_eq!(
            substitute_parameters("Layer<T>", &params(&["T"]), None, &args(&["IBar"])).as_deref(),
            Some("Layer<IBar>")
        );
        assert_eq!(
            substitute_parameters(
                "Pair<T, TT>",
                &params(&["T", "TT"]),
                None,
                &args(&["A", "B"])
            )
            .as_deref(),
            Some("Pair<A, B>")
        );
        assert_eq!(
            substitute_parameters("ns::T<T>", &params(&["T"]), None, &args(&["X"])).as_deref(),
            Some("ns::T<X>"),
            "a qualified `ns::T` is not the parameter"
        );
    }

    #[test]
    fn missing_argument_does_not_substitute() {
        assert_eq!(
            substitute_parameters("U", &params(&["T", "U"]), None, &args(&["A"])),
            None,
            "defaulted or omitted"
        );
    }

    #[test]
    fn expands_parameter_bases_through_nested_templates_in_declaration_scope() {
        let mut program = Program::default();
        for cls in [
            "IBar",
            "svc::IFoo",
            "IFoo",
            "Layer",
            "Wrapper",
            "BarImpl",
            "svc::Scoped",
            "OHOS::IRemoteStub",
        ] {
            program.types.define_struct(cls);
        }
        add_template(&mut program, "OHOS::IRemoteStub", &["I"], &["I"]);
        add_template(&mut program, "Layer", &["I"], &["I"]);
        add_template(&mut program, "Wrapper", &["T"], &["Layer<T>"]);
        program.add_template_base("BarImpl", "Wrapper<IBar>", "", false);
        program.add_template_base("svc::Scoped", "OHOS::IRemoteStub<IFoo>", "svc", false);
        program.add_template_base("Generic", "OHOS::IRemoteStub<T>", "", true);

        expand_template_parameter_bases(&mut program);

        assert!(program.derives_from("BarImpl", "IBar"));
        assert!(program.derives_from("svc::Scoped", "svc::IFoo"));
        assert!(!program.derives_from("svc::Scoped", "IFoo"));
        assert!(
            program.bases_of("Generic").is_empty(),
            "dependent facts name no concrete class"
        );
    }

    #[test]
    fn expansion_stops_on_self_referential_templates() {
        let mut program = Program::default();
        program.types.define_struct("Loop");
        add_template(&mut program, "Loop", &["T"], &["Loop<T>"]);
        program.add_template_base("User", "Loop<int>", "", false);
        expand_template_parameter_bases(&mut program); // must terminate
        assert!(program.bases_of("User").is_empty());
    }

    #[test]
    fn a_pack_binds_every_remaining_argument() {
        let mut program = Program::default();
        for cls in ["IA", "IB", "IC"] {
            program.types.define_struct(cls);
        }
        let mut multi = ClassTemplate {
            parameters: params(&["T", "Is"]),
            defaults: vec![None, None],
            pack: Some(1),
            ..ClassTemplate::default()
        };
        multi.add_dependent_base("T");
        multi.add_dependent_base("Is");
        program.add_class_template_fact("Multi", None, &multi);
        assert_eq!(
            parameter_bases_in_unit(&program, "Multi<IA,IB,IC>", ""),
            ["IA", "IB", "IC"]
        );
        assert_eq!(
            parameter_bases_in_unit(&program, "Multi<IA>", ""),
            ["IA"],
            "an empty pack"
        );
    }

    #[test]
    fn an_omitted_argument_takes_its_qualified_default() {
        let mut program = Program::default();
        for cls in ["IA", "tmpl::IDef", "svc::IDef"] {
            program.types.define_struct(cls);
        }
        let mut def = ClassTemplate {
            parameters: params(&["I", "D"]),
            defaults: vec![None, Some("::tmpl::IDef".to_string())],
            ..ClassTemplate::default()
        };
        def.add_dependent_base("I");
        def.add_dependent_base("D");
        program.add_class_template_fact("tmpl::Def", None, &def);
        assert_eq!(
            parameter_bases_in_unit(&program, "tmpl::Def<IA>", "svc"),
            ["IA", "tmpl::IDef"],
            "the default names the template's class, not the derived scope's"
        );
    }

    #[test]
    fn a_nested_template_binds_its_own_argument_list() {
        let mut program = Program::default();
        for cls in ["IA", "IC", "Outer", "Outer::In"] {
            program.types.define_struct(cls);
        }
        add_template(&mut program, "Outer::In", &["B"], &["B"]);
        assert_eq!(
            parameter_bases_in_unit(&program, "Outer<IA>::In<IC>", ""),
            ["IC"]
        );
    }

    #[test]
    fn each_instantiation_is_expanded_once() {
        let mut program = Program::default();
        program.types.define_struct("IA");
        // Fan(T) : Fan2<T>, Fan2<T> twice over: without the walked set this
        // is exponential in the depth.
        add_template(&mut program, "Fan", &["T"], &["Fan<T>", "Pair<T,T>", "T"]);
        add_template(
            &mut program,
            "Pair",
            &["A", "B"],
            &["Fan<A>", "Fan<B>", "A"],
        );
        assert_eq!(parameter_bases_in_unit(&program, "Fan<IA>", ""), ["IA"]);
    }

    #[test]
    fn a_template_template_parameter_base_names_the_argument_template() {
        let mut program = Program::default();
        for cls in ["IC", "Layer", "Wrap"] {
            program.types.define_struct(cls);
        }
        add_template(&mut program, "Layer", &["I"], &["I"]);
        add_template(&mut program, "Wrap", &["B", "T"], &["B<T>"]);
        assert_eq!(
            parameter_bases_in_unit(&program, "Wrap<Layer,IC>", ""),
            ["Layer", "IC"]
        );
    }

    /// `Twin<T> : T` in file 1 and `Twin<T> : Pair<T, IB>` in file 0, each
    /// file-local, beside a linked `Twin<T> : Pair<IB, T>`.
    fn twins() -> Program {
        let mut program = Program::default();
        for cls in ["IA", "IB"] {
            program.types.define_struct(cls);
        }
        add_template(&mut program, "Pair", &["A", "B"], &["A", "B"]);
        for (file, base) in [(0, "Pair<T, IB>"), (1, "T")] {
            let mut fact = ClassTemplate {
                parameters: params(&["T"]),
                defaults: vec![None],
                ..ClassTemplate::default()
            };
            fact.add_dependent_base(base);
            program.add_class_template_fact("Twin", Some(FileId(file)), &fact);
        }
        program
    }

    #[test]
    fn a_file_local_template_is_seen_from_its_own_file_only() {
        let mut program = twins();
        add_parameter_bases(&mut program, "One", "Twin<IA>", "", (FileId(1), &[]), false);
        assert_eq!(program.bases_of("One"), ["IA"], "file 1's own `Twin`");
        add_parameter_bases(
            &mut program,
            "Zero",
            "Twin<IA>",
            "",
            (FileId(0), &[]),
            false,
        );
        assert_eq!(
            program.bases_of("Zero"),
            ["IA", "IB"],
            "file 0's own `Twin`"
        );
        add_parameter_bases(
            &mut program,
            "Other",
            "Twin<IA>",
            "",
            (FileId(2), &[]),
            false,
        );
        assert!(
            program.bases_of("Other").is_empty(),
            "no file-local `Twin` of file 2, and no linked one"
        );
    }

    #[test]
    fn a_file_local_template_is_seen_from_its_whole_unit() {
        let mut program = twins();
        let unit = [FileId(1), FileId(2)];
        add_parameter_bases(
            &mut program,
            "Inc",
            "Twin<IA>",
            "",
            (FileId(2), &unit),
            false,
        );
        assert_eq!(program.bases_of("Inc"), ["IA"], "the included file 1's");
        let unit = [FileId(0), FileId(1), FileId(2)];
        add_parameter_bases(
            &mut program,
            "Both",
            "Twin<IA>",
            "",
            (FileId(2), &unit),
            false,
        );
        assert_eq!(
            program.bases_of("Both"),
            ["IA", "IB"],
            "ill-formed: the lowest file's"
        );
        add_template(&mut program, "Twin", &["T"], &["Pair<IB, T>"]);
        add_parameter_bases(
            &mut program,
            "Out",
            "Twin<IA>",
            "",
            (FileId(3), &[FileId(3)]),
            false,
        );
        assert_eq!(
            program.bases_of("Out"),
            ["IB", "IA"],
            "a file outside the unit is never seen"
        );
    }

    #[test]
    fn a_file_local_template_never_hides_a_linked_one() {
        let mut program = twins();
        add_template(&mut program, "Twin", &["T"], &["Pair<IB, T>"]);
        add_parameter_bases(
            &mut program,
            "Linked",
            "Twin<IA>",
            "",
            (FileId(2), &[]),
            false,
        );
        assert_eq!(program.bases_of("Linked"), ["IB", "IA"]);
        add_parameter_bases(&mut program, "Own", "Twin<IA>", "", (FileId(1), &[]), false);
        assert_eq!(program.bases_of("Own"), ["IA"], "the file's own is nearer");

        program.add_template_base("Merged", "Twin<IA>", "", false);
        expand_template_parameter_bases(&mut program);
        assert_eq!(
            program.bases_of("Merged"),
            ["IB", "IA"],
            "once merged, only the linked `Twin`"
        );
    }

    #[test]
    fn a_pack_forwarded_inside_arguments_substitutes_all_of_it() {
        let mut program = Program::default();
        for cls in ["IA", "IB", "IC"] {
            program.types.define_struct(cls);
        }
        let pack = |bases: &[&str]| {
            let mut fact = ClassTemplate {
                parameters: params(&["Ts"]),
                defaults: vec![None],
                pack: Some(0),
                ..ClassTemplate::default()
            };
            bases.iter().for_each(|base| fact.add_dependent_base(base));
            fact
        };
        program.add_class_template_fact("B", None, &pack(&["Ts"]));
        program.add_class_template_fact("Pack", None, &pack(&["B<Ts...>"]));
        program.add_class_template_fact("Mixed", None, &pack(&["B<IC, Ts...>"]));
        assert_eq!(
            parameter_bases_in_unit(&program, "Pack<IA,IB>", ""),
            ["IA", "IB"]
        );
        assert_eq!(
            parameter_bases_in_unit(&program, "Mixed<IA>", ""),
            ["IC", "IA"]
        );
        assert_eq!(
            parameter_bases_in_unit(&program, "Mixed<>", ""),
            ["IC"],
            "an empty pack"
        );
    }

    #[test]
    fn a_pack_expansion_pattern_copies_the_pattern_per_element() {
        let pack = params(&["Ts"]);
        let joined = |base: &str, elements: &[&str]| {
            let fact = ClassTemplate {
                parameters: pack.clone(),
                defaults: vec![None],
                pack: Some(0),
                ..ClassTemplate::default()
            };
            argument_lists(&fact, base, args(elements))
                .into_iter()
                .map(|arguments| substitute_parameters(base, &pack, Some(0), &arguments))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            joined("B<X<Ts>...>", &["IA", "IB"]),
            [Some("B<X<IA>, X<IB>>".to_string())]
        );
        assert_eq!(
            joined("B<IC, X<Ts>...>", &[]),
            [Some("B<IC>".to_string())],
            "an empty pack drops the argument and its separator"
        );
        assert_eq!(joined("B<X<Ts>..., IC>", &[]), [Some("B<IC>".to_string())]);

        let mut program = Program::default();
        for cls in ["IA", "IB"] {
            program.types.define_struct(cls);
        }
        let mut b = ClassTemplate {
            parameters: params(&["Us"]),
            defaults: vec![None],
            pack: Some(0),
            ..ClassTemplate::default()
        };
        b.add_dependent_base("Us");
        program.add_class_template_fact("B", None, &b);
        add_template(&mut program, "X", &["T"], &["T"]);
        let mut d = ClassTemplate {
            parameters: pack.clone(),
            defaults: vec![None],
            pack: Some(0),
            ..ClassTemplate::default()
        };
        d.add_dependent_base("B<X<Ts>...>");
        program.add_class_template_fact("D", None, &d);
        assert_eq!(
            parameter_bases_in_unit(&program, "D<IA,IB>", ""),
            ["IA", "IB"]
        );
    }

    #[test]
    fn an_instantiation_reached_deep_first_is_still_expanded_from_a_shallower_path() {
        let mut program = Program::default();
        program.types.define_struct("IA");
        // `Top<T>` reaches `X<T>` through six links first, where `Y<T>` falls
        // past the depth cap, and then directly.
        add_template(&mut program, "Top", &["T"], &["C1<T>", "X<T>"]);
        for (i, next) in ["C2<T>", "C3<T>", "C4<T>", "C5<T>", "C6<T>", "X<T>"]
            .iter()
            .enumerate()
        {
            add_template(&mut program, &format!("C{}", i + 1), &["T"], &[next]);
        }
        add_template(&mut program, "X", &["T"], &["Y<T>"]);
        add_template(&mut program, "Y", &["T"], &["T"]);
        assert_eq!(parameter_bases_in_unit(&program, "Top<IA>", ""), ["IA"]);
    }
}
