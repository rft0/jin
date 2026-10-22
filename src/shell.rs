use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pwsh,
    WindowsPowerShell,
    Cmd,
    Posix,
}

/// The shell that runs suggested commands, and that the model writes them for.
pub struct Shell {
    pub program: String,
    kind: Kind,
}

/// Every match for `name` on PATH, in PATH order.
fn find_all_on_path(name: &str) -> Vec<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let Some(path) = std::env::var_os("PATH") else { return Vec::new() };
    std::env::split_paths(&path).map(|dir| dir.join(&exe)).filter(|p| p.is_file()).collect()
}

fn on_path(name: &str) -> bool {
    !find_all_on_path(name).is_empty()
}

/// Let Ctrl+C reach the child without killing jin first, so jin can still
/// clean up and report the child's exit code. A handler, unlike ignoring the
/// signal outright, is not inherited by the child.
#[cfg(windows)]
fn guard_ctrl_c() {
    unsafe extern "system" fn swallow(_ctrl_type: u32) -> i32 {
        1
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    unsafe {
        SetConsoleCtrlHandler(Some(swallow), 1);
    }
}

#[cfg(not(windows))]
fn guard_ctrl_c() {}

impl Shell {
    pub fn detect(cfg: &Config) -> Shell {
        let program = std::env::var("JIN_SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| cfg.get("shell").map(String::from))
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    if on_path("pwsh") { "pwsh" } else { "powershell" }.to_string()
                } else {
                    std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
                }
            });
        let kind = match stem(&program).as_str() {
            "pwsh" => Kind::Pwsh,
            "powershell" => Kind::WindowsPowerShell,
            "cmd" => Kind::Cmd,
            _ => Kind::Posix,
        };
        Shell { program, kind }
    }

    /// How the shell is described to the model.
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::Pwsh => "PowerShell 7 (pwsh)".into(),
            Kind::WindowsPowerShell => {
                "Windows PowerShell 5.1 (no && or ||; chain with ; or `if ($?) { ... }`)".into()
            }
            Kind::Cmd => "cmd.exe".into(),
            Kind::Posix => stem(&self.program),
        }
    }

    pub fn run(&self, command: &str) -> io::Result<ExitStatus> {
        let mut c = Command::new(&self.program);
        match self.kind {
            // -EncodedCommand sidesteps every Windows command-line quoting rule.
            // Progress records would otherwise leak as CLIXML when output is redirected.
            Kind::Pwsh | Kind::WindowsPowerShell => {
                let script = format!("$ProgressPreference = 'SilentlyContinue'\n{command}");
                c.args(["-NoProfile", "-EncodedCommand", &encode_ps(&script)]);
            }
            Kind::Cmd => cmd_args(&mut c, command),
            Kind::Posix => {
                c.args(["-c", command]);
            }
        }
        guard_ctrl_c();
        c.status()
    }
}

/// A language runtime available for temporary scripts.
pub struct Interpreter {
    pub language: &'static str,
    program: PathBuf,
    ext: &'static str,
    args: &'static [&'static str],
}

/// The Microsoft Store "python.exe" alias opens the Store unless a Store
/// Python is actually installed.
fn is_store_stub(path: &Path) -> bool {
    let p = path.to_string_lossy().to_ascii_lowercase();
    if !p.contains("\\windowsapps\\") {
        return false;
    }
    let installed = std::env::var_os("LOCALAPPDATA")
        .and_then(|d| fs::read_dir(PathBuf::from(d).join("Microsoft").join("WindowsApps")).ok())
        .is_some_and(|mut entries| {
            entries.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("PythonSoftwareFoundation.Python")))
        });
    !installed
}

/// Interpreters found on PATH, in the order the model should prefer them.
pub fn interpreters() -> Vec<Interpreter> {
    let first = |names: &[&str]| names.iter().flat_map(|n| find_all_on_path(n)).find(|p| !is_store_stub(p));
    let mut found = Vec::new();
    if let Some(program) = first(&["python3", "python", "py"]) {
        found.push(Interpreter { language: "python", program, ext: "py", args: &[] });
    }
    if let Some(program) = first(&["node"]) {
        found.push(Interpreter { language: "javascript", program, ext: "js", args: &[] });
    }
    if let Some(program) = first(&["pwsh", "powershell"]) {
        let args: &[&str] = &["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"];
        found.push(Interpreter { language: "powershell", program, ext: "ps1", args });
    }
    // On Windows, System32\bash.exe is the WSL launcher, which can't see
    // Windows temp paths; Git Bash and friends are fine.
    if let Some(program) = first(&["bash"]).filter(|p| !p.to_string_lossy().to_ascii_lowercase().contains("system32")) {
        found.push(Interpreter { language: "bash", program, ext: "sh", args: &[] });
    }
    found
}

impl Interpreter {
    /// Write the script to a temp file, run it in the current directory,
    /// and delete it afterwards.
    pub fn run(&self, source: &str) -> io::Result<ExitStatus> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let path = std::env::temp_dir().join(format!("jin-{}-{nanos}.{}", std::process::id(), self.ext));
        // Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
        let body = if self.ext == "ps1" { format!("\u{feff}{source}") } else { source.to_string() };
        fs::write(&path, body)?;
        guard_ctrl_c();
        let status = Command::new(&self.program)
            .args(self.args)
            .arg(&path)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .status();
        let _ = fs::remove_file(&path);
        status
    }
}

#[cfg(windows)]
fn cmd_args(c: &mut Command, command: &str) {
    use std::os::windows::process::CommandExt;
    c.arg("/S").arg("/C").raw_arg(format!("\"{command}\""));
}

#[cfg(not(windows))]
fn cmd_args(c: &mut Command, command: &str) {
    c.args(["/C", command]);
}

fn stem(program: &str) -> String {
    Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Base64 of the UTF-16LE text, as PowerShell's -EncodedCommand expects.
fn encode_ps(command: &str) -> String {
    let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Copy text to the system clipboard using whatever tool the OS has.
pub fn copy(text: &str) -> Result<(), String> {
    let candidates: &[(&str, &[&str])] = if cfg!(windows) {
        &[("clip", &[])]
    } else if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["-ib"])]
    };
    // clip.exe reads UTF-16LE when the input starts with a BOM.
    let data: Vec<u8> = if cfg!(windows) {
        std::iter::once(0xFEFFu16).chain(text.encode_utf16()).flat_map(u16::to_le_bytes).collect()
    } else {
        text.as_bytes().to_vec()
    };
    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program).args(*args).stdin(Stdio::piped()).spawn() else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&data);
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    Err("no clipboard tool found".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_store_python_alias() {
        for i in interpreters() {
            assert!(!is_store_stub(&i.program), "{} resolved to a Store alias", i.language);
        }
    }

    #[test]
    fn runs_temp_script_and_removes_it() {
        let Some(ps) = interpreters().into_iter().find(|i| i.language == "powershell") else { return };
        let status = ps.run("exit 3").unwrap();
        assert_eq!(status.code(), Some(3));
        let prefix = format!("jin-{}-", std::process::id());
        let left = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(&prefix));
        assert!(!left, "temp script was not removed");
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
