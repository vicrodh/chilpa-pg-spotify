# Spotify for Chilpa

Spotify search, library, discovery and playback through Chilpa's native process
plugin API. All provider code and account handling live in this repository;
no changes to Chilpa core, UI or SDK are required.

I publish this plugin independently as [vicrodh](https://github.com/vicrodh).
Source and platform packages are available in
[chilpa-pg-spotify](https://github.com/vicrodh/chilpa-pg-spotify).
The SDK includes it as a [native source example](https://github.com/vicrodh/chilpa-plugins-sdk/blob/main/wiki/Native-Plugins.md#native-source-example).

## Features

- Search tracks, albums, artists and playlists, including Spotify links and URIs.
- Browse albums, artist releases, popular tracks, playlists and saved library items.
- Discover personalized Home when available, editorial playlists and new releases.
- Radio from album and artist pages, with a stable mix until explicitly refreshed.
- Create playlists and add ordered tracks through Chilpa's playlist picker.
- Source lyrics with timing, language and provider attribution when available.
- Full-track Ogg Vorbis playback, preferring 320, then 160, then 96 kbps.
- English interface with an optional saved Spanish preference.

Chilpa handles decoding, output, volume, queue, pause and seek. The plugin
serves the original decrypted Vorbis stream over a private loopback endpoint
with byte-range support. Provider normalization metadata goes to Chilpa's
existing normalization stage; the plugin applies no additional gain.

## Requirements

- A Chilpa host with native process plugin support.
- Linux x86_64 or macOS Apple Silicon. macOS requires a host with the native
  sandbox enabled; installing the ZIP does not add that runtime to older hosts.
- Spotify Premium for audio playback. Account restrictions and application
  quotas can also affect browsing.
- A personal Spotify developer Client ID is recommended. No Client Secret is needed.

Live catalog and audio checks were performed on Linux. The Mac package passed
protocol tests and isolated host loading; live Mac playback remains unverified.
Exact host revisions, package hashes and test results are in [VALIDATION.md](VALIDATION.md).

## Installation and sign-in

Download the ZIP for the target system from [Releases](https://github.com/vicrodh/chilpa-pg-spotify/releases).
Install it through Chilpa's plugin manager, review the requested permissions
and enable Spotify.

The setup wizard has three steps:

1. **Choose access.** Enter a personal Client ID, register the displayed callback
   URL in the developer app and add the account in Users Management. Shared
   Spotifast access is an explicit alternative with a shared request quota.
2. **Connect the library.** Select **Open Spotify: connect library**, approve in
   the browser and return to Chilpa. The callback advances the wizard.
3. **Authorize playback.** Select **Continue: authorize playback**, then
   **Open Spotify: authorize playback**. Approve with the same Spotify account.

Library and playback use separate PKCE approvals and callbacks:

| Approval | Callback |
| --- | --- |
| Library | `http://127.0.0.1:8989/login` |
| Playback | `http://127.0.0.1:8898/login` |

The wizard generates these URLs and reserves the callback port before offering
the browser link. Expired or rejected steps stop polling and allow a fresh
attempt. Manual callback entry is available as a fallback. Passwords belong
only on Spotify's browser page.

Reconnecting the same library account preserves playback approval. Changing
accounts requires matching playback approval. **Enable playlist writing**
requests the required playlist scopes; older grants are not assumed to have
those permissions. **Enable shared search** adds optional same-account Web
access while preserving the primary connection and playback.

## Permissions and storage

The manifest requests `register_source`, `register_panel`, `network` and
`own_account` with `keyring: false`. It requests no access to local music,
other plugins, selected export folders or host playlist writes.

Refresh tokens and reusable playback credentials are stored unencrypted in
`<data_dir>/account.json`, with mode `0600` inside a mode `0700` directory.
The OS keyring is not used. **Sign out** clears the grants, closes the session
and invalidates audio tickets. Server-side app approval can be revoked in the
Spotify account settings.

Persistent snapshots use `<cache_dir>/snapshots`. Temporary audio files remain
encrypted and are removed when their streams are released. Decrypted audio is
not saved. The loopback endpoint binds only to `127.0.0.1`, uses random 256-bit
tickets, and retains at most 128 resolutions. Credentials, callback responses,
stream URLs and tickets are excluded from logs.

Native workers have direct networking; the manifest domain list is not an
outbound firewall for librespot's regional access points and audio CDNs.

## Limits

- Vorbis playback is lossy. Lossless delivery, Spotify Connect receiver control,
  remote queue mirroring, podcasts and offline downloads are not implemented.
- Playlist writes are limited to creation and adding up to 100 tracks per request
  to playlists owned by the approved account. Rename, reorder and removal are
  not implemented. Duplicate song occurrences retain their order.
- Writes are not automatically replayed after an uncertain response. A private
  request journal handles repeated request IDs; inspect the playlist before
  retrying an operation whose outcome is unclear.
- Album and playlist listings are bounded at 10,000 entries. Artist releases are
  paginated; artist scene search uses the host's five-call default budget.
- Provider quotas and private endpoint access remain outside the plugin's control.
  Cached and session-based browsing can remain available during a Web API pause.
- Stream resolution and prefetch are not recorded as audible listening history.

## Build and verify

Build dependencies: Rust, a C linker and Python 3. Linux also needs OpenSSL
development headers and `pkg-config`; macOS needs the Xcode command-line tools.
The committed lockfile was tested with Rust 1.98.1 on Linux and 1.96.0 on macOS.

```sh
python3 tools/verify-vendor
cargo test --locked --bin plugin
tools/build-native
SPOTIFY_PLUGIN_BINARY=target/release/plugin python3 -m unittest discover -s tests -v
```

`tools/build-native` remaps local compiler paths and creates a platform-specific
ZIP under `dist/`. `--binary /path/to/plugin` packages an existing release
executable without invoking Cargo. Packaging identifies the executable's ELF
or Mach-O architecture. Dependency notices can be regenerated with
`python3 tools/collect-licenses`.

Run the exact Linux package through a disposable host profile:

```sh
python3 /path/to/chilpa/code/scripts/check-process-plugin.py \
  /path/to/chilpa-qt dist/app.chilpa.spotify-0.3.0-linux-x86_64.zip /tmp/spotify-host-smoke
```

On macOS, use `python3 tools/check-macos-host HOST ZIP OUTPUT`. This checks the
packaged worker in an isolated profile; executable protocol tests cover setup
and callbacks. See [VALIDATION.md](VALIDATION.md) for the scope of each check.

The implementation uses manifest API `1` and process protocol `1` directly.
The reviewed SDK pin is `protocol-4`, commit
`edee9aeb704e44dbeb883036ef6dcd871e5df8f4`. No WASM SDK is linked.

## Credits and licenses

**[Spotifast](https://github.com/crmne/spotifast)** by **Carmine Paolino** is the
basis for this plugin. I adapted its separate Web/playback authorization,
public OAuth defaults, PKCE flow, operation-specific grants, session metadata
and playlist readers, personalized Home query, rootlist, account snapshots,
quota classification, Radio and playlist write conventions.
The reviewed revision is `0b3e98087184fbda21ad4c720e220bd870148628` (0.13.3).
Spotifast's copyright and complete [MIT license](licenses/Spotifast-MIT.txt)
are retained in the source and packages. Its Home query also credits
[Psst](https://github.com/jpochyla/psst/pull/745).

**[librespot](https://github.com/librespot-org/librespot)**, by Paul Lietar and
the Librespot Org contributors, provides sessions, metadata, audio keys,
encrypted buffering and decryption. The unchanged vendored source is the
[crmne/librespot](https://github.com/crmne/librespot) revision used by Spotifast:
`ef27e124fe50b8519b5a5c6fa92c8f47f2544385`. Its
[MIT license](licenses/librespot-MIT.txt) is included. [SOURCES.json](SOURCES.json)
and `vendor.sha256` record provenance and integrity.

The plugin is MIT licensed. Dependency notices are in `THIRD-PARTY-NOTICES.txt`.
For `priority-queue`, the MPL 2.0 option is selected and its unchanged covered
source is included as `priority-queue-source.tar.gz` in the repository and ZIP.

This is an unofficial client with no affiliation to Spotify or Spotifast.
Spotify is a trademark of Spotify AB. The open-source licenses are separate
from Spotify's [Developer Terms](https://developer.spotify.com/terms) and
[Web API documentation](https://developer.spotify.com/documentation/web-api).
