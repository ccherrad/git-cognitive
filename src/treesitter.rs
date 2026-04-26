use tree_sitter::{Language, Parser};

#[derive(Debug, Clone, Copy)]
pub enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Java,
    C,
    Cpp,
    Go,
}

impl SupportedLanguage {
    fn tree_sitter_language(&self) -> Language {
        match self {
            SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
            SupportedLanguage::Python => tree_sitter_python::LANGUAGE.into(),
            SupportedLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            SupportedLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            SupportedLanguage::Java => tree_sitter_java::LANGUAGE.into(),
            SupportedLanguage::C => tree_sitter_c::LANGUAGE.into(),
            SupportedLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

pub fn detect_language(file_path: &str) -> Option<SupportedLanguage> {
    let ext = file_path.split('.').next_back()?.to_lowercase();
    match ext.as_str() {
        "rs" => Some(SupportedLanguage::Rust),
        "py" | "pyw" | "pyi" => Some(SupportedLanguage::Python),
        "js" | "mjs" | "cjs" => Some(SupportedLanguage::JavaScript),
        "ts" | "tsx" => Some(SupportedLanguage::TypeScript),
        "java" => Some(SupportedLanguage::Java),
        "c" | "h" => Some(SupportedLanguage::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(SupportedLanguage::Cpp),
        "go" => Some(SupportedLanguage::Go),
        _ => None,
    }
}

fn parse(src: &str, lang: &SupportedLanguage) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.tree_sitter_language()).ok()?;
    parser.parse(src, None)
}

/// Cyclomatic complexity: count decision points in the AST.
/// Each if, else if, for, while, loop, match arm, && / || adds 1.
fn cyclomatic_complexity(src: &str, tree: &tree_sitter::Tree) -> usize {
    let mut count = 0usize;
    let mut cursor = tree.walk();

    loop {
        let node = cursor.node();
        match node.kind() {
            "if_expression"
            | "if_statement"
            | "else_clause"
            | "else_if_clause"
            | "for_expression"
            | "for_statement"
            | "for_in_statement"
            | "while_expression"
            | "while_statement"
            | "loop_expression"
            | "match_arm"
            | "catch_clause"
            | "case_clause"
            | "conditional_expression" => {
                count += 1;
            }
            "binary_expression" | "logical_expression" => {
                let op_node = node.child_by_field_name("operator");
                let op_text = op_node
                    .map(|n| n.utf8_text(src.as_bytes()).unwrap_or(""))
                    .unwrap_or("");
                if matches!(op_text, "&&" | "||" | "and" | "or") {
                    count += 1;
                }
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return count;
            }
        }
    }
}

const FUNCTION_NODE_TYPES: &[&str] = &[
    "function_item",
    "function_declaration",
    "function_definition",
    "method_declaration",
    "method_definition",
    "impl_item",
];

const DOC_COMMENT_NODE_TYPES: &[&str] =
    &["line_comment", "block_comment", "doc_comment", "comment"];

/// Doc gap: ratio of functions added without a preceding doc comment.
/// Returns 0.0 (fully documented) to 1.0 (nothing documented).
fn doc_gap(tree: &tree_sitter::Tree) -> f32 {
    let mut total_fns = 0usize;
    let mut undocumented = 0usize;
    let mut cursor = tree.walk();

    loop {
        let node = cursor.node();

        if FUNCTION_NODE_TYPES.contains(&node.kind()) {
            total_fns += 1;
            let parent = node.parent();
            let has_doc = parent
                .and_then(|p| {
                    let idx = (0..p.child_count())
                        .find(|&i| p.child(i).map(|c| c.id()) == Some(node.id()))?;
                    if idx == 0 {
                        return None;
                    }
                    p.child(idx - 1)
                })
                .map(|prev| DOC_COMMENT_NODE_TYPES.contains(&prev.kind()))
                .unwrap_or(false);

            if !has_doc {
                undocumented += 1;
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                if total_fns == 0 {
                    return 0.0;
                }
                return undocumented as f32 / total_fns as f32;
            }
        }
    }
}

/// Compute complexity delta between old and new source.
/// Returns a 0.0–1.0 score: ratio of complexity added vs total complexity.
pub fn complexity_delta(old_src: &str, new_src: &str, lang: &SupportedLanguage) -> f32 {
    let old_tree = parse(old_src, lang);
    let new_tree = parse(new_src, lang);

    let old_complexity = old_tree
        .as_ref()
        .map(|t| cyclomatic_complexity(old_src, t))
        .unwrap_or(0);
    let new_complexity = new_tree
        .as_ref()
        .map(|t| cyclomatic_complexity(new_src, t))
        .unwrap_or(0);

    let added = new_complexity.saturating_sub(old_complexity);
    let total = new_complexity.max(1);
    (added as f32 / total as f32).clamp(0.0, 1.0)
}

/// Compute doc gap score for new source only.
/// Returns 0.0 (all documented) to 1.0 (nothing documented).
pub fn doc_gap_score(new_src: &str, lang: &SupportedLanguage) -> f32 {
    parse(new_src, lang).as_ref().map(doc_gap).unwrap_or(0.0)
}
