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

1. For each of the three submodules — `crates/blazingly-contract`,
   `crates/blazingly-json`, `crates/blazingly-wire` — check whether its code
   changed since its own last published version. **A changed submodule whose
   version was not bumped is the failure that hides best**: the release
   workflow skips any version already on crates.io, so the change is committed,
   pushed, and never reaches a single user. Bump it in its own repository, tag
   it, and push both before touching the parent.
2. Commit the three submodule gitlinks. Each must point at a pushed, tagged
   commit whose `version` satisfies the matching requirement in the root
   `[workspace.dependencies]`; the tagged CI checkout fetches those commits
   from the submodule remotes and cannot see anything that lives only locally.
   A fresh `git clone --recursive` must build with no further steps.
3. Bump `version` in `[workspace.package]`, **and every first-party
   requirement in `[workspace.dependencies]` that carries it**. Cargo reads
   `0.1.0` as `>=0.1.0, <0.2.0`, so a requirement left behind does not fail
   loudly: each crate resolves its siblings from the previous release instead
   of the one being published beside it.
4. Update every version pinned in prose that ships to crates.io. Each crate
   has its own `README.md` and each is uploaded with that crate's tarball, so
   an install snippet reading `= "0.1"` on the 0.2.0 page tells readers to
   depend on a version that cannot contain what they are reading about, and a
   published version's files can never be replaced. Sweep with
   `Select-String -Path "crates\*\README.md" -Pattern '= "0\.\d+"'` and check
   each hit against the version that crate actually publishes — a submodule
   crate on its own version line is often already correct.
5. Move the `Unreleased` section of `CHANGELOG.md` into a dated version
   section, and record every intentional breaking change. `workspace-semver`
   reports the API diff against the last published release.
6. Confirm `repository` in `[workspace.package]` is the canonical hosting URL.
   crates.io records it permanently.
7. Confirm the release gate above is green at the exact revision to be tagged.
8. Tag `vX.Y.Z`. That is the release decision: `release.yml` re-runs the gate,
   refuses a tag disagreeing with the workspace version, and publishes every
   crate in dependency order, submodule crates included, skipping versions
   already on crates.io so a resumed release is safe.

The first `1.0.0` requires a stable facade, documented support window,
production HTTP/1 hardening, independent security review, and at least two real
applications exercising the public API. HTTP/2 is not on that list.
