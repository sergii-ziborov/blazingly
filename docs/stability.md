# Stability and SemVer

Blazingly follows Semantic Versioning, but the framework is currently
pre-1.0. The `0.y.z` series is suitable for evaluation and controlled
production trials, not a promise that every public Rust API is frozen.

## Compatibility tiers

| Surface | Current tier | Compatibility rule |
| --- | --- | --- |
| `blazingly-contract` canonical format | versioned | Existing format readers and explicit compatibility reports are preserved |
| Typed operation contracts | versioned | Breaking changes require a new minor `0.y` release before 1.0 |
| Framework Rust API | experimental | Breaking changes require a new minor `0.y` release and migration notes |
| Native HTTP/1 wire behavior | preview | Protocol fixes may change internals; accepted HTTP semantics remain tested |
| HTTP/2, deployment, CLI, ecosystem adapters | experimental | No compatibility guarantee until promoted |

Patch releases must remain source-compatible within one `0.y` line. A minor
release may break a pre-1.0 Rust API, but the change must be documented.
Contract fingerprints never silently change: a canonical-format change also
increments the format version.

## Release gate

Each item below names the CI job that proves it. A release candidate must pass
all of them.

| # | Requirement | Job |
| --- | --- | --- |
| 1 | Formatting, all-feature Clippy, and tests on Linux, Windows, and macOS | `test` (`ci.yml`) |
| 2 | Meaningful feature subsets build, not only `--all-features` and `--no-default-features` | `feature-combinations` (`ci.yml`) |
| 3 | Documentation builds with no warnings | `rustdoc` (`ci.yml`) |
| 4 | Dependency advisory scanning | `security-audit` (`quality.yml`) |
| 5 | Miri over the portable, syscall-free crates | `miri` (`quality.yml`) |
| 6 | AddressSanitizer over the socket-facing crates, including `blazingly-native` | `address-sanitizer` (`quality.yml`) |
| 7 | Bounded fuzz smoke tests, with longer runs before important releases | `fuzz-smoke` (`quality.yml`) |
| 8 | `cargo-semver-checks` against the previous contract revision | `contract-semver` (`quality.yml`) |
| 9 | Workspace API changes reviewed | `workspace-semver` (`quality.yml`, advisory pre-1.0) |

Two requirements have no job and stay manual: HTTP/MCP conformance plus
external benchmark runs, and migration notes for every intentional breaking
change.

`workspace-semver` is advisory rather than blocking because a pre-1.0 minor
release may break the framework Rust API. It must become blocking before
`1.0.0`.

## Release process

1. Confirm `crates/blazingly-contract` is checked out at a released tag, and
   that its `version` satisfies the `blazingly-contract` requirement in the
   root `[workspace.dependencies]`. Commit the submodule pointer. A fresh
   `git clone --recursive` must build without any further steps.
2. Move the `Unreleased` section of `CHANGELOG.md` into a dated version
   section, and record every intentional breaking change reported by
   `workspace-semver`.
3. Confirm `repository` in `[workspace.package]` is the canonical hosting URL.
   crates.io records it permanently.
4. Bump `version` in `[workspace.package]`.
5. Confirm the release gate above is green.
6. Flip `publish` in `[workspace.package]` to `true`. `blazingly-wire-standalone`
   keeps `publish = false`; it is a test harness binary.
7. Publish in dependency order, contract first, then the leaf crates, then
   `blazingly` and `cargo-blazingly`.
8. Tag the release.

The first `1.0.0` requires a stable facade, documented support window,
production HTTP/1 hardening, a non-canary HTTP/2 dependency, independent
security review, and at least two real applications exercising the public API.
