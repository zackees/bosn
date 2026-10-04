// KDE Wayland owns window placement. Use the bubble's own monitor and usable
// area, so panels/docks and negative monitor origins are respected.
function isStatusWindow(window) {
    return window.resourceClass === "dev.bosn.widget" &&
        (window.caption === "bosn bubble" || window.caption === "bosn panel");
}
function placeBubble(window) {
    if (!isStatusWindow(window)) return;
    const area = workspace.clientArea(KWin.MaximizeArea, window);
    const geometry = window.frameGeometry;
    const margin = 24;
    window.keepAbove = true;
    window.skipTaskbar = true;
    window.skipPager = true;
    window.skipSwitcher = true;
    const expected = {
        x: Math.max(area.x, area.x + area.width - geometry.width - margin),
        y: Math.max(area.y, area.y + area.height - geometry.height - margin),
        width: geometry.width,
        height: geometry.height
    };
    window.frameGeometry = expected;
    const actual = window.frameGeometry;
    if (actual.x !== expected.x || actual.y !== expected.y ||
        actual.width !== expected.width || actual.height !== expected.height) {
        throw new Error("Bosn dock placement was not applied");
    }
    // Never activate the bubble: the panel/full view alone accept user focus.
}

function placeAll() {
    workspace.windowList().forEach(placeBubble);
}
function connectIfAvailable(signal, callback) {
    if (signal) signal.connect(callback);
}
function watch(window) {
    placeBubble(window);
    if (window.dock) {
        // Panel resizing, moving, appearing or hiding changes usable geometry.
        window.frameGeometryChanged.connect(placeAll);
        connectIfAvailable(window.windowShown, placeAll);
        connectIfAvailable(window.windowHidden, placeAll);
    } else if (isStatusWindow(window)) {
        window.outputChanged.connect(function () { placeBubble(window); });
        connectIfAvailable(window.windowShown, function () { placeBubble(window); });
    }
    placeAll();
}
workspace.windowList().forEach(watch);
workspace.windowAdded.connect(watch);
workspace.windowRemoved.connect(placeAll);
workspace.screensChanged.connect(placeAll);
workspace.virtualScreenGeometryChanged.connect(placeAll);

// A fresh marker transition proves this loaded script actually executes its
// placement handler and reads the resulting geometry. Never run the marker:
// it is an empty, compositor-owned script object used only for readiness.
const marker = "bosn-widget-corner-ready-v1";
let markerPresent = false;
let pulsePending = false;
const health = new QTimer();
health.interval = 500;
health.timeout.connect(function () {
    if (pulsePending) return;
    try { placeAll(); } catch (error) {
        health.stop();
        callDBus("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", marker);
        return;
    }
    pulsePending = true;
    function complete(result) {
        const accepted = markerPresent ? result === true : typeof result === "number" && result >= 0;
        if (!accepted) { health.stop(); return; }
        markerPresent = !markerPresent;
        pulsePending = false;
    }
    if (markerPresent) {
        callDBus("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", marker, complete);
    } else {
        callDBus("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "loadScript", "/dev/null", marker, complete);
    }
});
health.start();
