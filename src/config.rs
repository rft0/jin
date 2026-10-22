use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    OpenAI,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Anthropic, Provider::OpenAI, Provider::OpenRouter];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::OpenRouter => "openrouter",
        }
    }

    fn parse(s: &str) -> Result<Provider, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "openai" => Ok(Provider::OpenAI),
            "openrouter" | "or" => Ok(Provider::OpenRouter),
            _ => Err(format!("unknown provider '{s}' (use anthropic, openai or openrouter)")),
        }
    }

    fn key_env(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAI => "https://api.openai.com/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// Default (model, effort) for the fast and smart tiers.
    fn defaults(self, smart: bool) -> (&'static str, Option<&'static str>) {
        match (self, smart) {
            (Provider::Anthropic, false) => ("claude-haiku-5-5", Some("low")),
            (Provider::Anthropic, true) => ("claude-sonnet-5-5", Some("medium")),
            (Provider::OpenAI, false) => ("gpt-5-nano", Some("minimal")),
            (Provider::OpenAI, true) => ("gpt-5-mini", Some("low")),
            (Provider::OpenRouter, false) => ("~anthropic/claude-haiku-latest", None),
            (Provider::OpenRouter, true) => ("~anthropic/claude-sonnet-latest", None),
        }
    }
}

/// Per-provider settings in config.json.
const FIELDS: [&str; 6] = ["api_key", "model", "smart_model", "effort", "smart_effort", "base_url"];

/// Everything needed to make one model call.
pub struct Llm {
    pub provider: Provider,
    pub model: String,
    pub effort: Option<String>,
    pub api_key: String,
    pub base_url: String,
}

/// `~/.jin/config.json`:
///
/// ```json
/// { "provider": "openrouter", "shell": "pwsh",
///   "providers": { "openrouter": { "api_key": "...", "model": "..." } } }
/// ```
pub struct Config {
    root: Value,
    path: Option<PathBuf>,
}

/// `~/.jin`, or `$JIN_HOME` when set.
pub fn jin_dir() -> Option<PathBuf> {
    env("JIN_HOME").map(PathBuf::from).or_else(|| {
        let home = if cfg!(windows) { std::env::var_os("USERPROFILE") } else { std::env::var_os("HOME") };
        home.map(|h| PathBuf::from(h).join(".jin"))
    })
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = jin_dir().map(|d| d.join("config.json"));
        let mut root = json!({});
        if let Some(p) = &path {
            match fs::read_to_string(p) {
                // Notepad and PowerShell 5.1 save UTF-8 with a BOM.
                Ok(text) => {
                    root = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                        .map_err(|e| format!("{} is not valid JSON: {e}", p.display()))?;
                    if !root.is_object() {
                        return Err(format!("{} must contain a JSON object", p.display()));
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("reading {}: {e}", p.display())),
            }
        }
        Ok(Config { root, path })
    }

    fn save(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("no home directory to save config in")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.root).unwrap_or_default() + "\n";
        fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        restrict(path);
        Ok(())
    }

    /// A non-empty top-level string setting, e.g. "provider" or "shell".
    pub fn get(&self, key: &str) -> Option<&str> {
        self.root[key].as_str().filter(|v| !v.is_empty())
    }

    fn field(&self, p: Provider, key: &str) -> Option<&str> {
        self.root["providers"][p.name()].get(key).and_then(Value::as_str)
    }

    fn provider_str(&self, p: Provider, key: &str) -> Option<&str> {
        self.field(p, key).filter(|v| !v.is_empty())
    }

    /// Set (or with `None`, remove) a string at a path of object keys.
    fn set_path(&mut self, path: &[&str], value: Option<String>) {
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut cur = &mut self.root;
        for key in parents {
            if !cur[*key].is_object() {
                cur[*key] = json!({});
            }
            cur = &mut cur[*key];
        }
        let obj = cur.as_object_mut().expect("parent is an object");
        match value {
            Some(v) => obj.insert(last.to_string(), Value::String(v)),
            None => obj.remove(*last),
        };
    }

    fn api_key(&self, p: Provider) -> Option<String> {
        env(p.key_env()).or_else(|| self.provider_str(p, "api_key").map(String::from))
    }

    fn path_display(&self) -> String {
        self.path.as_ref().map_or("<no home directory>".into(), |p| p.display().to_string())
    }

    pub fn resolve(&self, provider: Option<&str>, model: Option<&str>, smart: bool) -> Result<Llm, String> {
        let chosen = provider
            .map(String::from)
            .or_else(|| env("JIN_PROVIDER"))
            .or_else(|| self.get("provider").map(String::from));
        let provider = match chosen {
            Some(name) => Provider::parse(&name)?,
            // Nothing chosen: use the first provider that has a key.
            None => Provider::ALL
                .into_iter()
                .find(|p| self.api_key(*p).is_some())
                .unwrap_or(Provider::Anthropic),
        };
        let p = provider.name();
        let (model_key, effort_key) = if smart { ("smart_model", "smart_effort") } else { ("model", "effort") };
        let (default_model, default_effort) = provider.defaults(smart);

        let model = model
            .map(String::from)
            .or_else(|| if smart { None } else { env("JIN_MODEL") })
            .or_else(|| self.provider_str(provider, model_key).map(String::from))
            .unwrap_or_else(|| default_model.to_string());

        // A stored "" means "send none". The built-in default effort only
        // applies to the built-in default model.
        let effort = match self.field(provider, effort_key) {
            Some(v) => Some(v.to_string()).filter(|v| !v.is_empty()),
            None if model == default_model => default_effort.map(String::from),
            None => None,
        };

        let api_key = self
            .api_key(provider)
            .ok_or_else(|| format!("no API key for {p}: run `jin --key {p}` (or set {})", provider.key_env()))?;
        let base_url = self
            .provider_str(provider, "base_url")
            .unwrap_or(provider.default_base_url())
            .trim_end_matches('/')
            .to_string();

        Ok(Llm { provider, model, effort, api_key, base_url })
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

// The user profile directory is already private to the user on Windows.
#[cfg(not(unix))]
fn restrict(_path: &Path) {}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return "****".into();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

/// `jin --key <provider> [key]`
pub fn set_key(provider: &str, key: Option<&str>) -> Result<ExitCode, String> {
    let p = Provider::parse(provider)?;
    let key = match key {
        Some(k) => k.to_string(),
        None => ui::ask_secret(&format!("{} API key: ", p.name())),
    };
    // PowerShell 5.1 prefixes piped input with a BOM.
    let key = key.trim().trim_start_matches('\u{feff}').to_string();
    if key.is_empty() {
        return Err("no key entered".into());
    }
    let mut cfg = Config::load()?;
    cfg.set_path(&["providers", p.name(), "api_key"], Some(key));
    let first = cfg.get("provider").is_none();
    if first {
        cfg.set_path(&["provider"], Some(p.name().into()));
    }
    cfg.save()?;
    eprintln!("{} {} key to {}", ui::green("saved"), p.name(), cfg.path_display());
    if first {
        eprintln!("{}", ui::dim(&format!("{} is now the default provider", p.name())));
    }
    Ok(ExitCode::SUCCESS)
}

/// `jin --use <provider> [model]`
pub fn use_provider(provider: &str, model: Option<&str>) -> Result<ExitCode, String> {
    let p = Provider::parse(provider)?;
    let mut cfg = Config::load()?;
    cfg.set_path(&["provider"], Some(p.name().into()));
    if let Some(m) = model {
        cfg.set_path(&["providers", p.name(), "model"], Some(m.trim().into()));
    }
    cfg.save()?;
    let active = cfg.provider_str(p, "model").unwrap_or(p.defaults(false).0);
    eprintln!("{} {} / {active}", ui::green("using"), p.name());
    if cfg.api_key(p).is_none() {
        eprintln!("{}", ui::dim(&format!("no API key yet: run `jin --key {}`", p.name())));
    }
    Ok(ExitCode::SUCCESS)
}

/// `jin --set <name> [value]`; no value removes the setting.
pub fn set(name: &str, value: Option<&str>) -> Result<ExitCode, String> {
    let path: Vec<&str> = match name.split_once('.') {
        Some((prov, field)) => {
            let p = Provider::parse(prov)?;
            if !FIELDS.contains(&field) {
                return Err(format!("unknown setting '{field}' (use {})", FIELDS.join(", ")));
            }
            vec!["providers", p.name(), field]
        }
        None if matches!(name, "provider" | "shell" | "auto_run" | "descriptions" | "spinner") => vec![name],
        None => return Err(format!("unknown setting '{name}' (see jin --help)")),
    };
    let mut value = value.map(|v| v.trim().to_string());
    if name == "provider"
        && let Some(v) = &value
    {
        value = Some(Provider::parse(v)?.name().to_string());
    }
    if let Some(v) = &value {
        let allowed: Option<Vec<&str>> = match name {
            "auto_run" => Some(vec!["safe", "never"]),
            "descriptions" => Some(vec!["on", "off"]),
            "spinner" => Some(ui::spinner_names()),
            _ => None,
        };
        if let Some(allowed) = allowed
            && !allowed.contains(&v.as_str())
        {
            return Err(format!("{name} must be one of: {}", allowed.join(", ")));
        }
    }
    // `effort off` stores "", which means "send no effort setting".
    if name.ends_with("effort") && value.as_deref() == Some("off") {
        value = Some(String::new());
    }

    let mut cfg = Config::load()?;
    let shown = match &value {
        Some(v) if name.ends_with("api_key") => mask(v),
        Some(v) if v.is_empty() => "off".into(),
        Some(v) => v.clone(),
        None => "default".into(),
    };
    cfg.set_path(&path, value);
    cfg.save()?;
    eprintln!("{} {name} = {shown}", ui::green("set"));
    Ok(ExitCode::SUCCESS)
}

/// `jin --config`
pub fn show() -> Result<ExitCode, String> {
    let cfg = Config::load()?;
    let exists = cfg.path.as_ref().is_some_and(|p| p.is_file());
    println!("config: {}{}", cfg.path_display(), if exists { "" } else { " (not created yet)" });
    match cfg.resolve(None, None, false) {
        Ok(llm) => println!("active: {} / {}", llm.provider.name(), llm.model),
        Err(e) => println!("active: none ({e})"),
    }
    println!(
        "settings: auto_run={}  descriptions={}  spinner={}  shell={}",
        cfg.get("auto_run").unwrap_or("safe"),
        cfg.get("descriptions").unwrap_or("off"),
        cfg.get("spinner").unwrap_or("dots"),
        cfg.get("shell").unwrap_or("auto"),
    );
    println!();
    for p in Provider::ALL {
        let model = cfg.provider_str(p, "model").unwrap_or(p.defaults(false).0);
        let smart = cfg.provider_str(p, "smart_model").unwrap_or(p.defaults(true).0);
        let key = match (env(p.key_env()), cfg.provider_str(p, "api_key")) {
            (Some(_), _) => format!("key from {}", p.key_env()),
            (None, Some(k)) => format!("key {}", mask(k)),
            (None, None) => "no key".into(),
        };
        println!("{:<11} model={model}  smart={smart}  ({key})", p.name());
    }
    if !exists {
        println!("\nGet started:");
        println!("  jin --key openrouter            save a key (typed hidden)");
        println!("  jin --use openrouter <model>    pick provider and model");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: Value) -> Config {
        Config { root, path: None }
    }

    #[test]
    fn set_path_creates_and_removes() {
        let mut c = cfg(json!({}));
        c.set_path(&["providers", "openai", "model"], Some("m".into()));
        assert_eq!(c.field(Provider::OpenAI, "model"), Some("m"));
        c.set_path(&["providers", "openai", "model"], None);
        assert_eq!(c.field(Provider::OpenAI, "model"), None);
    }

    #[test]
    fn resolve_uses_stored_provider_model_and_effort() {
        let c = cfg(json!({
            "provider": "openrouter",
            "providers": { "openrouter": { "api_key": "k", "model": "x/y", "effort": "" } }
        }));
        // SAFETY: tests in this module don't read these variables concurrently.
        unsafe {
            std::env::remove_var("JIN_PROVIDER");
            std::env::remove_var("JIN_MODEL");
        }
        let llm = c.resolve(None, None, false).unwrap();
        assert_eq!(llm.provider, Provider::OpenRouter);
        assert_eq!(llm.model, "x/y");
        assert_eq!(llm.effort, None);
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask("sk-or-v1-abcdef123456"), "sk-or-...3456");
        assert_eq!(mask("short"), "****");
    }
}
