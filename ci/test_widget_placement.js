// Execute the real compositor script against monitor/dock geometry.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync('crates/bosn-service/src/bin/bosn/widget/placement.js', 'utf8');
let added;
let timer;
const calls=[];
function QTimer() { timer=this; this.timeout=signal(); this.start=()=>{}; this.stop=()=>{this.stopped=true;}; }
function callDBus(...args) { calls.push(args); const callback=args.at(-1); if(typeof callback === "function") callback(args[3] === "unloadScript" ? true : 3); }
function signal() { const callbacks=[]; return { connect:f=>callbacks.push(f), emit:()=>callbacks.forEach(f=>f()) }; }
const screensChanged=signal(), virtualScreenGeometryChanged=signal(), removed=signal();
const dockChanged=signal();
// Some Plasma versions omit dock visibility signals; geometry still works.
const dock={dock:true, frameGeometryChanged:dockChanged};
const bubble = { caption: 'bosn bubble', resourceClass: 'dev.bosn.widget', frameGeometry: {x:2667,y:761,width:72,height:72}, active:false, outputChanged:signal(), windowShown:signal() };
const panel = { caption:'bosn panel', resourceClass:'dev.bosn.widget', frameGeometry:{x:20,y:30,width:420,height:640}, active:false, outputChanged:signal(), windowShown:signal() };
const other = { caption:'bosn bubble', resourceClass:'other', frameGeometry:{x:7,y:8,width:72,height:72} };
let area = {x:1423,y:99,width:2560,height:1396};
vm.runInNewContext(script, { QTimer,callDBus,KWin:{MaximizeArea:1}, workspace:{windowList:()=>[bubble,panel,other,dock], clientArea:()=>area, windowAdded:{connect:f=>added=f},screensChanged,virtualScreenGeometryChanged,windowRemoved:removed} });
assert.equal(JSON.stringify(bubble.frameGeometry), JSON.stringify({x:3887,y:1399,width:72,height:72}));
assert.equal(bubble.active,false);
assert.equal(bubble.keepAbove,true);
assert.equal(panel.frameGeometry.x,3539);
assert.equal(panel.frameGeometry.y,831);
assert.equal(panel.active,false);
assert.equal(other.frameGeometry.x,7);
area = {x:-1600,y:0,width:1600,height:860};
const next = {...bubble,frameGeometry:{x:0,y:0,width:72,height:72}};
added(next);
assert.equal(next.frameGeometry.x,-96);
assert.equal(next.frameGeometry.y,764);
area={x:1423,y:99,width:2560,height:1300};
dockChanged.emit();
assert.equal(bubble.frameGeometry.y,1303);
area={x:1423,y:99,width:2560,height:1396};
removed.emit();
assert.equal(bubble.frameGeometry.y,1399);
area={x:0,y:0,width:1920,height:1080};
screensChanged.emit();
assert.equal(bubble.frameGeometry.x,1824);
assert.equal(bubble.frameGeometry.y,984);
area={x:0,y:0,width:1280,height:720};
virtualScreenGeometryChanged.emit();
assert.equal(bubble.frameGeometry.x,1184);
area={x:-1600,y:0,width:1600,height:860};
bubble.outputChanged.emit();
assert.equal(bubble.frameGeometry.x,-96);
console.log('widget placement: dock clearance, negative monitor origin, autoload, focus and window scope passed');

timer.timeout.emit();
assert.equal(calls.at(-1)[3], 'loadScript');
timer.timeout.emit();
assert.equal(calls.at(-1)[3], 'unloadScript');
// A compositor that rejects geometry must stop readiness transitions.
let rejected = bubble.frameGeometry;
Object.defineProperty(bubble, 'frameGeometry', {get:()=>rejected, set:()=>{}});
area={x:0,y:0,width:800,height:600};
timer.timeout.emit();
assert.equal(timer.stopped,true);
assert.equal(calls.at(-1)[3],'unloadScript');
