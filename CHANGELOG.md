# Changelog

## [0.3.3](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.3.2...cli-engine-v0.3.3) (2026-06-25)


### Features

* add shell completion built-in (generate + install) ([#30](https://github.com/godaddy/cli-engine/issues/30)) ([021a45e](https://github.com/godaddy/cli-engine/commit/021a45e714237c6794038670dad79bd7e38952ce))

## [0.3.2](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.3.1...cli-engine-v0.3.2) (2026-06-24)


### Features

* global --debug HTTP request/response logging ([#29](https://github.com/godaddy/cli-engine/issues/29)) ([7f4dbb2](https://github.com/godaddy/cli-engine/commit/7f4dbb2098d65da9dec73b0b190d544786498c99))

## [0.3.1](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.3.0...cli-engine-v0.3.1) (2026-06-17)


### Features

* first-class environments, per-env OAuth, consistent User-Agent, token timeout ([1f3ace2](https://github.com/godaddy/cli-engine/commit/1f3ace26a3f242aab90ad9e7ada2289a40857ec9))

## [0.3.0](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.2.2...cli-engine-v0.3.0) (2026-06-16)


### ⚠ BREAKING CHANGES

* human views are no longer inferred from the command path or system — assign them with CommandSpec::with_view or with_view_id or they will not apply. Human output now honors default_fields and --fields narrows a registered view's columns, so human tables that previously showed every column now show only the selected set. New public fields were added to CommandSpec (view_columns, view_id) and MiddlewareRequest (view_id).

### Features

* explicit human views, composable field/column selection, --schema short-circuit ([f5e2b72](https://github.com/godaddy/cli-engine/commit/f5e2b72bd417267025e2d6c1d2e4f57e7cf428c1))


### Bug Fixes

* Show pkce browser login prompt ([#24](https://github.com/godaddy/cli-engine/issues/24)) ([de65d35](https://github.com/godaddy/cli-engine/commit/de65d35d028ab3d284c12572040878c6e333f916))
* use active env for auth login ([#26](https://github.com/godaddy/cli-engine/issues/26)) ([ba1711e](https://github.com/godaddy/cli-engine/commit/ba1711ef7e8433491e33cd97daa3d36d163a45e9))

## [0.2.2](https://github.com/godaddy/cli-engine/compare/cli-engine-v0.2.1...cli-engine-v0.2.2) (2026-06-12)


### Features

* injectable credential storage + per-CLI config file ([#21](https://github.com/godaddy/cli-engine/issues/21)) ([3c20bf7](https://github.com/godaddy/cli-engine/commit/3c20bf72f99e4b2919addaa6bb7c229f31c4c011))

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
