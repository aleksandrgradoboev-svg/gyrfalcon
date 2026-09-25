//! Small, normalized BSL syntax-tree fragments for project-practice mining.
//!
//! This module consumes source text already selected by the caller. It never
//! walks a configuration checkout. Each occurrence carries a method and line
//! so an aggregate miner can count *distinct methods/modules*, not repeated
//! statements in one large generated method.

use tree_sitter::{Node, Parser};

/// A hard bound on one method's contribution to the candidate pool.
pub const MAX_FRAGMENTS_PER_METHOD: usize = 48;
/// A bound on analysis work per method; exceeding it is reported, not hidden.
pub const MAX_CANDIDATES_PER_METHOD: usize = 512;

#[derive(Debug, Default)]
pub struct Extraction {
    pub fragments: Vec<Fragment>,
    pub methods_seen: usize,
    pub methods_with_errors: usize,
    pub methods_truncated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub signature: String,
    pub method_name: String,
    /// One-based source line of the fragment root.
    pub line: u32,
}

/// Parse a BSL module and collect statement-shaped AST fragments.
///
/// Identifiers, literals and comments are normalized; method-call names and
/// operators remain. This is candidate extraction, not acceptance of a
/// practice: frequency and cross-module support must be checked by the caller.
pub fn fragments(source: &str) -> Result<Vec<Fragment>, String> {
    Ok(fragments_with_stats(source)?.fragments)
}

/// Variant retaining coverage counters for truthful whole-index reporting.
pub fn fragments_with_stats(source: &str) -> Result<Extraction, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&gyrfalcon_parser::bsl::language())
        .map_err(|error| error.to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "BSL parser returned no tree".to_string())?;
    let bytes = source.as_bytes();
    let mut output = Extraction::default();
    let mut cursor = tree.walk();
    let mut methods = vec![tree.root_node()];

    while let Some(node) = methods.pop() {
        if matches!(node.kind(), "procedure_definition" | "function_definition") {
            output.methods_seen += 1;
            if node.has_error() {
                output.methods_with_errors += 1;
                continue;
            }
            let Some(method_name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(bytes).ok())
            else {
                continue;
            };
            let mut local = Vec::new();
            let mut walk = vec![node];
            let mut candidates = 0;
            let mut truncated = false;
            while let Some(part) = walk.pop() {
                if part.has_error() {
                    continue;
                }
                if is_candidate(part.kind()) {
                    candidates += 1;
                    if candidates > MAX_CANDIDATES_PER_METHOD {
                        truncated = true;
                        break;
                    }
                    let mut nodes = 0;
                    let signature = normalize(part, bytes, 0, &mut nodes);
                    // Bare calls/assignments provide little structural signal.
                    // A richer enclosing statement remains eligible.
                    if nodes >= 3 && signature.len() <= 320 {
                        let fragment = Fragment {
                            signature,
                            method_name: method_name.to_string(),
                            line: part.start_position().row as u32 + 1,
                        };
                        if !local
                            .iter()
                            .any(|item: &Fragment| item.signature == fragment.signature)
                        {
                            local.push(fragment);
                        }
                    }
                }
                let mut child_cursor = part.walk();
                walk.extend(part.named_children(&mut child_cursor));
            }
            if local.len() > MAX_FRAGMENTS_PER_METHOD {
                truncated = true;
                // Prefer branching/control-flow patterns and call-bearing
                // fragments over a method's repetitive assignments.
                local.sort_by(|a, b| {
                    priority(&b.signature)
                        .cmp(&priority(&a.signature))
                        .then_with(|| a.line.cmp(&b.line))
                });
                local.truncate(MAX_FRAGMENTS_PER_METHOD);
            }
            if truncated {
                output.methods_truncated += 1;
            }
            local.sort_by_key(|fragment| fragment.line);
            output.fragments.extend(local);
            continue;
        }
        methods.extend(node.named_children(&mut cursor));
    }

    output.fragments.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    Ok(output)
}

fn priority(signature: &str) -> u8 {
    let mut score = if signature.starts_with("if_statement")
        || signature.starts_with("try_statement")
        || signature.starts_with("for_statement")
        || signature.starts_with("for_each_statement")
        || signature.starts_with("while_statement")
    {
        2
    } else {
        0
    };
    if signature.contains("call:") {
        score += 1;
    }
    score
}

fn is_candidate(kind: &str) -> bool {
    matches!(
        kind,
        "assignment_statement"
            | "call_statement"
            | "if_statement"
            | "for_statement"
            | "for_each_statement"
            | "while_statement"
            | "try_statement"
            | "return_statement"
    )
}

/// Serialize a bounded tree shape. The explicit `more` marker prevents a
/// large subtree from colliding with an otherwise identical small subtree.
fn normalize(node: Node<'_>, source: &[u8], depth: usize, nodes: &mut usize) -> String {
    *nodes += 1;
    let kind = node.kind();
    if is_literal(kind) {
        return "literal".to_string();
    }
    if kind == "identifier" {
        return "id".to_string();
    }
    if depth >= 4 {
        return format!("{kind}(more)");
    }

    let mut parts = Vec::new();
    let call_name = if kind == "method_call" {
        node.child_by_field_name("name").map(|child| child.id())
    } else {
        None
    };
    let mut cursor = node.walk();
    for (index, child) in node.children(&mut cursor).enumerate() {
        if index >= 10 {
            parts.push("more".to_string());
            break;
        }
        if child.kind() == "comment" {
            continue;
        }
        if !child.is_named() {
            if matches!(
                child.kind(),
                "=" | "<>" | "+" | "-" | "*" | "/" | ">" | "<" | ">=" | "<="
            ) {
                parts.push(child.kind().to_string());
            }
            continue;
        }
        if Some(child.id()) == call_name {
            let name = child.utf8_text(source).unwrap_or("?").to_lowercase();
            parts.push(format!("call:{name}"));
        } else {
            parts.push(normalize(child, source, depth + 1, nodes));
        }
    }
    if parts.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}({})", parts.join(","))
    }
}

fn is_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string"
            | "string_content"
            | "number"
            | "integer"
            | "float"
            | "date"
            | "date_literal"
            | "boolean"
            | "true"
            | "false"
            | "null"
            | "undefined"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_and_literals_do_not_split_same_pattern() {
        let source = "Процедура Один()\n А = Проверить(\"раз\");\nКонецПроцедуры\n\nПроцедура Два()\n Б = Проверить(\"два\");\nКонецПроцедуры";
        let found = fragments(source).unwrap();
        let assignments: Vec<_> = found
            .iter()
            .filter(|item| item.signature.starts_with("assignment_statement"))
            .collect();
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].signature, assignments[1].signature);
        assert_eq!(assignments[0].method_name, "Один");
        assert_eq!(assignments[1].method_name, "Два");
        assert_eq!(assignments[0].line, 2);
        assert_eq!(assignments[1].line, 6);
    }

    #[test]
    fn method_call_name_changes_signature() {
        let source = "Процедура Тест()\n А = Проверить(1);\n Б = Сохранить(1);\nКонецПроцедуры";
        let found = fragments(source).unwrap();
        let assignments: Vec<_> = found
            .iter()
            .filter(|item| item.signature.starts_with("assignment_statement"))
            .collect();
        assert_eq!(assignments.len(), 2);
        assert_ne!(assignments[0].signature, assignments[1].signature);
    }

    #[test]
    fn syntax_errors_are_not_mined() {
        let source = "Процедура Тест()\nЕсли Тогда\nКонецПроцедуры";
        let extraction = fragments_with_stats(source).unwrap();
        assert!(extraction.fragments.is_empty());
        assert_eq!(extraction.methods_seen, 1);
        assert_eq!(extraction.methods_with_errors, 1);
    }

    #[test]
    fn output_is_bounded_and_truncation_is_reported() {
        let mut source = String::from("Процедура Тест()\n");
        for index in 0..80 {
            source.push_str(&format!(" Вызов{index}();\n"));
        }
        source.push_str("КонецПроцедуры");
        let extraction = fragments_with_stats(&source).unwrap();
        assert!(extraction.fragments.len() <= MAX_FRAGMENTS_PER_METHOD);
        assert_eq!(extraction.methods_truncated, 1);
    }
}
