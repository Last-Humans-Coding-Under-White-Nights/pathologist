//! Immutable C/C++ grammar metadata used during lowering.

use rustc_hash::FxHashMap;
use std::sync::LazyLock;
use tree_sitter::{Language, Node};

struct GrammarMetadata {
    language: Language,
    kinds: Vec<Option<&'static str>>,
    fields: FxHashMap<&'static str, u16>,
    f_declarator: Option<u16>,
    f_type: Option<u16>,
    f_name: Option<u16>,
    f_argument: Option<u16>,
    f_arguments: Option<u16>,
    f_function: Option<u16>,
    f_field: Option<u16>,
    f_body: Option<u16>,
    f_parameters: Option<u16>,
    f_value: Option<u16>,
    f_operator: Option<u16>,
    f_expression: Option<u16>,
    f_left: Option<u16>,
    f_right: Option<u16>,
    f_scope: Option<u16>,
    f_default_type: Option<u16>,
}

impl GrammarMetadata {
    fn new(language: Language) -> Self {
        let kinds = (0..language.node_kind_count() as u16)
            .map(|id| language.node_kind_for_id(id))
            .collect();
        let fields: FxHashMap<&'static str, u16> = (1..=language.field_count() as u16)
            .filter_map(|id| Some((language.field_name_for_id(id)?, id)))
            .collect();
        let f_declarator = fields.get("declarator").copied();
        let f_type = fields.get("type").copied();
        let f_name = fields.get("name").copied();
        let f_argument = fields.get("argument").copied();
        let f_arguments = fields.get("arguments").copied();
        let f_function = fields.get("function").copied();
        let f_field = fields.get("field").copied();
        let f_body = fields.get("body").copied();
        let f_parameters = fields.get("parameters").copied();
        let f_value = fields.get("value").copied();
        let f_operator = fields.get("operator").copied();
        let f_expression = fields.get("expression").copied();
        let f_left = fields.get("left").copied();
        let f_right = fields.get("right").copied();
        let f_scope = fields.get("scope").copied();
        let f_default_type = fields.get("default_type").copied();
        Self {
            language,
            kinds,
            fields,
            f_declarator,
            f_type,
            f_name,
            f_argument,
            f_arguments,
            f_function,
            f_field,
            f_body,
            f_parameters,
            f_value,
            f_operator,
            f_expression,
            f_left,
            f_right,
            f_scope,
            f_default_type,
        }
    }

    #[inline]
    fn kind(&self, id: u16, decode: impl FnOnce() -> &'static str) -> &'static str {
        self.kinds
            .get(id as usize)
            .copied()
            .flatten()
            .unwrap_or_else(decode)
    }
}

static C: LazyLock<GrammarMetadata> =
    LazyLock::new(|| GrammarMetadata::new(tree_sitter_c::LANGUAGE.into()));
static CPP: LazyLock<GrammarMetadata> =
    LazyLock::new(|| GrammarMetadata::new(tree_sitter_cpp::LANGUAGE.into()));

#[inline]
fn metadata(node: &Node<'_>) -> Option<&'static GrammarMetadata> {
    let language = node.language();
    if *language == CPP.language {
        Some(&CPP)
    } else if *language == C.language {
        Some(&C)
    } else {
        None
    }
}

/// Decode fixed grammar names once, and resolve field names once per grammar.
/// The node's own language selects the table, including when a worker switches
/// between C and C++. Unknown grammars and special symbols use tree-sitter.
pub(crate) trait NodeMetadata<'tree> {
    fn cached_kind(&self) -> &'static str;
    fn cached_field(&self, name: &str) -> Option<Node<'tree>>;
}

impl<'tree> NodeMetadata<'tree> for Node<'tree> {
    #[inline]
    fn cached_kind(&self) -> &'static str {
        metadata(self).map_or_else(|| self.kind(), |m| m.kind(self.kind_id(), || self.kind()))
    }

    #[inline]
    fn cached_field(&self, name: &str) -> Option<Node<'tree>> {
        match metadata(self) {
            Some(m) => {
                let id = match name {
                    "declarator" => m.f_declarator,
                    "type" => m.f_type,
                    "name" => m.f_name,
                    "argument" => m.f_argument,
                    "arguments" => m.f_arguments,
                    "function" => m.f_function,
                    "field" => m.f_field,
                    "body" => m.f_body,
                    "parameters" => m.f_parameters,
                    "value" => m.f_value,
                    "operator" => m.f_operator,
                    "expression" => m.f_expression,
                    "left" => m.f_left,
                    "right" => m.f_right,
                    "scope" => m.f_scope,
                    "default_type" => m.f_default_type,
                    _ => m.fields.get(name).copied(),
                }?;
                self.child_by_field_id(id)
            }
            None => self.child_by_field_name(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_source_with_lang, SourceLang};

    #[test]
    fn known_kind_names_need_no_repeated_decoding() {
        for language in [
            tree_sitter_c::LANGUAGE.into(),
            tree_sitter_cpp::LANGUAGE.into(),
        ] {
            let metadata = GrammarMetadata::new(language);
            for id in 0..metadata.language.node_kind_count() as u16 {
                let expected = metadata.language.node_kind_for_id(id).unwrap();
                assert_eq!(
                    metadata.kind(id, || panic!("decoded a known grammar name")),
                    expected
                );
            }
            // ERROR is outside the normal symbol table.
            assert_eq!(metadata.kind(u16::MAX, || "ERROR"), "ERROR");
        }
    }

    #[test]
    fn metadata_matches_nodes_in_both_grammars_including_recovery() {
        let source = r#"
            struct S { unsigned : 0; int (*callback)(int); };
            int run(int x) { return x + 1; }
            namespace ns { template<class T> struct Box { virtual T get() final; }; }
            int broken( { return;
        "#;
        for lang in [SourceLang::C, SourceLang::Cpp] {
            let parsed = parse_source_with_lang(source, lang).unwrap();
            let language = parsed.tree.language();
            let mut nodes = vec![parsed.tree.root_node()];
            while let Some(node) = nodes.pop() {
                assert_eq!(node.cached_kind(), node.kind());
                for id in 1..=language.field_count() as u16 {
                    let name = language.field_name_for_id(id).unwrap();
                    assert_eq!(node.cached_field(name), node.child_by_field_name(name));
                }
                assert_eq!(node.cached_field("not_a_grammar_field"), None);
                nodes.extend(node.children(&mut node.walk()));
            }
        }
    }
}
