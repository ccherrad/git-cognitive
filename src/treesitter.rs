use tree_sitter::Node;

#[derive(Clone, Copy, Debug)]
pub enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    C,
    Cpp,
}

pub fn detect_language(path: &str) -> Option<SupportedLanguage> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    match ext {
        "rs" => Some(SupportedLanguage::Rust),
        "py" => Some(SupportedLanguage::Python),
        "js" | "mjs" | "cjs" => Some(SupportedLanguage::JavaScript),
        "ts" | "tsx" => Some(SupportedLanguage::TypeScript),
        "go" => Some(SupportedLanguage::Go),
        "java" => Some(SupportedLanguage::Java),
        "c" | "h" => Some(SupportedLanguage::C),
        "cpp" | "cc" | "cxx" | "hpp" => Some(SupportedLanguage::Cpp),
        _ => None,
    }
}

pub fn parse(src: &str, lang: SupportedLanguage) -> Option<tree_sitter::Tree> {
    let ts_lang = match lang {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        SupportedLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        SupportedLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        SupportedLanguage::C => tree_sitter_c::LANGUAGE.into(),
        SupportedLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts_lang).ok()?;
    parser.parse(src, None)
}

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
                    .map(|n: Node| n.utf8_text(src.as_bytes()).unwrap_or(""))
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
                .and_then(|p: Node| {
                    let idx = (0..p.child_count())
                        .find(|&i| p.child(i).map(|c: Node| c.id()) == Some(node.id()))?;
                    if idx == 0 {
                        return None;
                    }
                    p.child(idx - 1)
                })
                .map(|prev: Node| DOC_COMMENT_NODE_TYPES.contains(&prev.kind()))
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

pub fn absolute_complexity(src: &str, lang: &SupportedLanguage) -> u32 {
    parse(src, *lang)
        .as_ref()
        .map(|t| cyclomatic_complexity(src, t) as u32)
        .unwrap_or(0)
}

pub fn doc_gap_score(new_src: &str, lang: &SupportedLanguage) -> f32 {
    parse(new_src, *lang).as_ref().map(doc_gap).unwrap_or(0.0)
}
