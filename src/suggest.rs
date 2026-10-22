use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::config::{Config, Llm};
use crate::edit::{self, FileText};
use crate::llm::{self, Prompt};
use crate::shell::{self, Interpreter, Shell};
use crate::ui;

const SYSTEM: &str = r#"You are jin, a terminal assistant. Each request gets one reply: a shell command, a temporary script, a short answer, or a file edit.

Choose the kind:
- "command" when a shell command would do what the user wants, including requests phrased as questions ("how do I find large files?").
- "script" when the task needs real logic (loops, parsing, data processing, several dependent steps) that is awkward as one command, or when the user asks for something to be done "with a script" / "programmatically". The script is temporary: it runs once from a temp file and is deleted, so nothing is saved in the project.
- "edit" when the user wants a file in the project changed, fixed or created and kept. Prefer this over commands that rewrite file contents.
- "answer" when the user wants knowledge or an explanation rather than an action ("what does chmod 755 mean?", "merge vs rebase?"), or when nothing else fits.
Leave the fields of the other kinds empty: "" for strings, [] for edits, and risk "safe" unless the kind is command or script.
When <context> says "descriptions: off", also leave explanation and summary empty.

For a command:
- Write it for the shell and OS given in <context>, using that shell's own syntax and built-ins.
- Return one command line. Chain steps with the shell's operators when needed; write a short multi-line script only when one line cannot do it.
- It runs in a child process in the given working directory: `cd` and environment changes do not persist after it exits.
- Prefer tools that are already installed. Do not add sudo or elevation unless the request needs it.
- Use the file names from <context> when relevant. If a value is truly unknown, write a placeholder like <FILE> and mention it in the explanation.
- command: the command only, with no markdown fences or prompt characters.
- explanation: one short sentence on what it does.
- risk: "safe" if read-only; "caution" if it changes files, settings or installed software but is recoverable; "dangerous" if it deletes data, overwrites without a backup, force-pushes, changes system-wide settings, or is otherwise hard to undo.

For a script:
- language: one of the interpreters listed in <context>; use the one the user names if it is listed.
- script: the complete source. It runs with the working directory from <context> as its current directory, so use relative paths. Print results to stdout.
- Keep it short and self-contained, using only the standard library. Do not ask for input.
- explanation: one short sentence on what it does.
- risk: the same scale as for commands, judged by what the script does to files, processes and settings.

For an answer:
- answer: brief and direct, a few sentences or a short list.
- It is shown in a plain terminal: no markdown headings, bold, or tables. Put code or commands on their own lines, indented by two spaces.
- Use <context> when the question is about this machine or folder.

For an edit:
- path: the one file to change, relative to the working directory.
- Files the user mentioned are attached in <file> blocks. If the file to change exists but is not attached, return the edit with path set and edits empty; it will be attached and you will be asked again.
- edits: exact text replacements, applied in order. Each `old` is copied verbatim from the file, including indentation and blank lines, and appears exactly once in it; include a few surrounding lines when needed to make it unique. `new` is the full replacement for `old`.
- Keep edits small; rewrite the whole file only when asked to. For a new or empty file, return one edit with `old` "" and `new` set to the full content.
- Match the file's language, style and indentation. Do not add comments describing the change.
- summary: one short sentence describing the change."#;

/// Auto-attached files: mentioned in the request, small, and text.
const ATTACH_MAX_FILES: usize = 3;
const ATTACH_MAX_BYTES: usize = 64 * 1024;
/// A file the model asks to edit may be larger.
const EDIT_MAX_BYTES: usize = 512 * 1024;
const MAX_ROUNDS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Risk {
    Safe,
    Caution,
    Dangerous,
}

impl Risk {
    fn parse(s: Option<&str>) -> Risk {
        match s {
            Some("safe") => Risk::Safe,
            Some("dangerous") => Risk::Dangerous,
            _ => Risk::Caution,
        }
    }

    /// Local second opinion, so a small model under-rating a command can't
    /// make it look harmless.
    fn scan(command: &str) -> Risk {
        let mut c = command.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        // Discarding output is not writing a file.
        for quiet in ["2>&1", "2>$null", "2>/dev/null", "2>nul", ">$null", ">/dev/null", ">nul"] {
            c = c.replace(quiet, " ").replace(&quiet.replace('>', "> "), " ");
        }
        let tokens: Vec<&str> = c
            .split(|ch: char| ch.is_whitespace() || ";|&(){}".contains(ch))
            .filter(|t| !t.is_empty())
            .collect();
        let has_word = |words: &[&str]| tokens.iter().any(|t| words.contains(t));
        let has_phrase = |phrases: &[&str]| phrases.iter().any(|p| c.contains(p));

        const DANGEROUS_WORDS: &[&str] = &[
            "format", "format-volume", "clear-disk", "diskpart", "shutdown", "reboot", "halt",
            "poweroff", "restart-computer", "stop-computer",
        ];
        const DANGEROUS_PHRASES: &[&str] = &[
            "rm -rf", "rm -fr", "rm -r ", "-recurse -force", "-force -recurse", "del /s", "rd /s",
            "rmdir /s", "mkfs", "dd if=", "git push -f", "git push --force", "git reset --hard",
            "git clean -f", "reg delete", ":(){", "chmod -r 777 /", "shutil.rmtree", "fs.rmsync",
            "rimraf",
        ];
        // Commands that write, install, kill, or change settings. Safe
        // commands run without asking, so these lists err toward asking.
        const CAUTION_WORDS: &[&str] = &[
            // files
            "rm", "rmdir", "rd", "del", "erase", "mv", "move", "cp", "copy", "xcopy", "robocopy",
            "ren", "rename", "mkdir", "md", "touch", "tee", "ln", "truncate", "shred", "attrib",
            // PowerShell aliases for writing cmdlets
            "ni", "ri", "mi", "cpi", "rni", "sc", "si", "sp",
            // permissions, elevation, processes, services, settings
            "chmod", "chown", "icacls", "takeown", "sudo", "su", "runas", "kill", "pkill",
            "killall", "taskkill", "systemctl", "service", "net", "reg", "setx", "schtasks",
            "crontab", "iex",
            // package managers and builds
            "npm", "npx", "pnpm", "yarn", "pip", "pip3", "cargo", "go", "make", "winget", "choco",
            "scoop", "apt", "apt-get", "dnf", "yum", "pacman", "brew", "msiexec", "install",
            "uninstall",
        ];
        // PowerShell verbs that change things; Get-, Select-, Where-, Sort-,
        // Measure-, Format-, Test-, ConvertTo- and friends stay safe.
        const CAUTION_VERBS: &[&str] = &[
            "set-", "new-", "remove-", "clear-", "add-", "rename-", "move-", "copy-", "stop-",
            "start-", "restart-", "suspend-", "resume-", "enable-", "disable-", "install-",
            "uninstall-", "update-", "register-", "unregister-", "reset-", "invoke-", "export-",
            "out-file", "tee-object", "expand-archive", "compress-archive",
        ];
        const CAUTION_PHRASES: &[&str] = &[
            ">", "sed -i", "| sh", "| bash", "|sh", "|bash", " -o ", "--output", "-outfile",
            "git push", "git commit", "git checkout", "git switch", "git reset", "git rebase",
            "git merge", "git pull", "git stash", "git rm", "git mv", "git clean", "git add",
            "git restore", "git branch -d", "git tag", "git cherry-pick", "git revert", "git apply",
            "git init", "git clone", "docker rm", "docker stop", "docker kill", "docker run",
            "docker system prune",
        ];

        if has_word(DANGEROUS_WORDS) || has_phrase(DANGEROUS_PHRASES) {
            Risk::Dangerous
        } else if has_word(CAUTION_WORDS)
            || has_phrase(CAUTION_PHRASES)
            || tokens.iter().any(|t| {
                !matches!(*t, "set-location" | "start-sleep") && CAUTION_VERBS.iter().any(|v| t.starts_with(v))
            })
        {
            Risk::Caution
        } else {
            Risk::Safe
        }
    }

    fn paint(self, s: &str) -> String {
        match self {
            Risk::Safe => ui::green(s),
            Risk::Caution => ui::yellow(s),
            Risk::Dangerous => ui::bold_red(s),
        }
    }
}

/// Up to 50 names from the working directory, directories marked with `/`.
fn listing() -> String {
    let Ok(entries) = std::fs::read_dir(".") else { return String::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| {
            let mut name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                name.push('/');
            }
            name
        })
        .collect();
    names.sort_by_key(|n| n.to_ascii_lowercase());
    let total = names.len();
    names.truncate(50);
    let mut out = names.join(", ");
    if total > 50 {
        out.push_str(&format!(", ... ({} more)", total - 50));
    }
    out
}

/// Existing small text files named in the request, e.g. `fix deploy.sh`.
fn mentioned_files(request: &str) -> Vec<FileText> {
    let mut files: Vec<FileText> = Vec::new();
    for word in request.split_whitespace() {
        let word = word
            .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '(' | ')' | ','))
            .trim_end_matches(['.', ':', ';', '!', '?']);
        if word.is_empty() || !Path::new(word).is_file() || files.iter().any(|f| f.is(word)) {
            continue;
        }
        if let Ok(file) = FileText::load(word, ATTACH_MAX_BYTES) {
            files.push(file);
            if files.len() == ATTACH_MAX_FILES {
                break;
            }
        }
    }
    files
}

fn schema(interpreters: &[Interpreter]) -> Value {
    let mut language = json!({ "type": "string" });
    if !interpreters.is_empty() {
        let mut names: Vec<&str> = interpreters.iter().map(|i| i.language).collect();
        names.push("");
        language["enum"] = json!(names);
    }
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["command", "script", "answer", "edit"] },
            "command": { "type": "string" },
            "language": language,
            "script": { "type": "string" },
            "explanation": { "type": "string" },
            "risk": { "type": "string", "enum": ["safe", "caution", "dangerous"] },
            "answer": { "type": "string" },
            "path": { "type": "string" },
            "summary": { "type": "string" },
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "old": { "type": "string" }, "new": { "type": "string" } },
                    "required": ["old", "new"],
                    "additionalProperties": false,
                },
            },
        },
        "required": [
            "kind", "command", "language", "script", "explanation", "risk", "answer", "path", "summary", "edits",
        ],
        "additionalProperties": false,
    })
}

pub fn run(llm: &Llm, cfg: &Config, request: &str, print_only: bool) -> Result<ExitCode, String> {
    let shell = Shell::detect(cfg);
    let interpreters = shell::interpreters();
    let languages: Vec<&str> = interpreters.iter().map(|i| i.language).collect();
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let describe = cfg.get("descriptions") == Some("on");
    let context = format!(
        "<context>\nos: {} {}\nshell: {}\ninterpreters: {}\ncwd: {cwd}\nfiles: {}\ndescriptions: {}\n</context>",
        std::env::consts::OS,
        std::env::consts::ARCH,
        shell.describe(),
        if languages.is_empty() { "none".into() } else { languages.join(", ") },
        listing(),
        if describe { "on" } else { "off" },
    );
    let mut files = mentioned_files(request);
    let mut note = String::new();
    let mut activity = if files.is_empty() {
        ui::verb().to_string()
    } else {
        format!("Reading {}", files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join(", "))
    };

    // Usually one round. Another only when an edit needs a file that wasn't
    // attached, or its `old` text didn't match.
    for round in 1..=MAX_ROUNDS {
        let last = round == MAX_ROUNDS;
        let blocks: String = files.iter().map(|f| f.block() + "\n\n").collect();
        let user = format!("{context}\n\n{blocks}<request>\n{request}\n</request>{note}");
        let mut v = llm::complete(
            llm,
            &Prompt {
                system: SYSTEM,
                user: &user,
                schema_name: "reply",
                schema: schema(&interpreters),
                max_tokens: 16000,
            },
            &activity,
        )?;
        // The model is told to skip them; this covers models that don't listen.
        if !describe {
            v["explanation"] = json!("");
            v["summary"] = json!("");
        }

        match v["kind"].as_str() {
            Some("edit") => {
                let path = v["path"].as_str().unwrap_or("").trim();
                if path.is_empty() {
                    return Err("the model chose to edit a file but gave no path".into());
                }
                let file = match files.iter().find(|f| f.is(path)) {
                    Some(f) => f.clone(),
                    None => {
                        let file = FileText::load(path, EDIT_MAX_BYTES)?;
                        if file.exists && !last {
                            note = format!("\n\n{path} is now attached; make the edit against its content.");
                            activity = format!("Reading {path}");
                            files.push(file);
                            continue;
                        }
                        file
                    }
                };
                let summary = v["summary"].as_str().unwrap_or("").trim();
                match edit::apply(&file.text, &v["edits"]) {
                    Ok(updated) => return edit::review_and_write(&file, &updated, summary, print_only),
                    Err(e) if !last => {
                        eprintln!("{}", ui::dim(&format!("retrying: {e}")));
                        activity = format!("Fixing the edit to {path}");
                        note = format!(
                            "\n\nYour previous edits to {path} could not be applied: {e}. Copy each `old` exactly from the file and make it unique."
                        );
                    }
                    Err(e) => return Err(format!("could not apply the edits: {e}")),
                }
            }
            Some("script") => return script(&v, &interpreters, print_only),
            _ => {
                let auto_run = cfg.get("auto_run") != Some("never");
                return reply(&v, &shell, print_only, auto_run);
            }
        }
    }
    Err("the model did not produce a usable edit".into())
}

/// Ask before running something. Dangerous things need a typed "yes".
/// Returns whether to run, and the raw answer (for [c]opy).
fn confirm(risk: Risk) -> (bool, String) {
    let answer = if risk == Risk::Dangerous {
        ui::ask(&format!("Type {} to run, [c]opy, or Enter to cancel: ", ui::bold("yes")))
    } else {
        ui::ask(&format!("Run? {} / [c]opy / [N]o: ", ui::bold("[y]es")))
    };
    let run = match risk {
        Risk::Dangerous => answer.eq_ignore_ascii_case("yes"),
        _ => ui::yes(&answer),
    };
    (run, answer)
}

fn copy_if_asked(answer: &str, text: &str) -> Result<ExitCode, String> {
    if answer.eq_ignore_ascii_case("c") {
        shell::copy(text)?;
        eprintln!("{}", ui::dim("copied to clipboard"));
    }
    Ok(ExitCode::SUCCESS)
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// Show a temporary script and run it on confirmation. Scripts always ask:
/// unlike a one-line command, jin can't tell whether one only reads.
fn script(v: &Value, interpreters: &[Interpreter], print_only: bool) -> Result<ExitCode, String> {
    let language = v["language"].as_str().unwrap_or("").trim();
    let source = v["script"].as_str().unwrap_or("").trim_end();
    let explanation = v["explanation"].as_str().unwrap_or("").trim();
    if source.trim().is_empty() {
        return Err("the model chose a script but returned no code".into());
    }
    let interpreter = interpreters
        .iter()
        .find(|i| i.language == language)
        .ok_or_else(|| format!("the model wrote a {language} script, but no {language} interpreter was found"))?;

    if print_only || !io::stdin().is_terminal() {
        println!("{source}");
        return Ok(ExitCode::SUCCESS);
    }
    let risk = Risk::parse(v["risk"].as_str()).max(Risk::scan(source)).max(Risk::Caution);

    eprintln!();
    eprintln!("  {} {}", risk.paint(&format!("{language} script")), ui::dim("(temp)"));
    for line in source.lines() {
        eprintln!("  {} {line}", ui::dim("│"));
    }
    if !explanation.is_empty() {
        eprintln!("  {}", ui::dim(explanation));
    }
    if risk == Risk::Dangerous {
        eprintln!("  {}", ui::bold_red("! destructive or hard to undo - read it before running"));
    }
    eprintln!();

    let (run, answer) = confirm(risk);
    if !run {
        return copy_if_asked(&answer, source);
    }
    let status = interpreter.run(source).map_err(|e| format!("could not run the {language} script: {e}"))?;
    Ok(exit_code(status))
}

/// Print an answer, or show a command and run it: right away when it is
/// read-only and `auto_run` is on, otherwise after confirmation.
fn reply(v: &Value, shell: &Shell, print_only: bool, auto_run: bool) -> Result<ExitCode, String> {
    let command = v["command"].as_str().unwrap_or("").trim().to_string();
    let explanation = v["explanation"].as_str().unwrap_or("").trim();
    let answer = v["answer"].as_str().unwrap_or("").trim();

    // An answer is the output itself: print it and stop.
    if v["kind"] == "answer" || command.is_empty() {
        let Some(text) = [answer, explanation].into_iter().find(|s| !s.is_empty()) else {
            ui::error("the model returned an empty reply");
            return Ok(ExitCode::from(1));
        };
        println!("{text}");
        return Ok(ExitCode::SUCCESS);
    }
    let risk = Risk::parse(v["risk"].as_str()).max(Risk::scan(&command));

    if print_only {
        println!("{command}");
        return Ok(ExitCode::SUCCESS);
    }
    // Read-only per both the model and the local scan: just run it.
    if risk == Risk::Safe && auto_run {
        eprintln!("{} {}", ui::dim(">"), risk.paint(&command.replace('\n', "\n  ")));
        return execute(shell, &command);
    }
    // No terminal to confirm on: the command is the output.
    if !io::stdin().is_terminal() {
        println!("{command}");
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!();
    eprintln!("  {}", risk.paint(&command.replace('\n', "\n  ")));
    if !explanation.is_empty() {
        eprintln!("  {}", ui::dim(explanation));
    }
    if risk == Risk::Dangerous {
        eprintln!("  {}", ui::bold_red("! destructive or hard to undo - read it before running"));
    }
    eprintln!();

    let (run, answer) = confirm(risk);
    if run {
        return execute(shell, &command);
    }
    copy_if_asked(&answer, &command)
}

fn execute(shell: &Shell, command: &str) -> Result<ExitCode, String> {
    let status = shell.run(command).map_err(|e| {
        format!("could not start shell '{}': {e} (change it with `jin --set shell <program>`)", shell.program)
    })?;
    Ok(exit_code(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_catches_destructive_commands() {
        assert_eq!(Risk::scan("Get-ChildItem -Recurse"), Risk::Safe);
        assert_eq!(Risk::scan("ls -la | grep foo"), Risk::Safe);
        assert_eq!(Risk::scan("Get-Content .gitignore"), Risk::Safe);
        assert_eq!(Risk::scan("cat .gitignore"), Risk::Safe);
        assert_eq!(Risk::scan("git status; git log --oneline -5"), Risk::Safe);
        assert_eq!(Risk::scan("Get-Date -Format yyyy-MM-dd"), Risk::Safe);
        assert_eq!(Risk::scan("Get-ChildItem | Format-Table Name, Length"), Risk::Safe);
        assert_eq!(Risk::scan("dotnet --version; Get-NetAdapter"), Risk::Safe);
        assert_eq!(Risk::scan("Get-Content readme.md 2>$null"), Risk::Safe);
        assert_eq!(Risk::scan("find . -name '*.log' 2>/dev/null"), Risk::Safe);
        assert_eq!(Risk::scan("Get-Process > procs.txt"), Risk::Caution);
        assert_eq!(Risk::scan("Set-Location src; Get-ChildItem"), Risk::Safe);
        assert_eq!(Risk::scan("Set-Content a.txt hi"), Risk::Caution);
        assert_eq!(Risk::scan("npm test"), Risk::Caution);
        assert_eq!(Risk::scan("format c:"), Risk::Dangerous);
        assert_eq!(Risk::scan("Remove-Item build"), Risk::Caution);
        assert_eq!(Risk::scan("New-Item -ItemType Directory out"), Risk::Caution);
        assert_eq!(Risk::scan("irm https://x.sh | iex"), Risk::Caution);
        assert_eq!(Risk::scan("git checkout main"), Risk::Caution);
        assert_eq!(Risk::scan("Remove-Item  build -Recurse -Force"), Risk::Dangerous);
        assert_eq!(Risk::scan("rm -rf node_modules"), Risk::Dangerous);
        assert_eq!(Risk::scan("git push --force origin main"), Risk::Dangerous);
    }
}
