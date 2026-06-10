# Changelog

## [0.2.1](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.2.0...cli-engine-v0.2.1) (2026-06-10)


### Features

* argv0 multi-call dispatch with link/shim installer ([#19](https://github.com/godaddy/cli-engine/issues/19)) ([9e39f2f](https://github.com/godaddy/cli-engine/commit/9e39f2fa7aca9b60fa898e33eb9a2a92d93bf350))
* OAuth scope step-up via command metadata ([#18](https://github.com/godaddy/cli-engine/issues/18)) ([f996e50](https://github.com/godaddy/cli-engine/commit/f996e5074c417c798a289d4d87fc283f78672c45))

## [0.2.0](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.1.3...cli-engine-v0.2.0) (2026-06-09)


### ⚠ BREAKING CHANGES

* `CommandSpec.no_auth` (bool) is replaced by `CommandSpec.auth` (`AuthRequirement`), and `MiddlewareRequest.no_auth` by `MiddlewareRequest.auth`. `CommandContext.credential` is now a `CredentialResolver` instead of `Option<Credential>`; `RuntimeCommandSpec::new` and `new_typed` handler closures receive a `CredentialResolver`; and `Authorizer::authorize` receives `&CredentialResolver` instead of `Option<&Credential>`. The `no_auth(true)` builder still works and maps to `AuthRequirement::None`; `auth_optional()` and `auth(AuthRequirement)` select the other policies.

### Features

* fail-closed authentication via AuthRequirement; populate PKCE identity ([#17](https://github.com/godaddy/cli-engine/issues/17)) ([34313bf](https://github.com/godaddy/cli-engine/commit/34313bf28b63270a151cd19de5d1f3b4665177e5))


### Bug Fixes

* render help for `<group> help` subcommand form ([#15](https://github.com/godaddy/cli-engine/issues/15)) ([c21db13](https://github.com/godaddy/cli-engine/commit/c21db1359a48d36caa0dd9f324cbc2a45ec84df7))

## [0.1.3](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.1.2...cli-engine-v0.1.3) (2026-06-05)


### Features

* agent-first root discovery, curated help, and TTY-aware output (DEVEX-695) ([#13](https://github.com/godaddy/cli-engine/issues/13)) ([791b335](https://github.com/godaddy/cli-engine/commit/791b335f8ec182ab8be4e2d29364fe27dc1aa8bf))

## [0.1.2](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.1.1...cli-engine-v0.1.2) (2026-06-01)


### Features

* Allow hard-coded redirect URL ([#9](https://github.com/godaddy/cli-engine/issues/9)) ([e24dc24](https://github.com/godaddy/cli-engine/commit/e24dc2476cdb415a1867912e6b4e8267d7ffc956))
* Fix keychain issues and add fs fallback ([#10](https://github.com/godaddy/cli-engine/issues/10)) ([853a98a](https://github.com/godaddy/cli-engine/commit/853a98ac2d0b2b0c763d8dc03ded90df36944185))


### Bug Fixes

* Allow Ctrl+C to work while waiting on OAuth flow ([#7](https://github.com/godaddy/cli-engine/issues/7)) ([2b6d10e](https://github.com/godaddy/cli-engine/commit/2b6d10e75725d10e8834d4c061b1c9446aa3b212))

## [0.1.1](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.1.0...cli-engine-v0.1.1) (2026-05-27)


### Features

* derive support for typed command arguments ([cc53319](https://github.com/godaddy/cli-engine/commit/cc53319179db572c6c3bcdd0f0952e9648459c39))
* formatting shorthand ([b4a4572](https://github.com/godaddy/cli-engine/commit/b4a457269b0b64309ebe3927939a25094e49add6))
* gdx/godaddy CLI feature support ([b2ab315](https://github.com/godaddy/cli-engine/commit/b2ab315316b9f4f57517658da3908571dd4f1c79))


### Bug Fixes

* remove default timeout ([d87440c](https://github.com/godaddy/cli-engine/commit/d87440c30505abcc66ef58d846dbca800a6cc8c1))
