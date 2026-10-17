# Validation

I validated version 0.3.0 on 2026-10-16, using Rust 1.98.1 on Linux x86_64
and Rust 1.96.0 on macOS 26.6.2 Apple Silicon. I changed only this plugin;
I made no changes to Chilpa core, its UI, facade, SDK or vendored librespot.

I rebuilt the public packages on 2026-10-17 with compiler paths remapped.
Both binaries contain no local `/home/` or `/Users/` build paths. I repeated
the ten executable protocol checks and exact-package host loading on each
platform. Runtime source and dependency versions are unchanged from the
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

I checked both ZIPs have ten flat entries, version 0.3.0, executable permissions,
matching binary hashes and SHA-256 sidecars, identical README, lockfile and source
pins, and Carmine Paolino's Spotifast MIT attribution. I verified all 666 vendored
source files. Both packages include the unchanged MPL dependency source and the
license notices for both target dependency graphs. The Mac executable links only
system IOKit, CoreFoundation, SystemConfiguration, Security, libiconv and libSystem.

I target native process protocol 1, manifest API 1 and the reviewed SDK
`protocol-4` at `edee9aeb704e44dbeb883036ef6dcd871e5df8f4`.
`SOURCES.json` records the immutable Spotifast and librespot source pins.

## Automated checks

On each platform, the final source passed:

- Rust: 64 passed, 0 failed, 1 ignored.
- Executable JSON-RPC protocol: 10 passed, 0 failed.
- Locked release build: passed.

The ignored test is an explicit-account Pathfinder diagnostic. I did not turn
it into an ordinary network test. The default suites need no live Spotify account.

I cover unknown legacy pause migration versus known rolling 429s, account/client
separation, persisted probe leases after cancellation/restart, independent search
branches, same-account auxiliary authorization, grant refresh isolation, real scope
checks, saturated Retry-After serialization, nested snapshot freshness, conservative
version-one cache fallback, last-complete-library preservation, current cache
invalidation keys, radio order and duplicates, inline continuation contexts,
deferred-page rejection, canonical links, native ISRC and artist references,
playlist ownership, write request bodies/headers and durable request replay handling.

I exercise create/add against a local HTTP fixture, including ambiguous failures.
I do not create or modify real user playlists during verification. Existing grants
without recorded playlist scopes must explicitly enable playlist writing.

On Linux, the icon tint and NPB navigation QML checks each passed 3 tests.
The builds retain the pinned librespot missing-Git-metadata notices and its unused
lint expectation. The plugin also reports unused test helpers, one test-only import,
and the legacy saved-pause helper. These are warnings, not build or test failures.

## Exact-package host checks

On Linux I loaded the final ZIP in disposable profiles with the existing host
`20261016-95ecf710c/chilpa-qt`, source
`95ecf710c1a091d1f27fd49fb367875e7594596b`.
The package/permissions/settings smoke passed. A separate real SearchModel fixture
passed two queries with tracks, albums, artists and playlists. Each query returned
ten tracks, ten albums and ten artists; playlist counts were six and four.
The legacy unknown deadline remained saved while catalogue and playlist capability
checks succeeded independently. I required no additional authorization for this.

With a known global Web API pause in another isolated profile, I verified the NPB
album route, 18 album tracks, ten artist releases, ten popular tracks, the existing
artist Radio header action, Discover, editorial fallback and New Music Friday's
100 playlist tracks. Radio returned 50 tracks, and reopening the mix kept its
exact order. I checked that native playlist routing consumes that same Radio mix.
I did not play audio or change live account settings during these checks.

On macOS I loaded the exact Mac ZIP in the existing native-plugin host from
`fix/macos-native-plugins`, source `700a984977bb76ef857adeaeb32504814e64d51d`.
I verified its isolated packaged worker remained alive and matched the binary hash.
That older host lacks an external QML smoke hook; I verified settings and callbacks
through the executable protocol. I did not transfer the user's Spotify credentials
to the Mac, so live browsing and audible Mac playback remain unverified.

## Service limits

The tested account's Pathfinder Home endpoint still rejects this client with 403.
I retain personal last-good Home data when present and otherwise label session
editorial discovery honestly. Search recovery does not imply Pathfinder access.
I honor all newly observed 429 deadlines; optional shared search is not automatic
quota failover. I select an initial finite Radio mix rather than following an
unbounded station continuation. I make no lossless or Spotify Connect claim.
Linux audible playback was confirmed by the user before this release; this round
of validation is read-only and silent.
