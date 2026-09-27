// Run with node --test test-count-format.cjs from windows/.
//
// The reading under each ring sits in a pill whose body is a fixed 70 px (`#pill`); the ring inside
// it is 44. That reading used to be the raw count, so a seven-figure token total measured wider than
// the body and the edge of the pill simply cut it off. `compactCount` is the fix, and these are the
// rules it has to keep.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const html = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');
const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)];
for (const [, source] of scripts) new vm.Script(source);

function markedSource(document, name) {
  const begin = `// BEGIN TESTABLE ${name} SELECTOR`;
  const end = `// END TESTABLE ${name} SELECTOR`;
  assert.equal(document.split(begin).length, 2, `exactly one ${name} begin marker`);
  assert.equal(document.split(end).length, 2, `exactly one ${name} end marker`);
  const start = document.indexOf(begin) + begin.length;
  const stop = document.indexOf(end);
  assert.ok(stop > start, `${name} markers are ordered`);
  return document.slice(start, stop);
}
const context = vm.createContext({});
vm.runInContext(markedSource(html, 'COUNT FORMAT'), context);
const { compactCount, thousands } = context;
const pill = (n) => '~' + compactCount(n);

test('Counts below a thousand are printed whole', () => {
  assert.equal(pill(0), '~0');
  assert.equal(pill(1), '~1');
  assert.equal(pill(42), '~42');
  assert.equal(pill(999), '~999');
});

test('A reading never grows past five characters, whatever magnitude it reaches', () => {
  // The case that started this: seven bare digits under the ring, clipped by the pill's own edge.
  assert.equal(pill(1583864), '~1.58M');
  const magnitudes = [0, 1, 42, 999, 1000, 1583, 9999, 15838, 99999, 999999, 1000000,
    1583864, 15838640, 158386400, 999999999, 1583864000, 999999999999];
  for (const n of magnitudes) {
    assert.ok(compactCount(n).length <= 5, `${n} prints as ${compactCount(n)}, past five characters`);
  }
});

test('Every scale keeps three significant figures', () => {
  assert.equal(pill(1000), '~1K');
  assert.equal(pill(1583), '~1.58K');
  assert.equal(pill(15838), '~15.8K');
  assert.equal(pill(99999), '~100K');
  assert.equal(pill(15838640), '~15.8M');
  assert.equal(pill(158386400), '~158M');
});

test('Rounding at a scale boundary promotes, never 1000 of the smaller unit', () => {
  // 999 999 is 999.999K; three figures round that to 1000K, which is not a figure anyone writes.
  assert.equal(pill(999999), '~1M');
  assert.equal(pill(999999999), '~1G');
  assert.equal(pill(999999999999), '~1T');
});

test('The card spells the count out in full, grouped', () => {
  assert.equal(thousands(1583864), '1,583,864');
  assert.equal(thousands(999), '999');
  assert.equal(thousands(0), '0');
});

test('The tidied reading still fits the pill it is printed in', () => {
  // A guard on the two numbers that made the bug. If the body narrows or the reading's type grows,
  // `compactCount` has to be revisited — rather than letting the text be clipped again.
  const body = Number(html.match(/#pill\{[^}]*?width:(\d+)px/)[1]);
  const size = Number(html.match(/\.pct\{font-size:(\d+)px/)[1]);
  const widest = Math.max(...[999999, 15838640, 158386400].map((n) => pill(n).length));
  // 0.62 em per character: tabular figures at this weight measure about 8.9 px when set at 15 px.
  const needed = widest * size * 0.62;
  assert.ok(needed < body,
    `widest reading is ${widest} chars (${needed.toFixed(0)}px) but the pill body is ${body}px`);
});
