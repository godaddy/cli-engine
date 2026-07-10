## Summary

* 

## Test plan

* `cargo fmt --all --check`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test --all-targets`

### Manual verification

<!-- Describe how a reviewer can manually verify the change.
     If the change requires building the CLI with a local path override, include
     setup/teardown steps so they can test with and without the fix. -->

**Setup:**

```bash
# In the cli repo, temporarily override the engine dependency:
cd cli/rust
# Edit Cargo.toml → cli-engine = { features = ["pkce-auth"], path = "../../cli-engine" }
cargo build --release && cp target/release/gddy ~/.local/bin/gddy
```

**Test WITHOUT the fix (baseline):**

```bash
cd cli-engine && git checkout main
cd cli/rust && cargo build --release && cp target/release/gddy ~/.local/bin/gddy
# <command to reproduce the issue>
# Expected: <describe broken behavior>
```

**Test WITH the fix:**

```bash
cd cli-engine && git checkout <this-branch>
cd cli/rust && cargo build --release && cp target/release/gddy ~/.local/bin/gddy
# <same command>
# Expected: <describe fixed behavior>
```

**Cleanup:**

```bash
# Revert cli/rust/Cargo.toml back to:
# cli-engine = { features = ["pkce-auth"], version = "<published-version>" }
```
