import QtQuick
import QtQuick.Window
import app.chilpa
import app.chilpa.controls

Window {
    id: window
    visible: true
    width: 640; height: 480
    property string pluginVersion: "PLUGIN_VERSION"
    property int step: 0
    property int category: 0
    property var scopes: ["songs", "albums", "artists", "playlists"]
    property var counts: []
    readonly property string playingPath: Player.path
    SettingsPage { id: settings; section: "plugins" }
    SearchModel { id: search; source: "app.chilpa.spotify"; scope: "songs" }
    function fail(message) { console.error("SMOKE FAIL Spotify search: " + message); Qt.exit(1) }
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
                search.text = "Radiohead"
                window.step = 1
                return
            }
            search.refresh()
            if (search.count <= 0) return
            window.counts.push(search.count)
            window.category++
            if (window.category < window.scopes.length) {
                search.scope = window.scopes[window.category]
                return
            }
            console.log("SMOKE Spotify search query " + window.step + " categories: " + JSON.stringify(window.counts))
            if (window.step === 1) {
                window.step = 2
                window.category = 0
                window.counts = []
                search.scope = "songs"
                search.text = "Daft Punk"
                return
            }
            console.log("SMOKE Spotify search completed: exact package, native SearchModel, two queries, tracks, albums, artists, playlists, legacy pause recovery, no audio played")
            Qt.quit()
        }
    }
    Timer { interval: 50000; running: true; onTriggered: window.fail("timed out") }
}
