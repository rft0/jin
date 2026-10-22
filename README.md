# jin

One-shot AI help in the terminal: describe a command and jin writes it, describe a bigger task and jin writes a throwaway script for it, ask a question and jin answers it, or describe a change and jin edits a file. One request, one reply, you confirm, done. jin works out which one a request needs.

```
jin find files over 100MB in this folder
jin -s show which process is listening on port 3000
jin what does chmod 755 mean
jin add logging to backup.ps1, written next to the script
jin --undo
```

- **Commands:** read-only commands (`Get-Content`, `ls`, `git status`, ...) run right away. Anything that writes, installs, kills or changes settings is shown in yellow with `Run? [y]es / [c]opy / [N]o`. Destructive commands are shown in red and need you to type `yes`. A command counts as read-only only if both the model and jin's own check of its commands and PowerShell verbs agree. `jin --set auto_run never` makes every command ask.
- **Scripts:** tasks that need loops or parsing get a Python, Node, PowerShell or Bash script, whichever are installed. jin shows the whole script, asks, runs it from a temp file in your current folder, and deletes it, so nothing is left in your project. Scripts always ask, because jin can't verify that a script only reads.
- **Questions:** the model decides whether a request needs a command or an answer. Answers are printed straight to stdout. "How do I…" requests usually get a runnable command.
- **Edits:** any existing file named in the request is sent along with it (up to 3 files, 64 KB each). If the model wants to edit a file you didn't name, jin sends that file and asks once more. The model returns exact find-and-replace edits. jin shows a diff and writes only after you confirm. It keeps CRLF line endings and BOMs as they were. `jin --undo` reverts the last edit.
- **Piping:** `-n`/`--print`, or a non-terminal stdin, prints only the command to stdout. For edits, `-n` shows the diff without writing. Status messages and prompts go to stderr.

## Providers

| Provider | Key | Default model | `--smart` |
|---|---|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-haiku-5-5` (effort low) | `claude-sonnet-5-5` (effort medium) |
| `openai` | `OPENAI_API_KEY` | `gpt-5-nano` (reasoning minimal) | `gpt-5-mini` (reasoning low) |
| `openrouter` | `OPENROUTER_API_KEY` | `~anthropic/claude-haiku-latest` | `~anthropic/claude-sonnet-latest` |

The provider comes from, in order: `-p`, `JIN_PROVIDER`, the saved default, then the first provider that has an API key.

## Setup

Settings live in `~/.jin/config.json` (`%USERPROFILE%\.jin\config.json` on Windows) and are managed with jin itself:

```
jin --key openrouter                              # asks for the key, input hidden
jin --use openrouter anthropic/claude-haiku-4.5   # default provider + model
jin --set openrouter.smart_model openai/gpt-5-mini
jin --set openai.effort off                       # send no effort/reasoning setting
jin --set shell pwsh                              # shell that runs commands
jin --set auto_run never                          # ask before every command (default: safe)
jin --set descriptions on                         # one-line explanations under commands, scripts, diffs (default: off)
jin --set spinner circle                          # dots, line, circle, arc, bounce, star, pulse, bar, none (text only)
jin --spinners                                    # list spinner styles
jin --set openrouter.model                        # no value = back to the default
jin --config                                      # show settings, keys masked
```

Per-provider settings: `api_key`, `model`, `smart_model`, `effort`, `smart_effort`, `base_url`. The file is plain JSON, so you can edit it by hand too:

```json
{
  "provider": "openrouter",
  "shell": "pwsh",
  "providers": {
    "openrouter": { "api_key": "sk-or-...", "model": "anthropic/claude-haiku-4.5" }
  }
}
```

Environment variables override the file: `JIN_PROVIDER`, `JIN_MODEL`, `JIN_SHELL`, the provider key variables, and `JIN_HOME` (moves the `.jin` folder).

## Build

```
cargo build --release
```

The binary is `target/release/jin.exe` (about 2 MB, starts in under 10 ms). Copy it to a folder on your `PATH`.
