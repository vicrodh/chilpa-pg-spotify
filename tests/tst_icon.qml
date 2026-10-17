import QtQuick
import QtQuick.Controls.impl as ControlsImpl
import QtTest

TestCase {
    id: test
    name: "SpotifyIcon"
    when: windowShown
    visible: true
    width: 100
    height: 60
    Rectangle {
        id: background
        color: "white"
        width: 64
        height: 32
        ControlsImpl.ColorImage {
            id: original
            width: 32; height: 32
            sourceSize.width: width; sourceSize.height: height
            source: Qt.resolvedUrl("../icon.svg")
            color: "transparent"
        }
        ControlsImpl.ColorImage {
            id: tinted
            x: 32
            width: 32; height: 32
            sourceSize.width: width; sourceSize.height: height
            source: original.source
            color: "#a3161f"
        }
    }
    function test_theme_tint_preserves_the_circle_cutouts_and_edges() {
        for (const size of [16, 32]) {
            original.width = size; original.height = size
            tinted.width = size; tinted.height = size
            tryCompare(original, "status", Image.Ready)
            tryCompare(tinted, "status", Image.Ready)
            waitForRendering(background)
            const picture = grabImage(background)
            compare(picture.red(0,0), 255)
            compare(picture.green(32,0), 255)
            const center = Math.floor(size / 2)
            compare(picture.red(center,Math.floor(size * .2)), 0)
            compare(picture.red(32 + center,Math.floor(size * .2)), 163)
            let holes = 0; let edges = 0
            for (let y = 0; y < size; ++y) for (let x = 0; x < size; ++x) {
                const shade = picture.red(x,y)
                verify(Math.abs(picture.red(x+32,y) - (163 + shade * 92 / 255)) <= 1)
                verify(Math.abs(picture.green(x+32,y) - (22 + shade * 233 / 255)) <= 1)
                if (shade > 0 && shade < 255) ++edges
                if (shade > 200 && x > size*.25 && x < size*.7 && y > size*.3 && y < size*.8) ++holes
            }
            verify(edges > 0)
            verify(holes > 0, "The waves must remain transparent inside the circle")
        }
    }
}
