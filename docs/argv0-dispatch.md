# argv0 Dispatch (Multi-Call Binaries)

`cli_engine` can behave differently depending on the name it was invoked as, in the style of
`busybox` or `git`'s built-ins. A single binary, symlinked (or shimmed) under several names,
dispatches to different behavior based on `argv[0]`.

This is entirely opt-in. An application that registers no alternative names behaves exactly as a
binary that never knew about the feature, so adding it breaks nothing.

## Two Kinds Of Route

Register alternative names on [`CliConfig`](../src/cli.rs). Each name maps to one of two behaviors:

### Alias — a shortcut to a command path

[`CliConfig::with_argv0_alias`](../src/cli.rs) rewrites the invocation into canonical subcommand
tokens and runs it through the normal command tree, with the real argument tail appended.

```rust
use cli_engine::CliConfig;

let config = CliConfig::new("my-cli", "Team CLI", "my-cli")
    // Invoked as `pl`, behave like `project list`.
    .with_argv0_alias("pl", ["project", "list"]);
```

Invoking the binary as `pl --team platform` is then identical to `my-cli project list --team
platform`: same parsing, flags, auth, middleware, and output.

### Personality — a different application

[`CliConfig::with_argv0_personality`](../src/cli.rs) runs an entirely separate CLI built from its own
[`CliConfig`] — its own root name, commands, flags, and auth. The closure runs lazily, only when the
route is actually dispatched, so unused personalities cost nothing.

```rust
use cli_engine::CliConfig;

let config = CliConfig::new("my-cli", "Team CLI", "my-cli")
    .with_argv0_personality("legacy-tool", || {
        CliConfig::new("legacy-tool", "Legacy compatibility shim", "legacy-tool")
        // ...modules, commands, auth for the legacy personality...
    });
```

The personality presents the name from its own `CliConfig` in help and usage output.

## Fall-Through (No Surprises)

The engine never needs to know its own canonical name. It only matches `argv[0]` against the
**registered** alternative names. Anything else — the real binary name, or the binary renamed to
something unregistered — falls through to the default CLI unchanged. Renaming `my-cli` to
`otherthing` (not a registered route) just runs the normal `my-cli` application.

## The Hidden `argv0` Command

`cli_engine` also accepts an undocumented `argv0` command that forces a route without a symlink:

```sh
my-cli argv0 pl --team platform     # dispatch as if invoked as `pl`
my-cli argv0 legacy-tool ...        # dispatch the `legacy-tool` personality
```

It is recognized as the first argument after the program name, is never registered with `clap`, and
so never appears in `--help`, `tree`, or `--search`. It is active only when the application has
registered at least one route.

Unlike the silent symlink fall-through, an **explicit** `argv0` invocation is strict: an unknown
name, or a bare `argv0` with no name, exits non-zero with an error listing the known names. This
keeps mistakes visible while leaving foreign scripts that merely call your binary untouched (nothing
triggers dispatch unless the literal `argv0` token is present).

## Installing The Alternative Names

### Unix — symlinks (or hardlinks/copies)

```sh
ln -s my-cli /usr/local/bin/pl
ln -s my-cli /usr/local/bin/legacy-tool
```

`argv[0]` is the link name, so the engine matches its basename (path and any extension stripped,
e.g. `.exe` on Windows) against the registered routes.

### Windows — symlinks and hard links

Native links work the same as on Unix and need no shim. A soft symlink
(`mklink pl.exe my-cli.exe`) or hard link (`mklink /H pl.exe my-cli.exe`) sets `argv[0]` to the link
name, not the resolved target, so the engine matches it directly. The `.exe` extension is stripped,
so a link named `pl.exe` matches a route registered as `pl`:

```bat
mklink pl.exe my-cli.exe            REM soft symlink (developer mode or elevated)
mklink /H legacy-tool.exe my-cli.exe REM hard link (no elevation needed)
```

### Windows — `.cmd` shims

When links are inconvenient (e.g. no developer mode, or distributing across volumes where hard links
can't reach), a tiny `.cmd` shim beside the binary is the lightweight alternative. Because a launched
process cannot read the name of the `.cmd` that started it, the shim forwards its own filename to the
explicit `argv0` command using the batch parameter `%~n0` (the script's base name):

```bat
REM  pl.cmd  — the FILENAME is the alias; the body is generic boilerplate
@"%~dp0my-cli.exe" argv0 %~n0 %*
```

- `%~dp0` — the directory of the shim, so the sibling `my-cli.exe` is found.
- `%~n0` — the shim's own base name (`pl`), supplied as the explicit `argv0` name.
- `%*` — the caller's arguments, forwarded unchanged.

The same one-line body works for every alias; only the file's name differs (`pl.cmd`,
`legacy-tool.cmd`, ...). Dispatch stays explicit, so the shim never triggers in unrelated scripts.

Nesting works correctly: if `myscript.cmd` calls `pl`, the dispatched name is `pl`, not `myscript` —
`%~n0` is evaluated inside `pl.cmd` and always resolves to `pl` regardless of the caller.

You always register the bare name (`with_argv0_alias("pl", ...)`). The name resolved from a link or
shim has its extension stripped, so the same registration matches `pl` (Unix link), `pl.exe`
(Windows link), and `pl.cmd` (shim) alike — whether the shim forwards `%~n0` (already extension-less)
or `%~nx0`/`%0` (with the `.cmd` suffix).

## Creating Links Programmatically

Rather than authoring links by hand, installers and self-healing code can create them with
[`Cli::create_link`](../src/cli.rs). It takes the registered name, a directory, an optional target
executable (`None` uses the current executable), and an [`Argv0LinkMethod`](../src/cli.rs):

| Method | Unix | Windows |
| --- | --- | --- |
| `SoftLink` | symbolic link `<name>` | symbolic link `<name>.exe` (Developer Mode/elevation) |
| `HardLink` | hard link `<name>` | hard link `<name>.exe` (same volume) |
| `Script` | executable `<name>` shell script | `<name>.cmd` batch shim |

```rust
use cli_engine::Argv0LinkMethod;

// Install (or restore) every registered alternative name into the bin directory,
// pointing at the running binary. Pick the method the platform/installer prefers.
let method = if cfg!(windows) {
    Argv0LinkMethod::Script
} else {
    Argv0LinkMethod::SoftLink
};
for name in cli.argv0_names() {
    cli.create_link(name, &bin_dir, None, method)?;
}
```

`create_link` ensures the desired state idempotently: a destination that already matches is left
untouched, while a missing, wrong-target, or corrupted one is created or replaced — so re-running it
restores both deleted and broken links. Registered names must be simple `[A-Za-z0-9_-]` tokens that
differ from the CLI's own name.
