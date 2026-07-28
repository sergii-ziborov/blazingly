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
| HTTP/2 | out of the release contour | Off by default, behind `native-http2`, excluded from every release gate. See below |
| Deployment, CLI, ecosystem adapters | experimental | No compatibility guarantee until promoted |

## HTTP/2 is deliberately outside the release contour

HTTP/2 is not a blocker for any Blazingly release, and its status does not
propagate to the rest of the framework. The reasoning is worth recording,
because it looks like a gap and is not.

Every Rust framework that advertises HTTP/2 gets it from the same crate:
hyperium's `h2`. Axum reaches it through Hyper, Actix Web through
`actix-http`, tonic and reqwest depend on it directly. `h2` needs no Tokio
runtime — its manifest asks only for Tokio's `io-util` traits — but it does put
`tokio` and `tokio-util` in the dependency graph. `deny.toml` forbids exactly
that, on purpose: a framework whose position is "no Tokio, no Hyper, no Axum"
cannot hold that position while vendoring one of them for trait definitions.

The current `shiguredo_http2` adapter stays available behind the
`native-http2` feature. It is pinned to a canary release from an upstream whose
README states that its specification changes actively and that it accepts
neither issues nor pull requests without prior discussion. That is a reasonable
basis for an experiment and an unreasonable one for a supported surface, so it
is treated as the former.

A supported HTTP/2 will live in a separate `blazingly-http2` repository, built
the same way `blazingly-contract` and `blazingly-wire` are. Both plausible
starting points are permissively licensed: `h2` is MIT and
`shiguredo_http2` is Apache-2.0, so either can be forked outright with its
copyright notice preserved. Forking with attribution is both cheaper and safer
than reimplementing from a reading of the source, which produces derivative-work
exposure without the licence that would have covered it.

Until that repository exists, no release gate mentions HTTP/2 and no
documentation claims it as a supported transport.

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

1. Confirm both submodules, `crates/blazingly-contract` and
   `crates/blazingly-wire`, are checked out at a released tag whose `version`
   satisfies the matching requirement in the root `[workspace.dependencies]`.
   Commit both submodule pointers. A fresh `git clone --recursive` must build
   without any further steps. Each submodule releases from its own repository
   and is published to crates.io before the framework crates that depend on it.
2. Move the `Unreleased` section of `CHANGELOG.md` into a dated version
   section, and record every intentional breaking change reported by
   `workspace-semver`.
3. Confirm `repository` in `[workspace.package]` is the canonical hosting URL.
   crates.io records it permanently.
4. Bump `version` in `[workspace.package]`.
5. Confirm the release gate above is green.
6. Flip `publish` in `[workspace.package]` to `true`.
7. Publish in dependency order: the submodule crates `blazingly-contract` and
   `blazingly-wire` first, from their own repositories, then the leaf crates,
   then `blazingly` and `cargo-blazingly`.
8. Tag the release.

The first `1.0.0` requires a stable facade, documented support window,
production HTTP/1 hardening, independent security review, and at least two real
applications exercising the public API. HTTP/2 is not on that list.
