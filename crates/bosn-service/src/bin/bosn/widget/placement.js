// KDE Wayland owns window placement. Use the bubble's own monitor and usable
// area, so panels/docks and negative monitor origins are respected.
function placeBubble(window) {
    if (window.resourceClass !== "dev.bosn.widget" || window.caption !== "bosn bubble") return;
    const area = workspace.clientArea(KWin.MaximizeArea, window);
    const geometry = window.frameGeometry;
    const margin = 24;
    window.keepAbove = true;
    window.skipTaskbar = true;
    window.skipPager = true;
    window.skipSwitcher = true;
    window.frameGeometry = {
        x: Math.max(area.x, area.x + area.width - geometry.width - margin),
        y: Math.max(area.y, area.y + area.height - geometry.height - margin),
        width: geometry.width,
        height: geometry.height
    };
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
    } else if (window.resourceClass === "dev.bosn.widget" && window.caption === "bosn bubble") {
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
