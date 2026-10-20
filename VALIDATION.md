# Validation

## Library enrichment 0.3.4 (phase 4)

Validated on 2026-10-20 using repository source, inline protobuf fixtures and
loopback HTTP only. No public downloads, authenticated service calls, owner
account files, private diagnostic/cache files or environment files were read.
The supplied live counts are user-provided evidence: all three partner operations
returned 403, Home also returned 403, and extended metadata still worked.

### Routes and static evidence

Labels and composer credits now share the playback session's
`POST /extended-metadata/v0/extended-metadata` transport in
`src/session_enrichment.rs:83`. The vendored implementation is
`vendor/librespot/core/src/spclient.rs:583`; session authentication is attached
at lines 497 and 511. The same transport is already used by
`src/native_catalog.rs:223`. It needs no partner operation hash.

- Labels: `src/session_labels.rs:88` parses the album protobuf;
  `src/session_labels.rs:227` selects `ALBUM_V4` (9). The vendored mapping is
  `vendor/librespot/core/src/spclient.rs:637`; `metadata.proto:50` defines
  `label = 5` and line 57 defines `copyright = 13`.
  `vendor/librespot/metadata/src/album.rs:32` exposes the same label.
  A nonblank protobuf label wins; otherwise `owner()` is applied to the first
  P-type copyright only. C-only copyright is not a label. Library track rows
  resolve their `album_ref`, sharing the same per-album cache with album rows.
  Existing labels survive; refreshed enrichment records `label_source` as
  `metadata` or `copyright`. The partner `getAlbum` route was removed.
- Credits: `src/session_credits.rs:133` reads only explicit composer roles from
  `TRACK_V4` (10), selected at line 291. The vendored mapping is
  `vendor/librespot/core/src/spclient.rs:628`.
  `vendor/librespot/protocol/proto/metadata.proto:94` defines track
  `artist_with_role = 32`; lines 106, 112 and 113 define composer role 5,
  artist name and role. Names retain response order and are deduplicated.
  The schema establishes composers, not comprehensive songwriter or lyricist
  coverage. `extension_kind.proto:110`, `:144` and `:199` also name
  `TRACK_EXTENDED_CREDITS`, `CREDITS_TRAIT` and `CREDITS_V2_TRAIT`, but this vendor
  contains no corresponding payload schema. The separate experimental
  `track-credits-view` JSON route has no vendored implementation or contract;
  it was not selected or claimed to work. The existing metadata route provides
  a concrete protobuf contract without inventing a JSON response. Both partner
  enrichment queries are gone. Whether actual tracks populate composer roles
  remains unverified.
- Genres: `src/catalog.rs:547` retains the Web API single-artist fallback after
  batch 403/404, and during a batch 403 cooldown. The vendored Artist and Album
  messages (`metadata.proto:11` and `:33`) contain no genres. No alternative
  music-genre source was established. Missing fields stay uncached; explicit
  arrays, including empty arrays, retain the 30-day cache. No genres are inferred.
  The old aggregate diagnostic cannot prove which response the live fallback
  received; the supplied counts alone cannot resolve this. Local fixtures prove
  that it runs and separately records 403, 404, success, malformed data and timeout.

The vendored batch request is a repeated `entity_request`
(`vendor/librespot/protocol/proto/extended_metadata.proto:33`). Neither that schema
nor `get_extended_metadata` declares a numerical server limit. The existing
catalog implementation uses chunks of 50 (`src/native_catalog.rs:208`); enrichment
uses one batch of at most 20 entities, below that existing bound. No larger server
limit is claimed. Each feature keeps its six-second deadline, including gates
and session setup. `album-label:v2` and `track-credits:v2` intentionally leave old
empty answers behind. Found answers last 90 days and empty answers seven days.
Known album-track membership and rotating continuation remain supported.

### Refusals and diagnostics

`src/enrichment.rs:104` and `:130` implement account-scoped route pauses using
`capability:enrichment:<route>`. A 403 stores `until = now + 3600`; an in-memory
copy also prevents repeats when snapshot writes fail. The session endpoint has
one shared pause for both extension kinds. Its gate prevents concurrent label
and credit calls from racing past a refusal. HTTP status, provider status and
per-entity status are checked. Successful siblings are usable, while failed or
malformed rows are never cached as empty answers. HTTP 429 also stops requests
and keeps a short pause; Web API handling continues to honor its existing
Retry-After state.

Web API routes `artist_genres` and `artist_genres_single` pause independently,
so the first batch refusal still permits a single-artist fallback. A single
403 stops that fallback immediately. Pauses survive restart, expire after the
hour and do not cross accounts. These pauses apply to enrichment only and do
not suppress playback or ordinary catalog metadata.

`library-enrichment:v1` now has four independent entries:
`album_label`, `track_credits`, `artist_genres`, `artist_genres_single`.
Each entry retains `at`, `asked`, `with_data`, `cached`, and `outcome`.
`asked` counts submitted entities, not HTTP packets. Genre batch counts no
longer include single-artist attempts. First refusals record
`{"class":"http","status":403}`. Subsequent skipped calls record
`{"class":"disabled","remaining":3599}` with `asked: 0` and current remaining
seconds. Fresh cached answers still apply. A fallback that did not run records
`not_attempted`; cache-only and candidate-free calls use `cache_hit` and
`no_candidates`. No IDs, names, response bodies, headers or tokens enter the
diagnostic. Legacy GraphQL outcome types remain readable solely for merging old
entries until each feature runs again.

Version is 0.3.4 in the manifest, Cargo package and lockfile. Manifest network
permissions, dependencies, vendor pins, SDK and process/API contracts are
unchanged. `api-partner.spotify.com` remains needed by the existing Home feature.
The historical 0.3.3 investigation below describes the superseded enrichment.

### Final checks and package

```text
Verified 666 vendored source files.
test result: ok. 109 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.86s
Finished `release` profile [optimized] target(s) in 23.59s
Ran 10 tests in 0.403s
OK
ZIP verification passed: 10 entries, CRC, sidecar, version, executable mode, packaged files and binary.
```

The Rust suite ran after `python3 tools/verify-vendor` using
`flock /mnt/build/tmp/blitzkriegfc/chilpa-cargo.lock $HOME/.local/bin/cargo-capped cargo test --locked --bin plugin`.
All Cargo commands used the shared lock and capped wrapper, in the foreground,
one at a time, with `CARGO_NET_OFFLINE=true`. The sole ignored test requires
explicit live-account access and was not run. The vendored lint-expectation
warning and four existing release unused-import/helper warnings remain.

`CARGO_NET_OFFLINE=true python3 tools/build-native` produced
`dist/app.chilpa.spotify-0.3.4-linux-x86_64.zip`, SHA-256:

```text
5d8b5bcf920c80c4235835078aa7555ad9b4c6f90b3b0f31f0ca6106c38dcaac
```

The ten protocol tests used the executable extracted from this exact ZIP and
temporary profiles. Packaged binary, manifest, README, Cargo lockfile, source
pins, icon, license and attribution files matched the repository/build output.
The binary contains no owner home-directory build path. No live installation
or host loading check was performed. Actual labels, composer-role population,
writer/lyricist completeness, single-artist genre availability, and Chilpa field
display remain unverified without a live account. These local fixtures establish
parsing, limits, caches, diagnostics and cooldowns, not returned live data.

## Library enrichment 0.3.3

Investigated on 2026-10-20 from repository source and unauthenticated public player
JavaScript. No account or plugin cache files were read and no service API calls
were made. Fixtures use temporary storage and loopback HTTP. The public policy
references below describe platform restrictions, not a measured response from
the owner's client.

### Causes and changes

Credits had a reproducible local blocker: `composers_until` loaded album-track
membership but selected candidates only from track cards and required
`kind == "tracks"` before requesting anything. An album-only run therefore
populated the album map without retrieving credits. It now requests remembered
album tracks under the same 20-request/six-second budget. Cache hits survive
gate timeouts. Missing role/group values are accepted as the player accepts
them; malformed non-null values are rejected. Request failures, session/token
absence, throttling, disabled operations and unexpected shapes are observable.
The disable flag belongs to the client instance, and only a recognized
PersistedQueryNotFound code or exact fixed message sets it.

The current credits contract is unchanged:
`queryTrackCreditsGroupedModal`,
hash `f135fb9be58a72d041ab5d214d817021a272405d883860468e2627afb01a3ca9`,
variables `trackUri`, `contributorsLimit: 100`, `contributorsOffset: 0`;
response `data.trackUnion.creditsTrait.contributors.items`.
There is no static evidence of a separate identity or token-client mismatch.
Library pages already require the same account identity. Session setup, login5
authorization and client-token retrieval remain the shared Home transport path.
Whether this client can use Pathfinder remains unverified.

Labels previously depended entirely on the Web API `label` field. The new
`getAlbum` request uses hash
`0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f`,
variables `uri: "spotify:album:ID"`, `locale: ""`, `offset: 0`, `limit: 50`,
and the player's `spotify-app-version: 896000000` header.
It reads `data.albumUnion.copyright.items` for an `Album` union. The fallback
preserves P-line text, otherwise C-line text, and marks `label_source` as
`copyright`. It does not infer a bare record-label name from a copyright or
courtesy line. Existing labels are preserved. Each album is cached within
`album-label:v1`; found entries last 90 days and explicit empty entries seven.
Malformed replies and failures are not negative-cached. A rotating cursor
prevents repeated failures from starving later albums.

The supplied zero-artist count cannot distinguish an HTTP denial, a quota
pause, missing authorization or a malformed response. The old code swallowed
all these cases. The batch endpoint removal is a documented explanation for
Development Mode clients: Spotify's
[February 2026 changelog](https://developer.spotify.com/documentation/web-api/references/changes/february-2026)
removes `GET /artists` and album `label`. Its
[migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
dates restrictions to February 11 for new apps and March 9 for existing apps,
exempts Extended Quota Mode, and directs clients to individual artist requests.
The plugin now falls back on batch HTTP 403/404, with at most 20 singles and
a six-second total genre budget. A single missing artist does not stop later
artists. HTTP 401/429 do not trigger fallback.

As checked on 2026-10-20, the
[single-artist reference](https://developer.spotify.com/documentation/web-api/reference/get-an-artist)
still describes `genres`, but marks it deprecated. The inspected evidence does
not prove that Spotify has stopped returning it for every client. An actual
array is accepted, `[]` is cached for 30 days, and a missing field is recorded
as `unexpected_shape` and left uncached. No genres are inferred.
The earlier [November 27, 2024 announcement](https://developer.spotify.com/blog/2024-11-27-changes-to-the-web-api)
restricted several recommendation/discovery features for new use cases.
The [April 15, 2025 announcement](https://developer.spotify.com/blog/2025-04-15-updating-the-criteria-for-web-api-extended-access)
changed extended-access eligibility from May 15; neither establishes a universal
artist-genres removal. March, May and July 2026 changelogs were also checked.

### Public bundle evidence

All offsets below are zero-based UTF-8 byte offsets. Bundles were downloaded
from `https://open.spotifycdn.com/cdn/build/web-player/` after obtaining script
URLs from the unauthenticated public player. No player code was executed.

| Bundle | Offset | Observation |
| --- | ---: | --- |
| `web-player.442d5025.js` | 1327137 | `getAlbum` operation and hash, module 798 |
| `web-player.442d5025.js` | 1327764 | Album variables, followed by request/header and `data.albumUnion` handling |
| `web-player.442d5025.js` | 1332443 | Album projection preserves `copyright` |
| `xpui-routes-album.fa460596.js` | 27006 | Album page sends copyright items and courtesy line to its footer |
| `xpui-routes-album.fa460596.js` | 30001 | Footer uses item `text` and C/P `type` |
| `xpui-root-dialogs.70132db5.js` | 576 | Credits operation and unchanged hash, module 49703 |
| `xpui-root-dialogs.70132db5.js` | 716 | Credits variables |
| `xpui-root-dialogs.70132db5.js` | 19428 | Credits contributor response path |
| `web-player.442d5025.js` | 1039335 | Contributor role is optional |
| `web-player.442d5025.js` | 1039782 | Missing role group defaults to Other |
| `web-player.442d5025.js` | 1363926 | `queryArtistOverview`; hash `9f8134ef565e78621f1e1793555bd6633c5ac144ae0f89604ed3ae3f80b3c8e6` |
| `xpui-routes-artist.d9e2484e.js` | 14495 | Artist response consumer; no `genres` occurrence in this entire 107206-byte chunk |

The main bundle's 17 `genres` occurrences concerned browse/search, audiobook
topics, generic protocol fields and flags; no artist-genre response consumer
was found in it or the artist route. This is a bounded negative result, not
proof about every lazy chunk or an undocumented server field.

Bundle SHA-256:

```text
6f56d5b9b7d253c5e9055d6b10c0459bd6bc70609bdf13f2637b2460b2219c9f  web-player.442d5025.js
6d89fcf395d8355de8d267931a47bb93bbb8da4ce2775fa3309eb7a03f2015aa  xpui-root-dialogs.70132db5.js
0aa6574389479faeb93eb754446cec6129441dd3f69828193eaeb8ca1d0aff34  xpui-routes-album.fa460596.js
c597fd405a2d577b689cb47326ebb9c0e0c2c0b7440eb02d4b87e4b5fe250320  xpui-routes-artist.d9e2484e.js
```

REA whole-bundle Evidence
`ev_7169dbebb1d7e3130abc299887d4ca29523dd3be795a2ad4617229f9d6ab0d3f`
retained static facts but reported incomplete semantic coverage from resource
limits. Six exact factory slices (798, 25006, 40891, 78297, 49703, 73062) were
then wrapped in an inert webpack carrier and analyzed as Evidence
`ev_35b84182ff00814649c32a90e23597eb4b06b126fa68927c5ee12a3ea1ec5a01`.
All six parsed without errors; external module resolution and dynamic
semantics remain unknown. The offset findings above also come from direct
static reading of the original bytes, not a claim that a graph trace observed
server behavior. The scratch extraction manifest records the original ranges.

### Diagnostics

`library-enrichment:v1` uses the existing account snapshot envelope and one
payload with keys `artist_genres`, `album_label`, and `track_credits`.
Atomic merges preserve concurrent feature updates. Every attempt contains:

```json
{"at":1792454400,"outcome":{"class":"http","status":403},"asked":20,"with_data":0,"cached":0}
```

`asked` counts submitted item lookups, including failed batch items and their
single-item retries; it is not a packet count. `with_data` counts new answers
with actual genres, copyright text, or composer names. `cached` counts usable
cached answers, including valid empty answers. A failed genre batch followed
by successful singles retains the batch status with positive `with_data`.
The first failure remains the call's outcome; subsequent calls replace it.
Successful calls use `ok`, `cache_hit`, or `no_candidates`. Other classes are
`http` with numeric status, `graphql` with an allowlisted code, `unexpected_shape`,
`transport`, `no_session`, `no_token`, `disabled`, or `timeout` with a fixed stage.
Unknown GraphQL codes become `UNKNOWN`; messages and bodies are never retained.
GraphQL rate limits stop requests and establish a pause, as HTTP 429 does.

Without an account identity there is no account scope in which to store a
diagnostic; library retrieval itself already requires identity. A storage
write failure can also prevent diagnostics from persisting, without failing
the library result. No new diagnostic payload contains names, IDs, tokens,
headers or response bodies.

Final checks for 0.3.3:

```text
Verified 666 vendored source files.
test result: ok. 115 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 6.82s
Finished `release` profile [optimized] target(s) in 22.34s
Ran 10 tests in 0.295s
OK
```

Cargo ran offline through the shared lock and capped wrapper. The ignored test
requires explicit live-account access and was not run. The existing vendored
librespot lint-expectation warning remains.

`tools/build-native` produced
`dist/app.chilpa.spotify-0.3.3-linux-x86_64.zip`, SHA-256
`407716e89e81e95933aadc5420b7b1ba3153d0a1ea8fa2ef1f813c38db44b118`.
ZIP verification passed for ten entries, CRC, SHA-256 sidecar, version,
executable permissions, and identical packaged binary/manifest/README/lockfile/
source pins. The ten protocol tests used the binary extracted from this ZIP
in temporary profiles. The release retained four pre-existing unused
import/helper warnings. No new permissions or dependencies were introduced.

The account's actual Web API status, single-artist genre availability,
Pathfinder authorization, returned copyright/credit data, and host display
remain unverified without a live account. No installation or live host check
was performed in this phase.

## Earlier release validation

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
