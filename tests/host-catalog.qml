import QtQuick
import QtQuick.Window
import app.chilpa
import app.chilpa.sources

Window {
    id: window
    visible: true
    width: 640; height: 480
    property string pluginVersion: "PLUGIN_VERSION"
    property int albumId: 0
    property int step: 0
    // Construct the same singleton that advances catalog revisions in Main.qml.
    readonly property string playingPath: Player.path
    SettingsPage { id: settings; section: "plugins" }
    AlbumDetailModel { id: album; albumId: window.albumId; contentRev: Library.remoteRev + Library.contentRev }
    TrackListModel { id: tracks; kind: "album"; albumId: album.selectedId; albumPage: album.tracksRev }
    function fail(message) { console.error("SMOKE FAIL Spotify catalog: " + message); Qt.exit(1) }
    Timer {
        interval: 100; repeat: true; running: true
        onTriggered: {
            const status = JSON.parse(settings.pluginsJson())
            if (status.busy || status.working) return
            const installed = (status.installed || []).find(row => row.id === "app.chilpa.spotify")
            if (!installed) return
            if (!installed.native || !installed.enabled || installed.state !== "fine" || installed.version !== window.pluginVersion) {
                window.fail("the expected package did not load"); return
            }
            if (window.step === 0) {
                window.albumId = Sources.referenceCard("app.chilpa.spotify", "album:4aawyAB9vmqN3uQ7FjRGTy")
                if (window.albumId >= 0) { window.fail("the album reference has no native route"); return }
                // This is the same navigation call used by the NPB album link.
                Navigation.go("album", window.albumId)
                window.step = 1
            }
            if (window.step === 1) {
                if (!album.found || tracks.count !== 18) return
                if (Navigation.screen !== "album" || Navigation.routeId !== window.albumId || album.title === "") {
                    window.fail("the NPB album route did not retain its record"); return
                }
                window.step = 2
            }
            const artist = JSON.parse(Sources.page("app.chilpa.spotify native:artist:0TnOYISbd1XYRBk9myaseg", Library.remoteRev))
            if (artist.loading) return
            if (artist.error) { window.fail("the artist page failed"); return }
            if (!(artist.shelves || []).some(shelf => shelf.kind === "album" && shelf.items.length === 10)) return
            if (!(artist.shelves || []).some(shelf => shelf.kind === "track" && shelf.items.length === 10)) return
            if (!(artist.header || {}).radio_id || artist.header.radio_id >= 0) {
                window.fail("the artist Radio action has no native playlist route"); return
            }
            const home = JSON.parse(Sources.page("app.chilpa.spotify native:discover:home", Library.remoteRev))
            if (home.loading) return
            if (home.error) { window.fail("Discover failed while Web API was paused"); return }
            if (!(home.shelves || []).some(shelf => shelf.kind === "playlist" && shelf.items.length > 0)) return
            const releases = JSON.parse(Sources.page("app.chilpa.spotify native:discover:releases", Library.remoteRev))
            if (releases.loading) return
            if (releases.error) { window.fail("new music discovery failed while Web API was paused"); return }
            if (!(releases.shelves || []).some(shelf => shelf.kind === "playlist" && shelf.items.length > 0)) return
            const playlist = JSON.parse(Sources.page("app.chilpa.spotify native:playlist:37i9dQZF1DX4JAvHpjipBk", Library.remoteRev))
            if (playlist.loading) return
            if (playlist.error) { window.fail("the editorial playlist did not open"); return }
            if (!(playlist.items || []).length) return
            const radio = JSON.parse(Sources.page("app.chilpa.spotify native:radio:artist:0TnOYISbd1XYRBk9myaseg", Library.remoteRev))
            if (radio.loading) return
            if (radio.error) { window.fail("the artist Radio page failed"); return }
            if (!(radio.items || []).length) return
            if (window.step === 2) {
                // The header action resolves this same playlist card before playback.
                window.albumId = artist.header.radio_id
                window.step = 3
                return
            }
            if (!album.found || tracks.count !== radio.items.length || album.title !== "Radio") return
            console.log("SMOKE Spotify catalog completed: exact package, NPB album route, 18 album tracks, 10 artist releases, popular tracks, reachable artist Radio, playlist.get Radio with " + tracks.count + " tracks, Discover, new music, editorial playlist, Web API paused, no audio played")
            Qt.quit()
        }
    }
    Timer { interval: 30000; running: true; onTriggered: window.fail("timed out") }
}
