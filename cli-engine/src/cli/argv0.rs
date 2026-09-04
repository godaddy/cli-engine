//! Busybox/git-style `argv[0]` multi-call dispatch: route types, resolution,
//! and on-disk link/shim management.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{Cli, CliRunOutput, config::CliConfig};
use crate::CliCoreError;

/// Maximum number of chained `argv0` dispatch hand-offs before the engine
/// refuses to recurse further. Real multi-call nesting is zero or one level;
/// this bounds a pathologically long explicit `argv0 … argv0 …` chain so it
/// errors cleanly instead of overflowing the stack.
pub(super) const MAX_ARGV0_DEPTH: usize = 16;

/// How the engine behaves when invoked under a registered alternative `argv[0]`
/// name (busybox/git-style multi-call dispatch).
///
/// A route is selected when the binary's `argv[0]` basename — or the name given
/// to the hidden `argv0` command — matches a key registered via
/// [`CliConfig::with_argv0_alias`] or [`CliConfig::with_argv0_personality`]. An
/// `argv[0]` that matches no route falls through to the default CLI, so existing
/// applications that register no routes are unaffected.
///
/// Non-exhaustive: more route kinds may be added in future releases. Register
/// routes through the [`CliConfig`] builders rather than matching on variants.
#[derive(Clone)]
#[non_exhaustive]
pub enum Argv0Route {
    /// Rewrite the invocation into these canonical subcommand tokens and run it
    /// through the normal command tree, with the real argument tail appended.
    ///
    /// For example, an `Alias(vec!["project".into(), "list".into()])` registered
    /// under `pl` makes `pl --team x` behave exactly like `project list --team x`.
    Alias(Vec<String>),
    /// Run an entirely separate CLI application built from the returned
    /// [`CliConfig`] (its own root name, commands, flags, and auth). The
    /// configuration is built lazily, only when the route is actually dispatched,
    /// so registering a personality costs nothing for invocations that never hit it.
    Personality(Arc<dyn Fn() -> CliConfig + Send + Sync>),
}

impl std::fmt::Debug for Argv0Route {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alias(tokens) => formatter.debug_tuple("Alias").field(tokens).finish(),
            Self::Personality(_) => formatter.write_str("Personality(..)"),
        }
    }
}

/// On-disk mechanism used by [`Cli::create_link`] to materialize an alternative
/// `argv[0]` name so the binary can be invoked under it.
///
/// Installers pick the mechanism that suits the platform and environment;
/// self-healing code can re-run [`Cli::create_link`] to restore a deleted link.
///
/// Non-exhaustive: more link mechanisms may be added in future releases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Argv0LinkMethod {
    /// A symbolic link to the target executable (`<name>` on Unix, `<name>.exe`
    /// on Windows). On Windows this may require Developer Mode or elevation.
    SoftLink,
    /// A hard link to the target executable (`<name>` on Unix, `<name>.exe` on
    /// Windows). The link must live on the same volume as the target.
    HardLink,
    /// A small shim script that forwards to the target via the `argv0` command:
    /// a `<name>.cmd` batch file on Windows, or an executable `<name>` shell
    /// script on Unix. Useful when links are unavailable or inconvenient.
    Script,
}

/// Outcome of [`Cli::resolve_argv0`]: either rewritten arguments to feed the
/// normal pipeline, or a fully rendered result to return immediately.
pub(super) enum Argv0Outcome {
    /// Continue the normal run pipeline with these arguments.
    Proceed(Vec<String>),
    /// Return this already-rendered result without further processing.
    Handled(CliRunOutput),
}

/// Resolves busybox/git-style `argv[0]` dispatch before the normal pipeline.
///
/// Returns [`Argv0Outcome::Proceed`] with the (possibly rewritten) argument
/// vector to feed the normal command pipeline, or [`Argv0Outcome::Handled`]
/// with a fully rendered result when a personality ran or an explicit `argv0`
/// invocation was rejected. When no routes are registered this is inert and
/// returns the arguments unchanged. `depth` counts chained hand-offs and
/// bounds recursion via [`MAX_ARGV0_DEPTH`].
pub(super) async fn resolve_argv0(cli: &Cli, text_args: Vec<String>, depth: usize) -> Argv0Outcome {
    if cli.config.argv0_routes.is_empty() {
        return Argv0Outcome::Proceed(text_args);
    }

    if depth > MAX_ARGV0_DEPTH {
        return Argv0Outcome::Handled(render_argv0_error(
            cli,
            &text_args,
            "argv0 dispatch recursion limit exceeded",
        ));
    }

    // The hidden `argv0` meta-command (`<bin> argv0 <name> [args...]`) forces
    // a route without an actual symlink. It is recognized positionally as the
    // first argument after the program name and is never registered with clap,
    // so it stays absent from `--help`, `tree`, and the `search` command.
    let explicit = text_args.get(1).map(String::as_str) == Some("argv0");
    let (name, rest) = if explicit {
        match text_args.get(2) {
            None => {
                return Argv0Outcome::Handled(render_argv0_error(
                    cli,
                    &text_args,
                    "the argv0 command requires a name to dispatch as",
                ));
            }
            // Normalize the explicit name the same way as a symlink basename
            // so a route registered as `whatever` matches whether the caller
            // passed `whatever`, `whatever.exe`, or a `.cmd` shim's `whatever.cmd`.
            Some(name) => (
                program_basename(name),
                text_args
                    .get(3..)
                    .map(<[String]>::to_vec)
                    .unwrap_or_default(),
            ),
        }
    } else {
        let name = text_args
            .first()
            .map(|arg| program_basename(arg))
            .unwrap_or_default();
        let rest = text_args
            .get(1..)
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        (name, rest)
    };

    match cli.config.argv0_routes.get(&name) {
        Some(Argv0Route::Alias(tokens)) => {
            // Rewrite as `<canonical-name> <tokens...> <rest...>`. Element 0 is
            // the canonical name so the downstream program-name skip applies.
            let mut rewritten = Vec::with_capacity(1 + tokens.len() + rest.len());
            rewritten.push(cli.config.name.clone());
            rewritten.extend(tokens.iter().cloned());
            rewritten.extend(rest);
            Argv0Outcome::Proceed(rewritten)
        }
        Some(Argv0Route::Personality(build)) => {
            // Hand off to an independent CLI built lazily from the route. Its
            // own config name leads so its help/usage and program-name skip
            // render correctly. `Box::pin` breaks the recursive `async fn`;
            // `depth + 1` bounds a pathological chain of hand-offs.
            let config = build();
            let bin = config.name.clone();
            let alt = Cli::new(config);
            let mut alt_args = Vec::with_capacity(1 + rest.len());
            alt_args.push(bin);
            alt_args.extend(rest);
            Argv0Outcome::Handled(
                Box::pin(super::run::run_with_depth(&alt, alt_args, depth + 1)).await,
            )
        }
        None if explicit => Argv0Outcome::Handled(render_argv0_error(
            cli,
            &text_args,
            format!(
                "{name:?} is not a registered argv0 name; known names: {}",
                known_argv0_names(cli)
            ),
        )),
        None => {
            // Unregistered name (e.g. the binary renamed to something we do not
            // recognize): fall through to the default CLI. Normalizing element 0
            // to the canonical name lets a renamed binary parse as the default
            // application instead of treating its name as a command token.
            let mut rewritten = Vec::with_capacity(1 + rest.len());
            rewritten.push(cli.config.name.clone());
            rewritten.extend(rest);
            Argv0Outcome::Proceed(rewritten)
        }
    }
}

/// Comma-separated, sorted list of registered alternative `argv[0]` names,
/// used in the error shown for an unknown explicit `argv0` invocation.
pub(super) fn known_argv0_names(cli: &Cli) -> String {
    cli.config
        .argv0_routes
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders an `argv0`-dispatch error through the engine's structured error
/// envelope so it honors `--output` (parsed from the raw args, since dispatch
/// runs before clap) and the shared exit-code mapping, matching every other
/// CLI error rather than emitting bare text.
pub(super) fn render_argv0_error(
    cli: &Cli,
    text_args: &[String],
    message: impl Into<String>,
) -> CliRunOutput {
    let mut middleware = cli.middleware.clone();
    middleware.output_format =
        crate::flags::extract_output_format(text_args, &super::run::resolve_run_output_format(cli));
    let err = CliCoreError::message(message);
    super::run::finish_run(
        cli,
        super::render::render_cli_error(&middleware, &err, &cli.config.app_id),
    )
}

/// Creates an on-disk link in `dir` that lets the binary be invoked under the
/// registered alternative `argv[0]` name `name`, using `method`. Backs
/// [`Cli::create_link`]; see that method's rustdoc for the full contract.
pub(super) fn create_link(
    cli: &Cli,
    name: &str,
    dir: impl AsRef<Path>,
    target: Option<&Path>,
    method: Argv0LinkMethod,
) -> std::io::Result<PathBuf> {
    if !cli.config.argv0_routes.contains_key(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name:?} is not a registered argv0 name"),
        ));
    }

    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    let link = dir.join(argv0_link_file_name(name, method));

    // Resolve the target up front so an existing entry can be compared against it.
    let resolved_target;
    let target = match target {
        Some(target) => target,
        None => {
            resolved_target = std::env::current_exe()?;
            resolved_target.as_path()
        }
    };

    // Ensure-desired-state. `symlink_metadata` does not follow links, so a
    // present-but-dangling link still counts as existing. A matching entry is
    // left untouched (idempotent); a differing one is removed and recreated.
    if std::fs::symlink_metadata(&link).is_ok() {
        if argv0_link_matches(&link, target, name, method)? {
            return Ok(link);
        }
        std::fs::remove_file(&link)?;
    }

    match method {
        Argv0LinkMethod::SoftLink => create_symlink(target, &link)?,
        Argv0LinkMethod::HardLink => std::fs::hard_link(target, &link)?,
        Argv0LinkMethod::Script => {
            std::fs::write(&link, argv0_script_contents(target, name))?;
            make_executable(&link)?;
        }
    }
    Ok(link)
}

/// Extracts the bare program name from an `argv[0]` value, dropping any directory
/// path and file extension (e.g. `/usr/bin/pl` or `pl.exe` both yield `pl`).
/// Falls back to the raw value when no file stem can be derived.
fn program_basename(arg: &str) -> String {
    Path::new(arg)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map_or_else(|| arg.to_owned(), ToOwned::to_owned)
}

/// Returns `true` when `name` is a valid alternative `argv[0]` route name: a
/// non-empty token of ASCII letters, digits, `-`, or `_`. This keeps the name
/// safe as a link/shim filename and as an `argv[0]` basename (which is matched
/// with its extension stripped, so an embedded dot would break matching).
pub(super) fn is_valid_argv0_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Returns `true` when the entry at `link` already matches what [`Cli::create_link`]
/// would produce for `method`/`target`/`name`, so it can be left untouched. A
/// mismatch (wrong kind, stale symlink target, or differing contents) returns
/// `false` so the caller replaces it.
fn argv0_link_matches(
    link: &Path,
    target: &Path,
    name: &str,
    method: Argv0LinkMethod,
) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(link)?;
    match method {
        Argv0LinkMethod::SoftLink => {
            Ok(metadata.file_type().is_symlink() && std::fs::read_link(link)? == target)
        }
        Argv0LinkMethod::HardLink => {
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            // A correct hard link is indistinguishable from the target by content;
            // comparing bytes also accepts an identical copy, which is harmless.
            Ok(std::fs::read(link)? == std::fs::read(target)?)
        }
        Argv0LinkMethod::Script => {
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            Ok(std::fs::read_to_string(link).ok() == Some(argv0_script_contents(target, name)))
        }
    }
}

/// File name for an alternative `argv[0]` link, per method and host platform.
fn argv0_link_file_name(name: &str, method: Argv0LinkMethod) -> String {
    let extension = match method {
        Argv0LinkMethod::Script if cfg!(windows) => ".cmd",
        // Unix scripts are extension-less executables; links carry `.exe` on Windows.
        Argv0LinkMethod::Script => "",
        _ if cfg!(windows) => ".exe",
        _ => "",
    };
    format!("{name}{extension}")
}

/// Contents of an alternative `argv[0]` shim script that forwards to `target`
/// via the explicit `argv0` command. A `.cmd` batch file on Windows, an
/// executable POSIX shell script elsewhere.
fn argv0_script_contents(target: &Path, name: &str) -> String {
    let target = target.display();
    if cfg!(windows) {
        format!("@\"{target}\" argv0 {name} %*\r\n")
    } else {
        format!("#!/bin/sh\nexec \"{target}\" argv0 {name} \"$@\"\n")
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlink creation is not supported on this platform",
    ))
}

/// Marks a freshly written shim script executable on Unix; a no-op elsewhere.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
