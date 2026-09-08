//! Conservative GN evidence extraction, not GN evaluation or target resolution.
//! Only standalone string entries in direct `defines =/+= [...]` lists qualify.

/// Block functions whose `defines` apply to compiled sources. Cited by
/// `docs/GN_DEFINES.md`; any other call block is weaker evidence.
const TARGET_FUNCTIONS: &[&str] = &[
    "executable",
    "shared_library",
    "static_library",
    "source_set",
    "loadable_module",
    "ohos_shared_library",
    "ohos_static_library",
    "ohos_source_set",
    "ohos_executable",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

#[derive(Debug)]
pub struct Candidate {
    pub name: String,
    pub value: Option<String>,
    pub line: u32,
    pub conditions: Vec<String>,
    pub confidence: Confidence,
}

struct Token<'a> {
    raw: &'a str,
    start: usize,
    line: u32,
    string: Option<(String, bool)>,
}

impl Token<'_> {
    fn end(&self) -> usize {
        self.start + self.raw.len()
    }
}

fn lex(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let (mut i, mut line) = (0, 1);
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            line += u32::from(bytes[i] == b'\n');
            i += 1;
            continue;
        }
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let (start, token_line) = (i, line);
        let mut string = None;
        if bytes[i] == b'"' {
            i += 1;
            let mut value = Vec::new();
            let mut dynamic = false;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\'
                    && i + 1 < bytes.len()
                    && matches!(bytes[i + 1], b'"' | b'\\' | b'$')
                {
                    i += 1;
                } else if bytes[i] == b'$' {
                    dynamic = true;
                }
                line += u32::from(bytes[i] == b'\n');
                value.push(bytes[i]);
                i += 1;
            }
            if i == bytes.len() {
                break;
            }
            i += 1;
            string = Some((
                String::from_utf8(value).expect("GN string stays UTF-8"),
                dynamic,
            ));
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else {
            i += text[i..].chars().next().unwrap().len_utf8();
            if i < bytes.len()
                && bytes[i] == b'='
                && matches!(&text[start..i], "+" | "-" | "=" | "!" | "<" | ">")
            {
                i += 1;
            }
        }
        out.push(Token {
            raw: &text[start..i],
            start,
            line: token_line,
            string,
        });
    }
    out
}

fn closing(tokens: &[Token<'_>], at: usize) -> Option<usize> {
    let close = match tokens.get(at)?.raw {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => return None,
    };
    let mut i = at + 1;
    while i < tokens.len() {
        if tokens[i].raw == close {
            return Some(i);
        }
        if matches!(tokens[i].raw, "(" | "[" | "{") {
            i = closing(tokens, i)?;
        }
        i += 1;
    }
    None
}

#[derive(Clone, Default)]
struct Context {
    conditions: Vec<String>,
    target: bool,
    deferred: bool,
}

pub fn scan(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    walk(text, &lex(text), &Context::default(), &mut out);
    out
}

fn walk(text: &str, tokens: &[Token<'_>], context: &Context, out: &mut Vec<Candidate>) {
    let mut i = 0;
    while i < tokens.len() {
        // Each handler reports where to resume. `None` means it could not
        // delimit the construct — an unbalanced bracket abandons the slice,
        // since nothing after it can be placed in a scope reliably.
        let resume = if tokens[i].raw == "if" && tokens.get(i + 1).is_some_and(|t| t.raw == "(") {
            if_chain(text, tokens, i, context, out)
        } else if is_defines_list(tokens, i) {
            defines_list(tokens, i, context, out)
        } else if tokens.get(i + 1).is_some_and(|t| t.raw == "(") {
            call_block(text, tokens, i, context, out)
        } else if matches!(tokens[i].raw, "[" | "(" | "{") {
            scope(text, tokens, i, context, out)
        } else {
            Some(i + 1)
        };
        let Some(resume) = resume else { return };
        i = resume;
    }
}

/// Consume an entire if/else chain so each alternative carries the
/// negations of all preceding alternatives, including nested chains.
fn if_chain(
    text: &str,
    tokens: &[Token<'_>],
    mut i: usize,
    context: &Context,
    out: &mut Vec<Candidate>,
) -> Option<usize> {
    let mut branch = context.clone();
    loop {
        let end = closing(tokens, i + 1)?;
        let condition = text[tokens[i + 1].end()..tokens[end].start]
            .trim()
            .to_string();
        let open = end + 1;
        if !tokens.get(open).is_some_and(|t| t.raw == "{") {
            return None;
        }
        let close = closing(tokens, open)?;
        let mut taken = branch.clone();
        taken.conditions.push(condition.clone());
        walk(text, &tokens[open + 1..close], &taken, out);
        branch.conditions.push(format!("!({condition})"));
        i = close + 1;
        if !tokens.get(i).is_some_and(|t| t.raw == "else") {
            return Some(i);
        }
        i += 1;
        if tokens.get(i).is_some_and(|t| t.raw == "if") {
            continue;
        }
        if tokens.get(i).is_some_and(|t| t.raw == "{") {
            let close = closing(tokens, i)?;
            walk(text, &tokens[i + 1..close], &branch, out);
            i = close + 1;
        }
        return Some(i);
    }
}

/// A direct `defines = [...]` or `defines += [...]`, not a member
/// assignment and not a removal.
fn is_defines_list(tokens: &[Token<'_>], i: usize) -> bool {
    tokens[i].raw == "defines"
        && (i == 0 || tokens[i - 1].raw != ".")
        && tokens
            .get(i + 1)
            .is_some_and(|t| matches!(t.raw, "=" | "+="))
        && tokens.get(i + 2).is_some_and(|t| t.raw == "[")
}

fn defines_list(
    tokens: &[Token<'_>],
    i: usize,
    context: &Context,
    out: &mut Vec<Candidate>,
) -> Option<usize> {
    let end = closing(tokens, i + 2)?;
    // A list here is an operand, not the assigned value: what survives
    // depends on the rest of the expression, which is not evaluated.
    // The whole expression is skipped rather than one arm of it, so a
    // concatenation such as `defines = ["A"] + ["B"]` yields nothing —
    // reporting `"A"` alone would be wrong for `["A"] + ["B"] - ["A"]`.
    if tokens
        .get(end + 1)
        .is_some_and(|t| matches!(t.raw, "+" | "-" | "["))
    {
        return Some(end + 1);
    }
    let mut entry = i + 3;
    let mut j = entry;
    while j <= end {
        if j == end || tokens[j].raw == "," {
            // A one-token entry is a standalone literal; anything longer
            // is computed, and its value is not established here.
            if j == entry + 1 {
                add_candidate(&tokens[entry], context, out);
            }
            entry = j + 1;
        } else if matches!(tokens[j].raw, "(" | "[" | "{") {
            j = closing(tokens, j)?;
        }
        j += 1;
    }
    Some(end + 1)
}

/// Function-call blocks introduce a scope; templates and loops are
/// weaker evidence even if a target appears inside them.
fn call_block(
    text: &str,
    tokens: &[Token<'_>],
    i: usize,
    context: &Context,
    out: &mut Vec<Candidate>,
) -> Option<usize> {
    let end = closing(tokens, i + 1)?;
    if !tokens.get(end + 1).is_some_and(|t| t.raw == "{") {
        return Some(end + 1);
    }
    let close = closing(tokens, end + 1)?;
    let mut nested = context.clone();
    nested.target = TARGET_FUNCTIONS.contains(&tokens[i].raw);
    nested.deferred |= matches!(tokens[i].raw, "template" | "foreach");
    walk(text, &tokens[end + 2..close], &nested, out);
    Some(close + 1)
}

fn scope(
    text: &str,
    tokens: &[Token<'_>],
    i: usize,
    context: &Context,
    out: &mut Vec<Candidate>,
) -> Option<usize> {
    let close = closing(tokens, i)?;
    // Anonymous scopes are not target definitions.
    if tokens[i].raw == "{" {
        let mut nested = context.clone();
        nested.target = false;
        walk(text, &tokens[i + 1..close], &nested, out);
    }
    Some(close + 1)
}

fn add_candidate(token: &Token<'_>, context: &Context, out: &mut Vec<Candidate>) {
    let Some((literal, dynamic)) = &token.string else {
        return;
    };
    let (name, value) = literal
        .split_once('=')
        .map_or((literal.as_str(), None), |(n, v)| (n, Some(v.to_string())));
    if name.is_empty()
        || name.starts_with(|c: char| c.is_ascii_digit())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return;
    }
    // Each term is sufficient on its own: an interpolated value, a template
    // or loop body, or more than one enclosing condition. A single condition
    // outside a template stays medium, which is what `> 1` guards.
    let confidence = if *dynamic || context.deferred || context.conditions.len() > 1 {
        Confidence::Low
    } else if context.target && context.conditions.is_empty() {
        Confidence::High
    } else {
        Confidence::Medium
    };
    out.push(Candidate {
        name: name.to_string(),
        value,
        line: token.line,
        conditions: context.conditions.clone(),
        confidence,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_keep_values_locations_and_conditions() {
        let candidates = scan("source_set(\"x\") {\n defines = [\"PLAIN\", \"VALUE=2\", \"EMPTY=\"]\n if (enabled) {\n if (cpu == \"arm\") { defines += [\"NESTED\"] }\n } else if (fallback) { defines = [\"FALLBACK\"]\n } else { defines = [\"OTHER\"] }\n}\n");
        assert_eq!(candidates.len(), 6);
        assert_eq!(candidates[0].name, "PLAIN");
        assert_eq!(candidates[0].value, None);
        assert_eq!(candidates[0].line, 2);
        assert_eq!(candidates[0].confidence, Confidence::High);
        assert_eq!(candidates[1].value.as_deref(), Some("2"));
        assert_eq!(candidates[2].value.as_deref(), Some(""));
        assert_eq!(candidates[3].conditions, ["enabled", "cpu == \"arm\""]);
        assert_eq!(candidates[3].confidence, Confidence::Low);
        assert_eq!(candidates[4].conditions, ["!(enabled)", "fallback"]);
        assert_eq!(candidates[5].conditions, ["!(enabled)", "!(fallback)"]);
    }

    #[test]
    fn ignores_mentions_removals_and_computed_entries() {
        let candidates = scan(
            r##"
# defines = ["COMMENT"]
note = "defines = [\"STRING\"]"
other_defines = ["OTHER"]
scope.defines = ["MEMBER"]
defines -= ["REMOVED"]
defines = ["SUBTRACTED"] - ["SUBTRACTED"]
defines = ["LEFT"] + ["RIGHT"]
defines = [variable, "PREFIX" + suffix, "${dynamic}", "VALUE=$value",
           "VALID", "QUOTED=\"yes\"", "DOLLAR=\$cash", "9INVALID"]
"##,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["VALUE", "VALID", "QUOTED", "DOLLAR"]
        );
        assert_eq!(candidates[0].confidence, Confidence::Low);
        assert_eq!(candidates[0].value.as_deref(), Some("$value"));
        assert_eq!(candidates[2].value.as_deref(), Some("\"yes\""));
        assert_eq!(candidates[3].value.as_deref(), Some("$cash"));
    }

    #[test]
    fn only_direct_target_literals_are_high_confidence() {
        let candidates = scan(
            r#"
defines = ["GLOBAL"]
config("flags") { defines = ["CONFIG"] }
template("wrapper") { source_set("inner") { defines = ["TEMPLATE"] } }
foreach(x, xs) { defines = ["LOOP"] }
source_set("x") { if (enabled) { defines = ["CONDITIONAL"] } }
"#,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.confidence.as_str())
                .collect::<Vec<_>>(),
            ["medium", "medium", "low", "low", "medium"]
        );
    }
}
