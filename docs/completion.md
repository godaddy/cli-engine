# Shell Completion

`cli-engine` provides a built-in `completion` command that enables tab-completion for your CLI. 
This completion is automatically generated from your CLI's command tree using `clap_complete`, ensuring it stays in sync with your commands, flags, and arguments.

## Usage

The `completion` command is a reserved built-in, similar to `help`, `tree`, and `guide`. 
It cannot be overridden by consumer-defined commands.

### Generate Completion Script

To print the completion script for a specific shell to stdout:

```bash
<bin> completion [bash|zsh|fish|elvish|powershell]
```

### Install Completion

To automatically install the completion script and configure your shell, use the `--install` flag:

```bash
<bin> completion --install [bash|zsh|fish|elvish|powershell]
```

This command is **idempotent**. Re-running it replaces any existing managed completion block in your shell configuration. There is no `--uninstall` flag; completion scripts can be removed by deleting the managed block from your shell configuration file.

## Shell Install Locations

The completion command manages the installation by appending a block to your shell configuration file. This block is wrapped in managed markers so you can identify and edit it if needed:

`# >>> <bin> completion (managed) >>>`
`# <<< <bin> completion (managed) <<<`

| Shell | Script Location | Shell Configuration File |
| --- | --- | --- |
| **bash** | `$XDG_DATA_HOME/bash-completion/completions/<bin>` | `~/.bashrc` |
| **zsh** | `~/.zfunc/_<bin>` | `~/.zshrc` |
| **fish** | `$XDG_CONFIG_HOME/fish/completions/<bin>.fish` | None (auto-loaded) |
| **elvish** | `$XDG_CONFIG_HOME/elvish/lib/<bin>-completion.elv` | `$XDG_CONFIG_HOME/elvish/rc.elv` |
| **powershell** | `~/Documents/PowerShell/<bin>-completion.ps1` | `$PROFILE` |

### Notes

- **bash**: Ensures `bash-completion` is sourced in your shell profile.
- **zsh**: Adds the script directory to your `fpath` and calls `autoload -U compinit; compinit`.
- **fish**: Files placed in the completions directory are auto-loaded by fish; no shell configuration edit is required.
- **powershell**: Adds the dot-source command to your PowerShell profile.
