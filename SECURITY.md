# Security policy

## Supported versions

Blazingly is currently pre-1.0. Security fixes are applied to the latest
development release. Older `0.x` lines do not receive a guaranteed maintenance
window yet.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use the repository's
private GitHub security-advisory reporting flow and include:

- affected revision and feature flags;
- a minimal reproduction;
- expected impact;
- whether the issue is remotely reachable;
- any suggested mitigation.

Please avoid testing against systems or data you do not own. Acknowledgement,
severity assessment, and disclosure timing are coordinated in the private
advisory.

## Release checks

CI runs dependency-advisory scanning, Miri, AddressSanitizer, bounded fuzz
smoke tests, cross-platform tests, and API compatibility checks. These controls
reduce risk but are not a claim that the project has completed an independent
third-party security audit.
