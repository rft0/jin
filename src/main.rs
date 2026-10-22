mod config;
mod edit;
mod llm;
mod shell;
mod suggest;
mod ui;

use std::process::ExitCode;

const HELP: &str = "\
jin - one-shot AI help for shell commands, quick questions and script edits

USAGE:
    jin [OPTIONS] <request...>

    jin decides what the request needs: a command, a temporary script (run
    from a temp file, then deleted), an answer (printed), or a file edit
    (diff shown, written on confirmation). Read-only commands run right
    away; everything else asks first. Files named in the request are sent
    along.

OPTIONS:
    -p, --provider <name>   anthropic | openai | openrouter (this run only)
    -m, --model <id>        model id (this run only)
    -s, --smart             use the provider's stronger model
    -n, --print             print the command or diff only; never run or write
        --undo              restore the file changed by the last edit
    -h, --help              show this help
    -V, --version           show version

SETUP (saved in ~/.jin/config.json):
    jin --key <provider> [key]      save an API key; asks with hidden input if omitted
    jin --use <provider> [model]    set the default provider, and optionally its model
    jin --set <name> [value]        set a setting, or reset it when no value is given:
                                      shell, auto_run (safe | never),
                                      descriptions (on | off, default off),
                                      spinner (see jin --spinners),
                                      <provider>.model, <provider>.smart_model,
                                      <provider>.effort, <provider>.smart_effort (off = none),
                                      <provider>.base_url
    jin --config                    show settings (keys masked)
    jin --spinners                  list spinner styles

EXAMPLES:
    jin --key openrouter
    jin --use openrouter anthropic/claude-haiku-4.5
    jin find files over 100MB in this folder
    jin -s show which process is listening on port 3000
    jin what is the difference between git merge and rebase
    jin count lines per file extension in src with a python script
    jin add logging to backup.ps1, written next to the script
";

pub struct Args {
    provider: Option<String>,
    model: Option<String>,
    smart: bool,
    print_only: bool,
    request: String,
}

enum Action {
    Help,
    Version,
    Config,
    Spinners,
    Undo,
    Key(String, Option<String>),
    Use(String, Option<String>),
    Set(String, Option<String>),
    Run(Args),
}

/// `<flag> <first> [second]` for the setup commands.
fn setup_args(flag: &str, rest: Vec<String>) -> Result<(String, Option<String>), String> {
    let mut rest = rest.into_iter();
    let first = rest.next().ok_or_else(|| format!("{flag} needs a value (see jin --help)"))?;
    let second = rest.next();
    if rest.next().is_some() {
        return Err(format!("too many values for {flag} (see jin --help)"));
    }
    Ok((first, second))
}

fn parse_args() -> Result<Action, String> {
    let mut it = std::env::args().skip(1);
    let mut args = Args {
        provider: None,
        model: None,
        smart: false,
        print_only: false,
        request: String::new(),
    };
    let mut words: Vec<String> = Vec::new();
    let mut any = false;

    while let Some(arg) = it.next() {
        any = true;
        // Options are only read before the request; everything after the
        // first word belongs to the request (so `jin ls -la` works).
        if !words.is_empty() {
            words.push(arg);
            continue;
        }
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "--config" => return Ok(Action::Config),
            "--spinners" => return Ok(Action::Spinners),
            "--undo" => return Ok(Action::Undo),
            "--key" | "--use" | "--set" => {
                let (first, second) = setup_args(&arg, it.by_ref().collect())?;
                return Ok(match arg.as_str() {
                    "--key" => Action::Key(first, second),
                    "--use" => Action::Use(first, second),
                    _ => Action::Set(first, second),
                });
            }
            "-p" | "--provider" => args.provider = Some(value(&arg)?),
            "-m" | "--model" => args.model = Some(value(&arg)?),
            "-s" | "--smart" => args.smart = true,
            "-n" | "--print" => args.print_only = true,
            "--" => {
                words.extend(it.by_ref());
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option '{s}' (see jin --help)"));
            }
            _ => words.push(arg),
        }
    }

    if !any {
        return Ok(Action::Help);
    }
    args.request = words.join(" ").trim().to_string();
    if args.request.is_empty() {
        return Err("missing request (see jin --help)".into());
    }
    Ok(Action::Run(args))
}

fn run() -> Result<ExitCode, String> {
    match parse_args()? {
        Action::Help => {
            print!("{HELP}");
            Ok(ExitCode::SUCCESS)
        }
        Action::Version => {
            println!("jin {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Action::Config => config::show(),
        Action::Spinners => {
            ui::set_spinner(config::Config::load()?.get("spinner"));
            ui::list_spinners();
            Ok(ExitCode::SUCCESS)
        }
        Action::Undo => edit::undo(),
        Action::Key(provider, key) => config::set_key(&provider, key.as_deref()),
        Action::Use(provider, model) => config::use_provider(&provider, model.as_deref()),
        Action::Set(name, value) => config::set(&name, value.as_deref()),
        Action::Run(args) => {
            let cfg = config::Config::load()?;
            ui::set_spinner(cfg.get("spinner"));
            let llm = cfg.resolve(args.provider.as_deref(), args.model.as_deref(), args.smart)?;
            suggest::run(&llm, &cfg, &args.request, args.print_only)
        }
    }
}

fn main() -> ExitCode {
    ui::init();
    match run() {
        Ok(code) => code,
        Err(e) => {
            ui::error(&e);
            ExitCode::from(1)
        }
    }
}
