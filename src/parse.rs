use serde::Deserialize;

pub const TYPE_USER: &str = "user";
pub const TYPE_ASSISTANT: &str = "assistant";

pub const CONTENT_TYPE_TEXT: &str = "text";
pub const CONTENT_TYPE_TOOL_USE: &str = "tool_use";

/// Tools that create or modify files.
pub const FILE_MODIFICATION_TOOLS: &[&str] = &[
    "Write",
    "Edit",
    "NotebookEdit",
    "mcp__filesystem__write_file",
    "mcp__filesystem__edit_file",
];

/// A single line in a Claude Code JSONL transcript.
///
/// Claude Code uses `type` to distinguish user/assistant messages; some formats
/// (e.g. Cursor) use `role`. `normalize` copies `role` into `type` so downstream
/// consumers can switch on `type` uniformly.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptLine {
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub role: String,
    /// Message UUID (used by transcript-linking consumers; parsed for parity).
    #[serde(default)]
    #[allow(dead_code)]
    pub uuid: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub message: serde_json::Value,
}

impl TranscriptLine {
    fn normalize(&mut self) {
        if self.type_.is_empty() && !self.role.is_empty() {
            self.type_ = self.role.clone();
        }
    }

    /// Assistant content blocks, or empty if this is not an assistant message.
    pub fn content_blocks(&self) -> Vec<ContentBlock> {
        match self.message.get("content") {
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|b| serde_json::from_value(b.clone()).ok())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Text of a user message, with IDE/command metadata tags stripped. Handles
    /// both string and array content forms.
    pub fn user_text(&self) -> String {
        let raw = match self.message.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => {
                let parts: Vec<&str> = arr
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(CONTENT_TYPE_TEXT))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                parts.join(" ")
            }
            _ => String::new(),
        };
        strip_context_tags(&raw)
    }

    /// Truncated timestamp (`YYYY-MM-DDTHH:MM:SS`) for display.
    pub fn short_ts(&self) -> &str {
        self.timestamp.get(..19).unwrap_or("")
    }
}

/// A content block within an assistant message.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlock {
    #[serde(default, rename = "type")]
    pub type_: String,
    /// Tool-use block id (parsed for parity with tool_result linkage).
    #[serde(default)]
    #[allow(dead_code)]
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

impl ContentBlock {
    pub fn is_tool_use(&self) -> bool {
        self.type_ == CONTENT_TYPE_TOOL_USE
    }

    pub fn is_text(&self) -> bool {
        self.type_ == CONTENT_TYPE_TEXT
    }

    /// `input.file_path`, falling back to `input.notebook_path`.
    pub fn tool_file(&self) -> Option<&str> {
        self.input
            .get("file_path")
            .and_then(|v| v.as_str())
            .or_else(|| self.input.get("notebook_path").and_then(|v| v.as_str()))
    }

    /// Content written by a Write/Edit tool, if this block is one.
    pub fn written_content(&self) -> Option<&str> {
        match self.name.as_str() {
            "Write" => self.input.get("content").and_then(|v| v.as_str()),
            "Edit" => self.input.get("new_string").and_then(|v| v.as_str()),
            _ => None,
        }
    }

    fn is_file_modification(&self) -> bool {
        FILE_MODIFICATION_TOOLS.contains(&self.name.as_str())
    }
}

/// Paired IDE/command-metadata tags whose whole span (open tag → close tag,
/// inclusive) is removed from user-facing prompt text. Mirrors entire's
/// StripIDEContextTags tag set.
const STRIPPED_TAGS: &[&str] = &[
    "ide_opened_file",
    "ide_selection",
    "local-command-caveat",
    "local-command-stdout",
    "system-reminder",
    "command-name",
    "command-message",
    "command-args",
];

/// Remove IDE/command-injected context tags (and their content) from a prompt,
/// and unwrap `<user_query>` wrappers (keeping their inner text). Dependency-free.
fn strip_context_tags(text: &str) -> String {
    let mut s = text.to_string();
    for tag in STRIPPED_TAGS {
        s = remove_tag_spans(&s, tag);
    }
    // Cursor wraps user text in <user_query>…</user_query>: drop tags, keep body.
    s = s.replace("<user_query>", "").replace("</user_query>", "");
    s.trim().to_string()
}

/// Remove every `<tag ...>...</tag>` span (case-sensitive, non-greedy) from `s`.
fn remove_tag_spans(s: &str, tag: &str) -> String {
    let open_prefix = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find(&open_prefix) {
        // Ensure the match is a real tag start: next char is '>' or whitespace.
        let after = &rest[start + open_prefix.len()..];
        let is_tag = after
            .chars()
            .next()
            .map(|c| c == '>' || c.is_whitespace())
            .unwrap_or(false);
        if !is_tag {
            // Not our tag; keep up to and including this occurrence, continue.
            let keep_to = start + open_prefix.len();
            out.push_str(&rest[..keep_to]);
            rest = &rest[keep_to..];
            continue;
        }

        out.push_str(&rest[..start]);
        match rest[start..].find(&close) {
            Some(rel_end) => {
                rest = &rest[start + rel_end + close.len()..];
            }
            None => {
                // Unclosed tag: drop the remainder.
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse a single JSONL transcript line. Returns None if malformed or blank.
pub fn parse_line(line: &str) -> Option<TranscriptLine> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parsed = serde_json::from_str::<TranscriptLine>(line).ok()?;
    parsed.normalize();
    Some(parsed)
}

/// Parse newline-delimited JSONL transcript lines. Malformed lines are skipped.
pub fn parse_lines<I, S>(lines: I) -> Vec<TranscriptLine>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for line in lines {
        let line = line.as_ref().trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(mut parsed) = serde_json::from_str::<TranscriptLine>(line) {
            parsed.normalize();
            out.push(parsed);
        }
    }
    out
}

/// Files created or modified by file-modification tool calls, in first-seen order.
pub fn extract_modified_files(lines: &[TranscriptLine]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::new();
    for line in lines {
        if line.type_ != TYPE_ASSISTANT {
            continue;
        }
        for block in line.content_blocks() {
            if !block.is_file_modification() {
                continue;
            }
            if let Some(file) = block.tool_file() {
                if seen.insert(file.to_string()) {
                    files.push(file.to_string());
                }
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_skips_malformed_and_blank() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("{not json").is_none());
    }

    #[test]
    fn normalize_copies_role_into_type() {
        let line = parse_line(r#"{"role":"user","message":{"content":"hi"}}"#).unwrap();
        assert_eq!(line.type_, TYPE_USER);
    }

    #[test]
    fn user_text_string_and_array() {
        let s = parse_line(r#"{"type":"user","message":{"content":"hello"}}"#).unwrap();
        assert_eq!(s.user_text(), "hello");

        let a = parse_line(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
        )
        .unwrap();
        assert_eq!(a.user_text(), "a b");
    }

    #[test]
    fn user_text_strips_ide_and_command_tags() {
        // Command metadata tags are removed entirely, real prompt survives.
        let line = parse_line(
            r#"{"type":"user","message":{"content":"<command-name>/model</command-name><command-args></command-args>\nhello there"}}"#,
        )
        .unwrap();
        assert_eq!(line.user_text(), "hello there");

        // local-command-* spans dropped.
        let l2 = parse_line(
            r#"{"type":"user","message":{"content":"<local-command-stdout>Set model to Haiku</local-command-stdout>"}}"#,
        )
        .unwrap();
        assert_eq!(l2.user_text(), "");

        // user_query wrapper unwrapped, content kept.
        let l3 = parse_line(
            r#"{"type":"user","message":{"content":"<user_query>fix the bug</user_query>"}}"#,
        )
        .unwrap();
        assert_eq!(l3.user_text(), "fix the bug");

        // A plain prompt is untouched.
        let l4 = parse_line(r#"{"type":"user","message":{"content":"what is this code about ?"}}"#).unwrap();
        assert_eq!(l4.user_text(), "what is this code about ?");
    }

    #[test]
    fn assistant_tool_use_blocks() {
        let line = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Write","input":{"file_path":"a.rs","content":"x"}}]}}"#,
        )
        .unwrap();
        let blocks = line.content_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].is_text());
        assert!(blocks[1].is_tool_use());
        assert_eq!(blocks[1].tool_file(), Some("a.rs"));
        assert_eq!(blocks[1].written_content(), Some("x"));
    }

    #[test]
    fn edit_written_content_uses_new_string() {
        let line = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"a.rs","new_string":"y"}}]}}"#,
        )
        .unwrap();
        assert_eq!(line.content_blocks()[0].written_content(), Some("y"));
    }

    #[test]
    fn tool_file_falls_back_to_notebook_path() {
        let line = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"NotebookEdit","input":{"notebook_path":"nb.ipynb"}}]}}"#,
        )
        .unwrap();
        assert_eq!(line.content_blocks()[0].tool_file(), Some("nb.ipynb"));
    }

    #[test]
    fn extract_modified_files_dedups_in_order() {
        let lines = parse_lines([
            r#"{"type":"user","message":{"content":"go"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"a.rs","content":"1"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"b.rs","new_string":"2"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"a.rs","new_string":"3"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"c.rs"}}]}}"#,
        ]);
        assert_eq!(extract_modified_files(&lines), vec!["a.rs", "b.rs"]);
    }
}
