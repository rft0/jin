//! Terminal output. Everything interactive goes to stderr so stdout stays clean
//! for `--print` and for the output of commands jin runs.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// stderr is a terminal that understands ANSI escapes.
static VT: AtomicBool = AtomicBool::new(false);
static COLOR: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let vt = io::stderr().is_terminal() && enable_vt();
    VT.store(vt, Ordering::Relaxed);
    COLOR.store(vt && std::env::var_os("NO_COLOR").is_none(), Ordering::Relaxed);
}

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

fn cursor(visible: bool) {
    if VT.load(Ordering::Relaxed) {
        eprint!("{}", if visible { SHOW_CURSOR } else { HIDE_CURSOR });
        let _ = io::stderr().flush();
    }
}

/// If Ctrl+C ends jin while the cursor is hidden, show it again on the way
/// out so the terminal isn't left without one.
#[cfg(windows)]
fn restore_cursor_on_interrupt() {
    unsafe extern "system" fn restore(_ctrl_type: u32) -> i32 {
        let _ = io::stderr().write_all(SHOW_CURSOR.as_bytes());
        0 // not handled: the default handler still ends the process
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    unsafe {
        SetConsoleCtrlHandler(Some(restore), 1);
    }
}

#[cfg(unix)]
fn restore_cursor_on_interrupt() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
        fn _exit(status: i32) -> !;
    }
    // Only async-signal-safe calls in here.
    extern "C" fn on_sigint(_: i32) {
        let msg = b"\x1b[?25h\n";
        unsafe {
            write(2, msg.as_ptr(), msg.len());
            _exit(130);
        }
    }
    const SIGINT: i32 = 2;
    unsafe {
        signal(SIGINT, on_sigint as extern "C" fn(i32) as usize);
    }
}

#[cfg(not(any(windows, unix)))]
fn restore_cursor_on_interrupt() {}

#[cfg(windows)]
mod win {
    pub type Handle = *mut core::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetStdHandle(n: u32) -> Handle;
        pub fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }
    pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
}

#[cfg(windows)]
fn enable_vt() -> bool {
    use win::*;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            // Not a Windows console (e.g. mintty); those understand ANSI if TERM is set.
            return std::env::var_os("TERM").is_some();
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

fn paint(code: &str, s: &str) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn bold_red(s: &str) -> String {
    paint("1;31", s)
}

pub fn error(msg: &str) {
    eprintln!("{} {msg}", red("jin:"));
}

/// Print a prompt on stderr and read one trimmed line from stdin.
pub fn ask(prompt: &str) -> String {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}

/// Like `ask`, but with terminal echo turned off while typing.
pub fn ask_secret(prompt: &str) -> String {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    let restore = echo_off();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    restore();
    eprintln!();
    line.trim().to_string()
}

#[cfg(windows)]
fn echo_off() -> impl FnOnce() {
    use win::*;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        let ok = GetConsoleMode(h, &mut mode) != 0 && SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) != 0;
        move || {
            if ok {
                SetConsoleMode(h, mode);
            }
        }
    }
}

#[cfg(not(windows))]
fn echo_off() -> impl FnOnce() {
    use std::process::{Command, Stdio};
    let stty = |arg: &str| Command::new("stty").arg(arg).stdin(Stdio::inherit()).status().is_ok_and(|s| s.success());
    let ok = stty("-echo");
    move || {
        if ok {
            stty("echo");
        }
    }
}

pub fn yes(answer: &str) -> bool {
    matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes")
}

/// A word for "working on it", different from run to run.
pub fn verb() -> &'static str {
    const VERBS: [&str; 10] = [
        "Thinking", "Pondering", "Working", "Brewing", "Cooking", "Figuring", "Tinkering", "Mulling",
        "Conjuring", "Crunching",
    ];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as usize);
    VERBS[nanos % VERBS.len()]
}

/// Spinner styles: name, frames, milliseconds per frame. "none" drops the
/// animation but keeps the activity text and timer.
pub const SPINNERS: &[(&str, &[&str], u64)] = &[
    ("dots", &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"], 80),
    ("line", &["-", "\\", "|", "/"], 100),
    ("circle", &["◐", "◓", "◑", "◒"], 120),
    ("arc", &["◜", "◠", "◝", "◞", "◡", "◟"], 100),
    ("bounce", &["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"], 90),
    ("star", &["✶", "✸", "✹", "✺", "✹", "✷"], 110),
    ("pulse", &["·", "•", "●", "•"], 150),
    ("bar", &["▁", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃"], 90),
    ("none", &[], 100),
];

static SPINNER: AtomicUsize = AtomicUsize::new(0);

/// Pick the spinner style by name; unknown names keep the default.
pub fn set_spinner(name: Option<&str>) {
    if let Some(i) = SPINNERS.iter().position(|s| Some(s.0) == name) {
        SPINNER.store(i, Ordering::Relaxed);
    }
}

pub fn spinner_names() -> Vec<&'static str> {
    SPINNERS.iter().map(|s| s.0).collect()
}

/// `jin --spinners`: every style's frames, the current one marked.
pub fn list_spinners() {
    let current = SPINNER.load(Ordering::Relaxed);
    for (i, (name, frames, _)) in SPINNERS.iter().enumerate() {
        let mark = if i == current { green("*") } else { " ".into() };
        let shown = if frames.is_empty() { dim("(text and timer only)") } else { cyan(&frames.join(" ")) };
        println!("{mark} {name:<7} {shown}");
    }
    println!("\nchange with: jin --set spinner <name>");
}

/// Spinner on stderr with what jin is doing and how long it has taken;
/// cleared on drop.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(label: String) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        let (_, frames, interval) = SPINNERS[SPINNER.load(Ordering::Relaxed)];
        let handle = io::stderr().is_terminal().then(|| {
            static HOOK: std::sync::Once = std::sync::Once::new();
            HOOK.call_once(restore_cursor_on_interrupt);
            cursor(false);
            let stop = stop.clone();
            thread::spawn(move || {
                let started = std::time::Instant::now();
                let mut width = 0;
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let elapsed = format!("{:.1}s", started.elapsed().as_secs_f32());
                    width = width.max(label.chars().count() + elapsed.len() + 4);
                    let frame = if frames.is_empty() { String::new() } else { cyan(frames[i % frames.len()]) + " " };
                    eprint!("\r{frame}{label} {}", dim(&elapsed));
                    let _ = io::stderr().flush();
                    i += 1;
                    thread::park_timeout(Duration::from_millis(interval));
                }
                eprint!("\r{}\r", " ".repeat(width));
                let _ = io::stderr().flush();
            })
        });
        Spinner { stop, handle }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
            cursor(true);
        }
    }
}
