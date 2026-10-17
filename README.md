# Chilpa Spotify plugin

I integrate Spotify with Chilpa's existing process-plugin API. I keep all
provider code, account state and stream handling in this repository. I do not
require changes to Chilpa core, the facade, the UI, or the plugin SDK.

I publish this independently as [vicrodh](https://github.com/vicrodh), as a
working example of the [native process plugin contract](https://github.com/vicrodh/chilpa-plugins-sdk/blob/main/wiki/Native-Plugins.md).
I keep the source and Linux/macOS release packages in
[chilpa-pg-spotify](https://github.com/vicrodh/chilpa-pg-spotify).

## Credits and source attribution

I base this plugin on the approach developed by **Carmine Paolino** in
**[Spotifast](https://github.com/crmne/spotifast)**, a native Spotify client.
I credit Carmine for the separate library/playback authorization design,
the public OAuth application defaults, the PKCE flow structure and the
Spotify API conventions used here. I adapt the session metadata and playlist
readers (`session_reads.rs`), personalized Home query (`made_for_you.rs`),
rootlist reader (`player.rs`), account snapshot strategy (`account_cache.rs`)
and quota classification (`api/client.rs`), operation-specific grants
(`api/gateway.rs`), radio (`session_reads.rs`, `app/radio.rs`) and playlist
writes, as well as authorization code from
Spotifast revision
`0b3e98087184fbda21ad4c720e220bd870148628` (0.13.3).
Spotifast's copyright remains Carmine Paolino's; I include its complete
[MIT license](licenses/Spotifast-MIT.txt) with the source and the package.

I use **[librespot](https://github.com/librespot-org/librespot)** by Paul
Lietar and the Librespot Org contributors for Spotify sessions, metadata,
audio keys, encrypted temporary buffering and decryption. I vendor the exact
[crmne/librespot](https://github.com/crmne/librespot) revision used by the
reviewed Spotifast source:
`ef27e124fe50b8519b5a5c6fa92c8f47f2544385`.
I preserve that source and its [MIT license](licenses/librespot-MIT.txt).
`SOURCES.json` and `vendor.sha256` record provenance and integrity.
I include registry dependency notices in `THIRD-PARTY-NOTICES.txt`. I select
the MPL 2.0 option for `priority-queue` and include its unchanged covered
source as `priority-queue-source.tar.gz` in the source tree and ZIP.

I am not affiliated with Spotify or Spotifast. Spotify is a trademark of
Spotify AB. This is an unofficial client using librespot; I do not describe
its streaming implementation as an official Spotify Web API capability.
Spotify's [Developer Terms](https://developer.spotify.com/terms) and
[Web API documentation](https://developer.spotify.com/documentation/web-api)
are separate from the open-source licenses.

## What I implement

- Native Chilpa search for tracks, albums, artists and playlists, including
  Spotify links and URIs.
- Albums with all their tracks, artist pages, playlist order and duplicate
  song occurrences. Local and missing playlist entries remain unavailable.
- Saved albums, liked songs, followed artists and playlists, with provider pagination.
- Discover with session-based personalized Home when Spotify accepts it,
  Spotify editorial playlists as a session fallback, new and lesser-known
  albums through Web Search, and an activity tab.
- Persistent account snapshots and coalesced refreshes, including after restart.
- English interface text by default, with an optional Spanish translation.
  **Use Spanish** / **Use English** in plugin settings saves the preference.
  Provider titles, artist names, playlist names and lyrics keep their original text.
- Library save/remove actions through Spotify's current `/me/library` API.
- Create playlists and add ordered tracks, including duplicates, through the
  existing Chilpa playlist picker. Write access requires an explicit Spotify approval.
- Session Radio from album and artist pages, keeping the displayed mix stable
  until an explicit refresh. I use the initial finite mix Spotify supplies;
  I do not chase the station's ongoing continuation.
- Separate browser PKCE approvals for the Web API and local playback.
- A three-step setup that recommends a personal Client ID and generates callback URLs.
- Source lyrics through the existing `track.lyrics` contract, including timing,
  language and the provider attribution Spotify supplies.
- Full-track Vorbis streaming through librespot, selected in 320, 160,
  then 96 kbps preference order when the account offers those files.
- A private loopback HTTP endpoint with byte ranges. Chilpa decodes the
  original Vorbis stream and controls output, volume, queue, pause and seek.
  I pass the provider's normalization gain and peak to the existing host
  normalization stage; I do not apply a second gain inside the plugin.

I require Spotify Premium for audio. Account and app access restrictions can
also affect browsing. I retain Spotify errors instead of showing an invented
empty library. I do not automatically retry account writes after a timeout.

## Install and sign in

I build separate Linux x86_64 and macOS Apple Silicon ZIPs under `dist/`.
In Chilpa I open the plugin manager, install the ZIP for my operating system,
review its requested permissions and enable Spotify.

For macOS validation I use the **existing private native-plugin host** from
`fix/macos-native-plugins`, commit `700a984977bb76ef857adeaeb32504814e64d51d`.
The reviewed release host does not enable process plugins on macOS. I do not
patch core or claim that installing a ZIP enables a runtime absent from that host.
I have not built an Intel Mac package.

1. **Choose access.** I recommend my own Spotify developer Client ID. I register
   the displayed callback URL in my app and add my account in Users Management.
   I paste only the Client ID; no Client Secret is needed. The shared Spotifast
   app remains an explicit alternative, subject to its shared quota.
2. **Connect the library.** I press **Open Spotify: connect library**, approve
   in the browser, and return to Chilpa. The callback advances the wizard automatically.
3. **Authorize playback.** I press **Continue: authorize playback**, then
   **Open Spotify: authorize playback**. I approve with the same Spotify account.
   This second browser link is a separate required action, not an automatic redirect.

I display the generated callback URL before opening the browser. Library
access uses `http://127.0.0.1:8989/login`; playback uses
`http://127.0.0.1:8898/login`. I use the host's existing pending-settings
refresh to complete the browser response, without modifying Chilpa core.

I enter my password only in Spotify's browser page. Expired or rejected steps stop
polling and offer a fresh retry. Manual callback entry remains an optional fallback.
After setup I can select **Check library access**. Reconnecting the
same library account preserves its existing playback approval; changing accounts
requires a matching playback approval. **Enable playlist writing** reconnects
that Web grant with the required playlist scopes. Existing grants are never
assumed to have write permission. **Enable shared search** is an optional
additional same-account approval; it preserves personal and playback access.

Shared app quotas and personal app allowlists remain Spotify-controlled. I follow Spotify's documented
[PKCE authorization flow](https://developer.spotify.com/documentation/web-api/tutorials/code-pkce-flow).
The playback grant uses the same desktop client identity and redirect
`http://127.0.0.1:8898/login` as Spotifast.

I reserve the appropriate loopback callback port before offering the browser
link. If another application owns that port, I report it before sign-in.
Canceling releases the listener. I validate the returned origin, path,
state, one-time code and ten-minute expiry before exchanging the code.
I handle a declined browser approval without retrying the exchange.
Advanced options retain a masked manual-response field as a fallback; I
prefill it if a captured response needs inspection. I never save callback
responses to disk or include them in logs.

## Permissions and account storage

I declare `register_source`, `register_panel`, `network` and
`own_account` with `keyring: false`. I request no access to the local music
library, other plugins, selected folders or host playlist writes.

The current Linux host grants process plugins a shared network namespace;
the manifest domain list is not an enforced outbound firewall for native
librespot connections. The listed hosts describe the Web API and artwork;
librespot also resolves regional Spotify access points and audio CDNs.
I bind the audio endpoint only to `127.0.0.1` and use a random 256-bit URL
ticket. Tickets, account credentials and stream URLs stay out of logs.

I store refresh tokens and reusable playback credentials in
`<data_dir>/account.json`, using atomic replacement and mode `0600` in a
mode `0700` directory on Linux and macOS. This file is not encrypted and does not use
the OS keyring. Optional shared search uses its own refresh token in that file.
**Sign out** clears all grants, shuts down the
session and invalidates audio tickets. Clearing local credentials does not
revoke Spotify's server-side app approval; I revoke that in my Spotify
account when needed.

I use only the data and cache paths supplied by the host. Librespot uses
temporary encrypted files under the private cache directory and removes
them when their streams are released. I keep no offline collection and
never persist decrypted audio. Audio URLs remain reusable for one hour
after their last access; I retain at most 128 resolutions per process.

## Discover, quotas and partial access

I follow Spotifast's playback-session route for album and track metadata,
artist releases and popular tracks, playlist headers and contents, and the
account's playlist rootlist. These reads do not spend the selected Web API
application's quota. Playlist entries retain provider order, duplicates,
unavailable placeholders and their actual album reference for NPB navigation.
A changing playlist revision stops the read rather than merging two versions.
Metadata batches contain at most 50 unique entities.

I adapt Spotifast's Pathfinder `homeSection` query for personalized Home.
That contract credits [Psst](https://github.com/jpochyla/psst/pull/745). It
uses login5 and client tokens from the playback session and the same account's
Web grant as context. That private endpoint can reject a client independently
of the Web API. When it does, I resolve public Spotify editorial playlist
headers through the session: New Music Friday, Fresh Finds and Top 50 Global.
I show their actual provider titles and images, mark the response as
nonpersonal, and label the shelf **Featured by Spotify**. I do not describe
editorial playlists as personalized mixes. The current live check exercised
this fallback after Pathfinder returned HTTP 403.

I use Spotify's current [Search filters](https://developer.spotify.com/documentation/web-api/reference/search)
`tag:new` for albums released in the last two weeks and `tag:hipster` for
lesser-known albums. I paginate ten albums at a time. If both release searches
fail while playback is authorized, the tab still offers the session-based
new music and charts playlists. Personal top tracks and artists use `/me/top/*`
when playback has not been authorized; activity stays in its own tab.

I do not call the removed Web API `/browse/new-releases`, recommendations or
artist top-tracks endpoints. I follow Spotify's
[February 2026 migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide).
The artist's popular tracks come from session metadata instead.

I store account-scoped JSON snapshots under `<cache_dir>/snapshots`, with
atomic mode-0600 files in a mode-0700 directory. Library results and their
underlying Web pages stay fresh for six hours; catalog and Search responses
for one hour; Discover pages for twenty minutes. Playlist snapshots stay
fresh for ten minutes and rootlist/header reads for thirty minutes. I retain
at most 512 snapshot files, combine simultaneous requests for the same key,
and keep the last successful copy during transient network or quota failures.
I never retimestamp a complete snapshot with stale or partial nested results.
I retain version-one snapshots as conservative fallback copies during migration.
A permission denial remains an error. Sign-out clears the current account's
snapshots; changing accounts never returns another account's data.

Manual library refreshes share at least one minute. I compare the first page
and total with the previous result: when unchanged, I reuse the remaining
cached pages; when changed, I invalidate that category before reading it.
Pages also expire independently, so a change deeper in the list is not hidden
indefinitely by an unchanged first page. Successful favorite actions invalidate
personal library snapshots. This avoids downloading ten identical Web pages
every time Chilpa asks for 500 library rows.

Spotify's [rate limit](https://developer.spotify.com/documentation/web-api/concepts/rate-limits)
applies to application traffic. A proxy using one Client ID pools that traffic.
A personal Client ID separates traffic from the shared public app, but remains
subject to Spotify's [development quotas](https://developer.spotify.com/documentation/web-api/concepts/quota-modes),
including developer-account quotas across owned apps.

I distinguish `QUOTA_EXCEEDED` from a normal rolling rate limit and honor the
full `Retry-After`, including long waits. Spotify does not publish bucket
membership, so I persist a development-quota pause for the observed route
family; a normal rolling limit pauses the selected Web grant. Session Home
has its own pause file. I retain an older global Web pause whose original endpoint was not saved.
I allow one persisted capability check each for catalogue and playlist Search;
a successful check permits only that operation while preserving the unknown
deadline for other operations. A known rolling HTTP 429 never takes this path.
Cancellation or restart preserves a five-minute probe lease. Fresh provider
limits retain their full deadline, including long waits.

I search tracks, albums and artists separately from playlists, with bounded
concurrent branches. The selected Web grant handles catalogue search. Playlist
search uses optional shared access when explicitly connected, otherwise the
selected grant. I never switch identities after a quota error or use playback
tokens for Web API searches. Available categories remain visible if another
branch fails; settings reports partial access. A personal app requests at most
ten results per category per page. I coalesce duplicate queries and reuse cached
results; I do not add another debounce on top of Chilpa's existing search delay.
Session browsing remains available when a Web grant is paused.

I do not import Spotifast's account files or automatically authorize an additional
shared grant.

## Lyrics

I ask Spotify's `color-lyrics` endpoint through the authorized playback session,
as Spotifast does. This is a private service API, not a public Web API lyrics
endpoint. I return Chilpa's existing document format with original millisecond
timing, language and provider name. Missing or unordered timestamps produce
unsynced text; a 404 means no lyrics, while a failed lookup stays an error.
I bound the lookup below the host's four-second source deadline.

Chilpa already caches and presents source lyrics and handles its other providers
when needed. I do not add another external lyrics service inside this plugin.
Lyrics availability depends on the song, account and region.

## Build and verify

I tested Rust 1.98.1 on Linux and 1.96.0 on macOS with the committed lockfile.
I need a C linker and Python 3. On Linux I also need OpenSSL development headers
and `pkg-config`; on macOS I use the Xcode command-line tools and native TLS frameworks.
I do not build a GUI, an audio backend
or another copy of Chilpa.

```sh
python3 tools/verify-vendor
cargo test --locked --bin plugin
python3 tools/collect-licenses
tools/build-native
SPOTIFY_PLUGIN_BINARY=target/release/plugin python3 -m unittest discover -s tests -v
```

On the Chilpa development machine I run Cargo under the shared build lock
and `cargo-capped` memory limit. I build only one Rust target at a time.
`tools/build-native --binary /path/to/plugin` packages a previously built
release executable without starting Cargo.
The build script remaps local compiler paths before packaging a public release.

I test the exact ZIP through the existing host, in a disposable profile:

```sh
python3 /path/to/chilpa/code/scripts/check-process-plugin.py \
  /path/to/chilpa-qt dist/app.chilpa.spotify-0.3.0-linux-x86_64.zip /tmp/spotify-host-smoke
```

On macOS I run `python3 tools/check-macos-host HOST ZIP OUTPUT` against the existing
native host. It checks the exact packaged worker in an offscreen disposable
profile. That older host has no external QML-smoke hook; settings are tested
separately through the executable protocol. I never substitute a Linux binary
in a Mac ZIP: packaging reads the executable's ELF or Mach-O architecture.

I pin the process contract to protocol `1`, manifest API `1`, SDK
`protocol-4` (`edee9aeb704e44dbeb883036ef6dcd871e5df8f4`), and reviewed
Chilpa host `0cb8a28814d1c1af1b053f2326146fdf35abcc43`.
I consume the process contract directly; I do not link the Rust WASM SDK.
Detailed validation results live in `VALIDATION.md`.

## Current limits

I have not implemented Spotify Connect receiver
control, remote queue mirroring, playlist rename/reorder/removal, podcasts,
lossless delivery or offline downloads. Chilpa already owns its player and
presentation; I do not embed Spotifast's GUI, skins, equalizer or visualizers.

I allow adding tracks only to playlists owned by the approved account; a
collaborative flag alone does not establish write permission. Writes require
actual `playlist-modify-private` and `playlist-modify-public` scopes. I use
`POST /me/playlists` and `POST /playlists/{id}/items`, with up to 100 tracks
per add. A private request journal prevents replaying request IDs after restart
or an uncertain response. Replaying a confirmed create omits its reference so
Chilpa cannot repeat the initial track chunks; it tells the user to inspect the
playlist. Unidentified adds remain distinct intentional operations.

I preserve playlist occurrences but do not expose editable membership IDs.
An album or playlist listing is bounded at 10,000 entries and reports an
error when it exceeds that bound. Artist release pages are paginated.
Artist scene searches use the host's five-call default
budget to avoid unbounded shared-app traffic. I do not report resolution
or prefetch as audible listening history; Spotify history behavior for this
direct audio route is not verified.

I verified live catalog replies and synchronized source lyrics with an existing
account; audible Linux playback was confirmed during use. I make no lossless
claim: this implementation selects Vorbis. Passing package and sandbox checks
does not prove audible playback on another operating system or account.
