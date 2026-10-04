// Exercise the installed script against KWin's actual duplicate-name behavior.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync('crates/bosn-service/src/bin/bosn/widget/placement.js', 'utf8');
function signal() {
    const callbacks = [];
    return {connect: callback => callbacks.push(callback), emit: () => callbacks.forEach(callback => callback())};
}
function verify(initialMarker, mode = 'ordinary') {
    let present = initialMarker;
    let timer;
    const transitions = [];
    let pendingDelete = false;
    let mutations = 0;
    const warnings = [];
    function QTimer() {
        timer = this;
        this.timeout = signal();
        this.start = () => {};
        this.stop = () => { this.stopped = true; };
    }
    function callDBus(...args) {
        const callback = args.at(-1);
        let result;
        switch (args[3]) {
        case 'isScriptLoaded':
            if (mode === 'missing-callback') return;
            result = mode === 'invalid-query' ? 'true' : present;
            break;
        case 'loadScript':
            mutations++;
            result = mode === 'rejected-load' || present ? -1 : 3;
            if (result >= 0) { present = true; transitions.push(present); }
            break;
        case 'unloadScript':
            mutations++;
            result = mode === 'rejected-unload' ? false : present;
            if (result) {
                if (mode === 'deferred-delete') pendingDelete = true;
                else { present = false; transitions.push(present); }
            }
            break;
        default: throw new Error('unexpected compositor method');
        }
        if (typeof callback === 'function') callback(result);
    }
    const workspace = {
        windowList: () => [], windowAdded: signal(), windowRemoved: signal(),
        screensChanged: signal(), virtualScreenGeometryChanged: signal()
    };
    vm.runInNewContext(script, {QTimer, callDBus, workspace, console: {warn: message => warnings.push(message)}, KWin: {MaximizeArea: 1}});
    const ticks = mode === 'missing-callback' ? 12 : 3;
    for (let tick = 0; tick < ticks && !timer.stopped; tick++) {
        // KWin unloadScript schedules deleteLater; completion is not removal.
        if (tick === 2 && pendingDelete) { present = false; transitions.push(present); }
        timer.timeout.emit();
    }
    if (mode === 'invalid-query' || mode === 'rejected-load' || mode === 'rejected-unload') {
        assert.equal(timer.stopped, true, 'invalid compositor replies must fail closed');
        assert.deepEqual(transitions, []);
        assert.equal(mutations, mode === 'invalid-query' ? 0 : 1);
        assert.equal(warnings.length, 1, 'failure must emit one actionable diagnostic');
        assert.match(warnings[0], /bosn widget install/);
        return;
    }
    if (mode === 'missing-callback') {
        assert.equal(mutations, 0, 'pending requests must not manufacture readiness transitions');
        assert.deepEqual(transitions, []);
        assert.equal(warnings.length, 1, 'a missing callback must produce a bounded diagnostic');
        assert.match(warnings[0], /pending.*bosn widget install/);
        return;
    }
    assert.equal(timer.stopped, undefined, 'existing readiness marker must not permanently stop the placement script');
    assert.deepEqual(warnings, [], 'healthy readiness stays quiet');
    assert.deepEqual(transitions, mode === 'deferred-delete' ? [false, true] : initialMarker ? [false, true, false] : [true, false, true]);
}
verify(false);
verify(true);
verify(true, 'deferred-delete');
verify(false, 'invalid-query');
verify(false, 'missing-callback');
verify(false, 'rejected-load');
verify(true, 'rejected-unload');
console.log('widget readiness: fresh/stale markers, deferred deletion and invalid/pending replies passed');
