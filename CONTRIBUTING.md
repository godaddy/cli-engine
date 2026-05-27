# Contributing

## Commit Messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/). The release tooling reads commit messages to determine version bumps and generate the changelog, so the format matters.

```
<type>(<optional scope>): <description>

<optional body>

<optional footer>
```

Common types:

| Type       | Changelog section          | Version effect (pre-1.0)  |
|------------|----------------------------|---------------------------|
| `feat`     | Features                   | minor bump (0.x → 0.x+1)  |
| `fix`      | Bug Fixes                  | patch bump                |
| `perf`     | Performance Improvements   | patch bump                |
| `docs`     | Documentation              | patch bump                |
| `refactor` | Code Refactoring           | patch bump                |
| `chore`    | Miscellaneous              | patch bump                |
| `build`    | Build System               | patch bump                |
| `test`     | Tests                      | patch bump                |
| `ci`       | *(hidden)*                 | no bump                   |

### Breaking changes

Append `!` to the type, or add a `BREAKING CHANGE:` footer, to signal a breaking change. This
produces a major version bump once the project reaches 1.0; before 1.0 it produces a minor bump.

```
feat!: remove deprecated --output flag

BREAKING CHANGE: the --output flag was removed; use --format instead
```

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
cargo test --all-targets
cargo test --doc
```

## Release Process

Releases are automated via [release-please](https://github.com/googleapis/release-please).

### How it works

1. Every merge to `main` runs the Release Please GitHub Action.
2. The action inspects commits since the last release and opens (or updates) a **Release PR** that:
   - bumps the version in `Cargo.toml`
   - updates `CHANGELOG.md`
3. When a maintainer is ready to ship, they **merge the Release PR**.
4. Merging the Release PR triggers the `publish` job, which runs `cargo publish` to crates.io.

No manual tagging or version editing is needed — everything flows from commit messages.

### Pre-1.0 versioning

Until the crate reaches `1.0.0`, the bump rules are conservative:

- A `feat` commit bumps the **patch** version (not minor).
- A breaking change bumps the **minor** version (not major).

This matches the project's current stability expectations. The rules will switch to standard semver
once `1.0.0` is tagged.

### Retrying a failed publish

If the release tag was created but `cargo publish` failed (network blip, crates.io outage, etc.),
trigger the workflow manually without creating a duplicate release:

1. Go to **Actions → Release → Run workflow**.
2. Check **Skip release-please, run publish directly**.
3. Click **Run workflow**.
