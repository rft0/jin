use std::fs;
use std::io::{self, ErrorKind, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use crate::config;
use crate::ui;

/// A text file as the model sees it: LF line endings, no BOM. Both are
/// restored on write.
#[derive(Clone)]
pub struct FileText {
    pub path: String,
    pub text: String,
    pub exists: bool,
    original: String,
    bom: bool,
    crlf: bool,
}

impl FileText {
    /// Read a UTF-8 file of at most `max_bytes`. A missing file loads as an
    /// empty new file.
    pub fn load(path: &str, max_bytes: usize) -> Result<FileText, String> {
        let original = match fs::read(path) {
            Ok(bytes) if bytes.len() > max_bytes => {
                return Err(format!("{path} is over {} KB", max_bytes / 1024));
            }
            Ok(bytes) => String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"))?,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(FileText {
                    path: path.to_string(),
                    text: String::new(),
                    exists: false,
                    original: String::new(),
                    bom: false,
                    crlf: false,
                });
            }
            Err(e) => return Err(format!("reading {path}: {e}")),
        };
        Ok(FileText {
            path: path.to_string(),
            text: original.trim_start_matches('\u{feff}').replace("\r\n", "\n"),
            exists: true,
            bom: original.starts_with('\u{feff}'),
            crlf: original.contains("\r\n"),
            original,
        })
    }

    /// The `<file>` block sent to the model.
    pub fn block(&self) -> String {
        format!("<file path=\"{}\">\n{}\n</file>", self.path, self.text)
    }

    /// Whether `other` names this file (ignoring `./` and slash direction).
    pub fn is(&self, other: &str) -> bool {
        fn norm(p: &str) -> String {
            let p = p.trim().replace('\\', "/");
            let p = p.strip_prefix("./").unwrap_or(&p);
            if cfg!(windows) { p.to_lowercase() } else { p.to_string() }
        }
        norm(&self.path) == norm(other)
    }
}

/// Apply exact-match replacements in order; each `old` must match once.
pub fn apply(text: &str, edits: &Value) -> Result<String, String> {
    let edits = edits.as_array().ok_or("the reply had no edits list")?;
    let mut out = text.to_string();
    for (i, e) in edits.iter().enumerate() {
        let n = i + 1;
        let old = e["old"].as_str().unwrap_or("").replace("\r\n", "\n");
        let new = e["new"].as_str().unwrap_or("").replace("\r\n", "\n");
        if old.is_empty() {
            if out.trim().is_empty() {
                out = new;
                continue;
            }
            return Err(format!("edit {n} has an empty `old` but the file is not empty"));
        }
        match out.matches(old.as_str()).count() {
            1 => out = out.replacen(old.as_str(), &new, 1),
            0 => return Err(format!("edit {n}: `old` text was not found in the file")),
            k => return Err(format!("edit {n}: `old` text matches {k} places and must be unique")),
        }
    }
    Ok(out)
}

/// Show the diff, ask, and write (keeping line endings and BOM). With
/// `dry_run` only the diff is shown.
pub fn review_and_write(file: &FileText, updated: &str, summary: &str, dry_run: bool) -> Result<ExitCode, String> {
    if updated == file.text {
        eprintln!("{}", if summary.is_empty() { "no changes".into() } else { format!("no changes: {summary}") });
        return Ok(ExitCode::SUCCESS);
    }

    print_diff(&file.path, &file.text, updated, file.exists);
    if !summary.is_empty() {
        eprintln!("\n  {}\n", ui::dim(summary));
    }
    if dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    if !io::stdin().is_terminal() {
        return Err("stdin is not a terminal; not writing without confirmation".into());
    }
    let verb = if file.exists { "Apply to" } else { "Create" };
    if !ui::yes(&ui::ask(&format!("{verb} {}? [y/N]: ", file.path))) {
        return Ok(ExitCode::SUCCESS);
    }

    let mut out = if file.crlf { updated.replace('\n', "\r\n") } else { updated.to_string() };
    if file.bom {
        out.insert(0, '\u{feff}');
    }
    if let Some(dir) = Path::new(&file.path).parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    save_undo(Path::new(&file.path), file.exists.then_some(file.original.as_str()));
    fs::write(&file.path, out).map_err(|e| format!("writing {}: {e}", file.path))?;
    eprintln!("{} {}  {}", ui::green("wrote"), file.path, ui::dim("(jin --undo to revert)"));
    Ok(ExitCode::SUCCESS)
}

fn print_diff(name: &str, old: &str, new: &str, exists: bool) {
    let diff = similar::TextDiff::from_lines(old, new);
    let from = if exists { format!("a/{name}") } else { "/dev/null".into() };
    let text = diff.unified_diff().context_radius(3).header(&from, &format!("b/{name}")).to_string();
    eprintln!();
    for line in text.lines() {
        let painted = if line.starts_with("+++") || line.starts_with("---") {
            ui::bold(line)
        } else if line.starts_with('+') {
            ui::green(line)
        } else if line.starts_with('-') {
            ui::red(line)
        } else if line.starts_with("@@") {
            ui::cyan(line)
        } else {
            line.to_string()
        };
        eprintln!("{painted}");
    }
}

// Undo file: line 1 is the absolute path, line 2 is "existed" or "new",
// the rest is the original content.
fn undo_path() -> Option<PathBuf> {
    config::jin_dir().map(|d| d.join("undo"))
}

fn save_undo(path: &Path, original: Option<&str>) {
    let (Some(undo), Ok(abs)) = (undo_path(), std::path::absolute(path)) else { return };
    let state = if original.is_some() { "existed" } else { "new" };
    let body = format!("{}\n{state}\n{}", abs.display(), original.unwrap_or(""));
    if let Some(dir) = undo.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(e) = fs::write(&undo, body) {
        ui::error(&format!("could not save undo state: {e}"));
    }
}

pub fn undo() -> Result<ExitCode, String> {
    let undo = undo_path().ok_or("no home directory")?;
    let body = match fs::read_to_string(&undo) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err("nothing to undo".into()),
        Err(e) => return Err(format!("reading {}: {e}", undo.display())),
    };
    let mut parts = body.splitn(3, '\n');
    let (Some(target), Some(state)) = (parts.next(), parts.next()) else {
        return Err("undo state is corrupt".into());
    };
    let content = parts.next().unwrap_or("");

    let prompt = if state == "new" {
        format!("Delete {target} (jin created it)? [y/N]: ")
    } else {
        format!("Restore {target} to before the last jin edit? [y/N]: ")
    };
    if !ui::yes(&ui::ask(&prompt)) {
        return Ok(ExitCode::SUCCESS);
    }
    if state == "new" {
        fs::remove_file(target).map_err(|e| format!("removing {target}: {e}"))?;
    } else {
        fs::write(target, content).map_err(|e| format!("writing {target}: {e}"))?;
    }
    let _ = fs::remove_file(&undo);
    eprintln!("{} {target}", ui::green("restored"));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_unique_edits_in_order() {
        let edits = json!([{ "old": "b", "new": "B" }, { "old": "B\nc", "new": "B\nC" }]);
        assert_eq!(apply("a\nb\nc\n", &edits).unwrap(), "a\nB\nC\n");
    }

    #[test]
    fn rejects_missing_and_ambiguous_old() {
        assert!(apply("x\nx\n", &json!([{ "old": "x", "new": "y" }])).unwrap_err().contains("2 places"));
        assert!(apply("x\n", &json!([{ "old": "z", "new": "y" }])).unwrap_err().contains("not found"));
    }

    #[test]
    fn creates_new_file_from_empty_old() {
        assert_eq!(apply("", &json!([{ "old": "", "new": "hi\n" }])).unwrap(), "hi\n");
    }

    #[test]
    fn matches_paths_loosely() {
        let f = FileText::load("definitely-missing.txt", 10).unwrap();
        assert!(f.is("./definitely-missing.txt"));
        assert!(!f.is("other.txt"));
    }
}
