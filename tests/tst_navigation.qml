import QtQuick
import QtTest
import app.chilpa
import app.chilpa.sources
import "qrc:/qt/qml/app/chilpa/qml/Patterns"

TestCase {
    name: "SpotifyNpbContract"
    when: windowShown
    visible: true
    width: 900; height: 300
    PlayerLcd { id: lcd; width: 700 }
    function test_the_npb_accepts_a_native_album_route_with_a_negative_id() {
        Sources.show({playing_parent: "", music: true})
        Player.album = "Fixture record"
        Player.albumId = -42
        Player.artist = "Fixture artist"
        Navigation.page = ""
        waitForRendering(lcd)
        const link = findChild(lcd, "playerAlbum")
        verify(link.visible && link.width > 0)
        mouseClick(link, link.width / 2, link.height / 2)
        compare(Navigation.page, "album")
        compare(Navigation.selectedId, -42)
    }
}
