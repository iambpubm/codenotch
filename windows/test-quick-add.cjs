// Run with node --test test-quick-add.cjs from windows/.
//
// The plus on the to-do card opens a second window, and everything it does is a name that only
// exists at runtime: `invoke('close_quick')`, `invoke('add_todo')`, the language code it asks for.
// A typo in any of those is not a compile error — the page's catch swallows it and the window sits
// there doing nothing, which is the hardest possible bug to report. These tests read the pages and
// the handler list and check the names agree.
//
// The other half is the language table. A missing key falls back to English silently, so a Russian
// window with one English chip looks like a design choice rather than a gap.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const quick = readFileSync(join(__dirname, 'codenotch/ui/quick.html'), 'utf8');
const notch = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');
const main = readFileSync(join(__dirname, 'codenotch/src/main.rs'), 'utf8');

/// Everything between a `{` and the `}` that closes it, so a table can be read without a parser.
function braced(src, open) {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) return src.slice(open, i + 1);
  }
  return '';
}

/// The same for `generate_handler![ … ]`, which is a bracket list rather than an object.
function bracketed(src, open) {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '[') depth++;
    else if (src[i] === ']' && --depth === 0) return src.slice(open, i + 1);
  }
  return '';
}

const handlerList = () => bracketed(main, main.indexOf('[', main.indexOf('generate_handler!')));

/// The quick window's text tables, keyed by language code.
function textTables() {
  const at = quick.indexOf('const TXT = {');
  assert.ok(at > 0, 'quick.html has no TXT table');
  const whole = braced(quick, quick.indexOf('{', at));
  const out = new Map();
  const re = /\n\s{2}'?([\w-]+)'?\s*:\s*\{/g;
  let m;
  while ((m = re.exec(whole))) {
    out.set(m[1], braced(whole, m.index + m[0].length - 1));
  }
  return out;
}

const keysOf = table => {
  const keys = new Set();
  for (const m of table.matchAll(/(\w+)\s*:/g)) keys.add(m[1]);
  return keys;
};

test('Every quick-add language carries every key, and none of them is blank', () => {
  const tables = textTables();
  assert.ok(tables.size >= 8, `expected eight languages, found ${tables.size}`);
  const english = keysOf(tables.get('en'));
  assert.ok(english.size >= 12, 'the English table lost its keys');
  for (const [lang, table] of tables) {
    const keys = keysOf(table);
    for (const key of english) assert.ok(keys.has(key), `${lang} has no "${key}"`);
    for (const key of keys) assert.ok(english.has(key), `${lang} has an extra "${key}"`);
  }
  // A key present but empty reads as a blank button, which looks like a broken layout
  for (const [lang, table] of tables) {
    for (const m of table.matchAll(/(\w+)\s*:\s*'([^']*)'/g)) {
      assert.ok(m[2].trim().length > 0, `${lang}.${m[1]} is empty`);
    }
  }
});

test('Every command the quick window calls is registered', () => {
  const handler = handlerList();
  const called = [...quick.matchAll(/invoke\('(\w+)'/g)].map(m => m[1]);
  assert.ok(called.includes('close_quick'), 'the quick window never closes itself');
  assert.ok(called.includes('add_todo'), 'the quick window never writes anything');
  for (const name of called) {
    assert.ok(new RegExp(`\\b${name}\\b`).test(handler), `${name} is called but not registered`);
  }
});

test('The plus exists on the to-do card and opens the window', () => {
  assert.ok(/class="t-add"/.test(notch), 'the to-do card has no plus button');
  assert.ok(/open_quick_add/.test(notch), 'the plus does not open anything');
  const handler = handlerList();
  assert.ok(/\bopen_quick_add\b/.test(handler), 'open_quick_add is not registered');
  assert.ok(/\bclose_quick\b/.test(handler), 'close_quick is not registered');
});

test('The plus is labelled in every language the card speaks', () => {
  // Seven UI tables, seven labels: an English-only label would sit in a fully translated card
  const labels = notch.match(/todoAdd:'[^']*'/g) || [];
  assert.equal(labels.length, 7, `expected seven todoAdd labels, found ${labels.length}`);
});
