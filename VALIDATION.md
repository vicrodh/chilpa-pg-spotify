# Validation

Version 0.3.0 was validated on 2026-10-16 with Rust 1.98.1 on Linux x86_64
and Rust 1.96.0 on macOS 26.6.2 Apple Silicon. The changes were confined to
the plugin; Chilpa core, UI, facade, SDK and vendored librespot were unchanged.

The public packages were rebuilt on 2026-10-17 with compiler paths remapped.
Neither binary contains local `/home/` or `/Users/` build paths. All ten
executable protocol checks and exact-package host loading passed again on
each platform. Runtime source and dependency versions are unchanged from the
64-test and live account validation on 2026-10-16.

## Packages

| Target | ZIP bytes | Executable bytes |
| --- | ---: | ---: |
| Linux x86_64 | 3867989 | 9419016 |
| macOS aarch64 | 3572743 | 8385216 |

Linux ZIP SHA-256: `bbeb71d071f29d29dbb28f21c18c2802db7bdcfb785fca3eea87df950423ee9b`.
Linux executable SHA-256: `81c5e20d7dd77814e3c2e724e5e0c590ba01bfc63c859d84ec290159243818e3`.

macOS ZIP SHA-256: `bb7160f70595442df5f1900fc455f53de329a48655328ddb60ea4b665f5f959b`.
macOS executable SHA-256: `828d0d008e6f4b0da6dc74185d824c9ec1721298a0fc9c7dc148a5ef47fe00af`.

Both ZIPs passed checks for ten flat entries, version 0.3.0, executable
permissions, matching binary hashes and SHA-256 sidecars, identical README,
lockfile and source pins, and Carmine Paolino's Spotifast MIT attribution.
All 666 vendored source files passed verification. Both packages include the unchanged MPL dependency source and the
license notices for both target dependency graphs. The Mac executable links only
system IOKit, CoreFoundation, SystemConfiguration, Security, libiconv and libSystem.

The packages target native process protocol 1, manifest API 1 and the reviewed SDK
`protocol-4` at `edee9aeb704e44dbeb883036ef6dcd871e5df8f4`.
`SOURCES.json` records the immutable Spotifast and librespot source pins.

## Automated checks

On each platform, the final source passed:

- Rust: 64 passed, 0 failed, 1 ignored.
- Executable JSON-RPC protocol: 10 passed, 0 failed.
- Locked release build: passed.

The ignored test is a Pathfinder diagnostic that requires an explicit live
account. The default suites need no live Spotify account.

Test coverage includes unknown legacy pause migration versus known rolling 429s, account/client
separation, persisted probe leases after cancellation/restart, independent search
branches, same-account auxiliary authorization, grant refresh isolation, real scope
checks, saturated Retry-After serialization, nested snapshot freshness, conservative
version-one cache fallback, last-complete-library preservation, current cache
invalidation keys, radio order and duplicates, inline continuation contexts,
deferred-page rejection, canonical links, native ISRC and artist references,
playlist ownership, write request bodies/headers and durable request replay handling.

Create/add tests use a local HTTP fixture, including ambiguous failures.
Verification creates or modifies no real user playlists. Existing grants
without recorded playlist scopes must explicitly enable playlist writing.

On Linux, the icon tint and NPB navigation QML checks each passed 3 tests.
The builds retain the pinned librespot missing-Git-metadata notices and its unused
lint expectation. The plugin also reports unused test helpers, one test-only import,
and the legacy saved-pause helper. These are warnings, not build or test failures.

## Exact-package host checks

On Linux, the final ZIP was loaded in disposable profiles with the existing host
`20261016-95ecf710c/chilpa-qt`, source
`95ecf710c1a091d1f27fd49fb367875e7594596b`.
The package/permissions/settings smoke passed. A separate real SearchModel fixture
passed two queries with tracks, albums, artists and playlists. Each query returned
ten tracks, ten albums and ten artists; playlist counts were six and four.
The legacy unknown deadline remained saved while catalogue and playlist capability
checks succeeded independently with the existing authorization.

With a known global Web API pause in another isolated profile, the host checks
passed for the NPB album route, 18 album tracks, ten artist releases, ten popular tracks, the existing
artist Radio header action, Discover, editorial fallback and New Music Friday's
100 playlist tracks. Radio returned 50 tracks, and reopening the mix kept its
exact order. Native playlist routing consumed that same Radio mix.
These checks played no audio and changed no live account settings.

On macOS, the exact Mac ZIP was loaded in the existing native-plugin host from
`fix/macos-native-plugins`, source `700a984977bb76ef857adeaeb32504814e64d51d`.
The isolated packaged worker remained alive and matched the binary hash.
That older host lacks an external QML smoke hook; settings and callbacks passed
through the executable protocol. No Spotify credentials were transferred to
the Mac; live browsing and audible Mac playback remain unverified.

## Service limits

The tested account's Pathfinder Home endpoint still rejects this client with 403.
The plugin retains personal last-good Home data when present and otherwise
labels discovery as session editorial content. Search recovery does not imply Pathfinder access.
The plugin honors all newly observed 429 deadlines; optional shared search
is not automatic quota failover. Radio uses an initial finite mix, without
unbounded station continuation. Lossless delivery and Spotify Connect are
not implemented. Audible Linux playback was confirmed before this release;
this validation round was read-only and silent.
