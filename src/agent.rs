use std::path::{Path, PathBuf};

/// A coding agent whose sessions git-cognitive can discover on disk.
///
/// git-cognitive has no session ID (it installs no agent hooks), so it locates
/// sessions by encoding the repo path into the agent's per-project store and
/// scanning for transcript files. Only agents whose native store is keyed by
/// project directory are supported this way (Claude, Cursor, Factory Droid, Pi).
pub trait Agent {
    /// Stable identifier, e.g. "claude", used by `enable <agent>`.
    fn name(&self) -> &'static str;

    /// Directory holding this repo's session transcripts, or None if it does
    /// not exist for this repo.
    fn project_dir(&self, repo_path: &Path) -> Option<PathBuf>;

    /// File extension of transcript files in the project dir (without dot).
    fn transcript_ext(&self) -> &'static str {
        "jsonl"
    }
}

/// Resolve an agent by name.
pub fn by_name(name: &str) -> Option<Box<dyn Agent>> {
    match name {
        "claude" => Some(Box::new(Claude)),
        "cursor" => Some(Box::new(Cursor)),
        "factory" | "droid" | "factory-droid" => Some(Box::new(FactoryDroid)),
        "pi" => Some(Box::new(Pi)),
        _ => None,
    }
}

/// Every supported agent name, for help text and default scanning.
pub const SUPPORTED: &[&str] = &["claude", "cursor", "factory", "pi"];

/// Repo-local file listing enabled agent names, one per line.
fn config_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".git").join("cognitive-agents")
}

/// Record `name` as an enabled agent for this repo (idempotent). Starts from the
/// explicitly-recorded set (not the claude default) so `enable <agent>` on a
/// fresh repo yields exactly that agent.
pub fn enable(repo_path: &Path, name: &str) -> std::io::Result<()> {
    let mut names = recorded_names(repo_path);
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
    std::fs::write(config_path(repo_path), names.join("\n") + "\n")
}

/// Names explicitly recorded in the config file, empty if none.
fn recorded_names(repo_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path(repo_path))
        .map(|content| {
            content
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Names of agents enabled for this repo. Defaults to `["claude"]` when no
/// config exists, preserving prior single-agent behavior.
pub fn enabled_names(repo_path: &Path) -> Vec<String> {
    let names = recorded_names(repo_path);
    if names.is_empty() {
        vec!["claude".to_string()]
    } else {
        names
    }
}

/// Resolve the enabled agents for this repo into trait objects.
pub fn enabled(repo_path: &Path) -> Vec<Box<dyn Agent>> {
    enabled_names(repo_path)
        .iter()
        .filter_map(|n| by_name(n))
        .collect()
}

fn home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Canonical repo path as a lossy string.
fn cwd_string(repo_path: &Path) -> Option<String> {
    Some(repo_path.canonicalize().ok()?.to_string_lossy().to_string())
}

/// Replace every non-alphanumeric character with `-` (Claude/Cursor/Droid scheme).
fn sanitize_non_alnum(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Find a subdirectory of `base` whose name matches `key`, tolerating a
/// difference in leading dashes (a leading-slash cwd sanitizes to a leading dash).
fn match_project_subdir(base: &Path, key: &str) -> Option<PathBuf> {
    std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == key || name.trim_start_matches('-') == key.trim_start_matches('-')
        })
        .map(|e| e.path())
}

/// Claude Code: `~/.claude/projects/<sanitized-cwd>/<id>.jsonl`.
pub struct Claude;
impl Agent for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }
    fn project_dir(&self, repo_path: &Path) -> Option<PathBuf> {
        let base = home()?.join(".claude").join("projects");
        let key = sanitize_non_alnum(&cwd_string(repo_path)?);
        match_project_subdir(&base, &key)
    }
}

/// Cursor: `~/.cursor/projects/<sanitized-cwd>/agent-transcripts/<id>.jsonl`.
pub struct Cursor;
impl Agent for Cursor {
    fn name(&self) -> &'static str {
        "cursor"
    }
    fn project_dir(&self, repo_path: &Path) -> Option<PathBuf> {
        let base = home()?.join(".cursor").join("projects");
        let cwd = cwd_string(repo_path)?;
        let key = sanitize_non_alnum(cwd.trim_start_matches('/'));
        let proj = match_project_subdir(&base, &key)?;
        let transcripts = proj.join("agent-transcripts");
        transcripts.is_dir().then_some(transcripts)
    }
}

/// Factory AI Droid: `~/.factory/sessions/<sanitized-cwd>/<id>.jsonl`.
pub struct FactoryDroid;
impl Agent for FactoryDroid {
    fn name(&self) -> &'static str {
        "factory"
    }
    fn project_dir(&self, repo_path: &Path) -> Option<PathBuf> {
        let base = home()?.join(".factory").join("sessions");
        let key = sanitize_non_alnum(&cwd_string(repo_path)?);
        match_project_subdir(&base, &key)
    }
}

/// Pi: `~/.pi/agent/sessions/<encoded-cwd>/<id>.jsonl`, where the encoded key is
/// `--<slashes-as-dashes, trimmed>--`.
pub struct Pi;
impl Agent for Pi {
    fn name(&self) -> &'static str {
        "pi"
    }
    fn project_dir(&self, repo_path: &Path) -> Option<PathBuf> {
        let base = home()?.join(".pi").join("agent").join("sessions");
        let cwd = cwd_string(repo_path)?;
        let key = encode_for_pi(&cwd);
        match_project_subdir(&base, &key)
    }
}

/// Pi's repo-path encoding: slashes → dashes, trim leading/trailing dashes,
/// then wrap in `--...--`.
fn encode_for_pi(repo_path: &str) -> String {
    if repo_path.is_empty() {
        return String::new();
    }
    let body = repo_path.replace(['/', '\\'], "-");
    let body = body.trim_matches('-');
    format!("--{}--", body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_claude_scheme() {
        assert_eq!(sanitize_non_alnum("/Users/x/repo.git"), "-Users-x-repo-git");
    }

    #[test]
    fn pi_encoding() {
        assert_eq!(encode_for_pi("/Users/foo/repo"), "--Users-foo-repo--");
        assert_eq!(encode_for_pi(""), "");
    }

    #[test]
    fn by_name_aliases() {
        assert_eq!(by_name("droid").unwrap().name(), "factory");
        assert_eq!(by_name("factory-droid").unwrap().name(), "factory");
        assert!(by_name("codex").is_none());
    }

    // Serial: mutates process-global HOME. Run with other HOME users disabled.
    #[test]
    fn project_dir_discovers_each_agent_layout() {
        let tmp = std::env::temp_dir().join(format!("gc-home-{}", std::process::id()));
        let home = tmp.join("home");
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let cwd = repo.to_string_lossy().to_string();

        let claude_key = sanitize_non_alnum(&cwd);
        let cursor_key = sanitize_non_alnum(cwd.trim_start_matches('/'));
        let pi_key = encode_for_pi(&cwd);

        let layouts = [
            home.join(".claude").join("projects").join(&claude_key),
            home.join(".cursor")
                .join("projects")
                .join(&cursor_key)
                .join("agent-transcripts"),
            home.join(".factory").join("sessions").join(&claude_key),
            home.join(".pi").join("agent").join("sessions").join(&pi_key),
        ];
        for l in &layouts {
            std::fs::create_dir_all(l).unwrap();
        }

        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        assert!(Claude.project_dir(&repo).is_some(), "claude not found");
        assert!(Cursor.project_dir(&repo).is_some(), "cursor not found");
        assert!(FactoryDroid.project_dir(&repo).is_some(), "factory not found");
        assert!(Pi.project_dir(&repo).is_some(), "pi not found");

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn enabled_defaults_to_claude_and_records() {
        let repo = std::env::temp_dir().join(format!("gc-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(enabled_names(&repo), vec!["claude".to_string()]);

        enable(&repo, "cursor").unwrap();
        enable(&repo, "pi").unwrap();
        enable(&repo, "cursor").unwrap(); // idempotent

        assert_eq!(
            enabled_names(&repo),
            vec!["cursor".to_string(), "pi".to_string()]
        );
        assert_eq!(enabled(&repo).len(), 2);

        std::fs::remove_dir_all(&repo).ok();
    }
}
