// The tools ring: a second hairline and a switch at the end of the pill.
//
// Three things here have no other guard. The pill's markup is a template string, so a separator that
// is emitted in the wrong place is still valid JavaScript and still paints — it just puts the new
// ring inside the counts. The click branch is one line among several, and the day it stops
// dispatching `tool:` ids the ring silently starts refreshing a provider instead. And the switch
// itself must keep its promise of leaving no script behind: the version that wrote `.ps1` files into
// %TEMP% and ran them with `-ExecutionPolicy Bypass` was flagged by the machine's security suite,
// which is the whole reason this module was rewritten. Reading the source is the only test that can
// see a regression to that.
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, 'codenotch');
const notch = fs.readFileSync(path.join(root, 'ui', 'notch.html'), 'utf8');
const touch = fs.readFileSync(path.join(root, 'src', 'touch.rs'), 'utf8');

test('the tools ring sits behind its own hairline, after the counts', () => {
  // The template emits providers, then the counts, then a separator, then the ring. Pinned in that
  // order because the order is the design: the line is what keeps the switch out of the counts.
  const counts = notch.indexOf('bs.map((b,i)=>`<div class="cell" data-p="b:${b.id}"');
  const sep = notch.indexOf('+`<div class="sep"></div>');
  const tool = notch.indexOf('<div class="cell tool" data-p="tool:touch"');
  assert.ok(counts > 0, 'the counts are no longer rendered');
  assert.ok(sep > counts, 'the tools separator has to come after the counts');
  assert.ok(tool > sep, 'the tools ring has to come after its separator');
  // The counts' own separator is still the one before them, so a pill with no counts still has a
  // hairline in front of the ring rather than two in a row.
  assert.ok(notch.includes("+(bs.length?`<div class=\"sep\"></div>`:'')"), 'the counts lost their hairline');
});

test('the tools ring keeps a cell\'s own height', () => {
  // The pill's drag silhouette is worked out from the cells it holds, and a ring without its label
  // slot would be shorter than the others without anything saying so.
  const cell = notch.slice(notch.indexOf('<div class="cell tool"'), notch.indexOf('<div class="cell tool"') + 900);
  assert.match(cell, /<div class="pct"><\/div>/, 'the tool cell has no label slot');
  assert.match(cell, /<svg class="ring"/, 'the tool cell has no ring');
  assert.match(cell, /class="tool-slash"/, 'the tool cell cannot show the screen is off');
});

test('a tool id is dispatched to the switch, not to a ring', () => {
  assert.match(notch, /if\(press\.id\.indexOf\('tool:'\)===0\) runTool\(/, 'a click on the tools ring goes nowhere');
  // And hovering it must not open the card: that card names a provider, and there is none there.
  assert.match(notch, /id\.indexOf\('tool:'\)===0\)\{ hoverId=null;/, 'the card still opens over the switch');
});

test('the drop outline counts the hairlines too', () => {
  // Two children of the flex column that are not cells, each with a gap either side. Left out of the
  // shape, the outline offered for a landing is 29 px shorter per hairline than the notch.
  assert.match(notch, /const seps=pill\.querySelectorAll\('\.sep'\)\.length/);
  assert.match(notch, /seps\*\(2\*between\+1\)/);
});

test('the switch starts no script and no visible console', () => {
  // Comments are stripped first: this file documents the approach that was replaced, and the point of
  // the test is that nothing in the code goes back to it.
  const code = touch
    .split('\n')
    .filter((line) => !line.trimStart().startsWith('//'))
    .join('\n');
  for (const banned of ['powershell', 'ExecutionPolicy', 'Start-Process', '.ps1"']) {
    assert.ok(!code.toLowerCase().includes(banned.toLowerCase()), `touch.rs still mentions ${banned}`);
  }
  // The one program it does start is schtasks, and only hidden: a console would flash over the notch.
  assert.match(code, /Command::new\("schtasks\.exe"\)/);
  assert.match(code, /creation_flags\(0x0800_0000\)/, 'schtasks would open a console window');
  // And it reaches the device tree through the API Device Manager uses.
  assert.match(code, /CM_Disable_DevNode/);
  assert.match(code, /CM_Get_DevNode_Registry_PropertyW/);
});

test('nothing is written into the temporary directory but the task definition', () => {
  // The old version left read.ps1 / apply.ps1 / launch.ps1 there after every read and every switch.
  // The task XML is the only file this module may put in temp, and it is deleted right after.
  const writes = [...touch.matchAll(/std::fs::write\(/g)].length;
  assert.strictEqual(writes, 2, `expected two writes (the request file and the task xml), found ${writes}`);
  assert.match(touch, /let _ = std::fs::remove_file\(&path\);/, 'the task xml is left behind');
});
