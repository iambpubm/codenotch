// Run with node --test test-card-i18n-coverage.cjs from windows/.
//
// The card and the settings page translate by lookup, and a lookup that misses falls back to English
// rather than throwing. That is the right behaviour at runtime and the wrong thing to discover at
// runtime: a language table missing one key leaves one line of a Chinese card in English, and nothing
// anywhere reports it. These tests are the report.
//
// The balance line is the sharper case. `leftCopy` calls `ui().leftOf`, which the language table has
// to provide as a function — a table without it is not a missing translation but a thrown exception
// the moment a WorkBuddy card with a remainder is opened.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const notch = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');
const settings = readFileSync(join(__dirname, 'codenotch/ui/settings.html'), 'utf8');

// The language codes the card carries tables for. English is the fallback and has no TEXT or PATTERNS
// table of its own — it is the untranslated original.
const CARD_LANGS = ['ko', "'pt-BR'", 'uk', 'ru', 'zh', "'zh-Hant'"];
// The UI table adds en, which is the one language that must have every helper.
const UI_LANGS = ['en'].concat(CARD_LANGS);

function block(document, anchor, endMarker = '},') {
  const at = document.indexOf(anchor);
  assert.ok(at > 0, `no table for ${anchor.trim()}`);
  const stop = document.indexOf(endMarker, at + anchor.length);
  assert.ok(stop > at, `${anchor.trim()} has no end`);
  return document.slice(at, stop);
}

test('Every card language can word the balance line', () => {
  for (const lang of UI_LANGS) {
    const table = block(notch, `\n  ${lang}:{locale:`);
    assert.ok(/leftOf:/.test(table), `the ${lang} UI table has no leftOf`);
  }
});

test('Every card language can name the spend breakdown', () => {
  for (const lang of CARD_LANGS) {
    const table = block(notch, `\n  ${lang}:{\n`, '\n  },');
    for (const key of ["'Estimated spend'", "'Cache-hit input'", "'Cache-miss input'", "'Output'"]) {
      assert.ok(table.includes(key), `the ${lang} TEXT table has no ${key}`);
    }
    assert.ok(table.includes("Estimated from this machine"), `the ${lang} TEXT table has no rate note`);
    assert.ok(table.includes("WorkBuddy refused the pasted credential"), `the ${lang} TEXT table has no refusal notice`);
  }
});

test('Every card language can say which models were left out of the money', () => {
  for (const lang of CARD_LANGS) {
    const table = block(notch, `\n  ${lang}:[\n`, '\n  ],');
    assert.ok(/\[\/\^Not priced: \(\.\+\)\$\//.test(table), `the ${lang} PATTERNS table has no Not priced rule`);
  }
});

test('Every settings language can label the credential row', () => {
  const names = ['PT_BR', 'RU', 'ZH', 'ZH_HANT', 'JA', 'KO', 'UK'];
  for (const name of names) {
    const table = block(settings, `const ${name}_STATIC = {\n`, '\n};\n');
    for (const key of ["'WorkBuddy credential'", "'Paste the accessToken value'", "'Enterprise id (optional)'",
      "'Save'", "'Remove'", "'Credential saved", "'Paste a credential to read the Credits balance'"]) {
      assert.ok(table.includes(key), `the ${name} table has no ${key}`);
    }
  }
});

test('The card draws money only where a window carries it', () => {
  // A window with no cost must contribute nothing to the row: an unstyled or zero figure beside every
  // count on every provider is worse than the empty space it replaces.
  assert.ok(/function costCopy\(w,snap\)\{\s*\n\s*if\(w\.cost==null\) return '';/.test(notch),
    'costCopy must be silent without a cost');
  assert.ok(/function leftCopy\(w\)\{\s*\n\s*if\(w\.remaining==null\) return '';/.test(notch),
    'leftCopy must be silent without a remainder');
});

test('The breakdown is summed from the rows above it', () => {
  // The total on the card is computed from the same array that is printed, so a figure can never
  // disagree with the rows under it.
  assert.ok(/const total=parts\.reduce\(\(sum,p\)=>sum\+Number\(p\.cost\|\|0\),0\);/.test(notch),
    'the breakdown total must be a sum of its own rows');
  assert.ok(/const parts=\(snap\.cost\.parts\|\|\[\]\)\.filter\(p=>p\.tokens>0\);/.test(notch),
    'the breakdown must filter out the kinds with no tokens');
});

test('Every card language can name the balance window it draws', () => {
  // DeepSeek's balance arrives as a window labelled "Balance". A table without the key would print
  // the English word in the middle of an otherwise translated card, and nothing reports it.
  for (const lang of CARD_LANGS) {
    const table = block(notch, `\n  ${lang}:{\n`, '\n  },');
    assert.ok(table.includes("'Balance'"), `the ${lang} TEXT table has no 'Balance'`);
  }
});

test('Every settings language can label the DeepSeek key row', () => {
  const names = ['PT_BR', 'RU', 'ZH', 'ZH_HANT', 'JA', 'KO', 'UK'];
  for (const name of names) {
    const table = block(settings, `const ${name}_STATIC = {\n`, '\n};\n');
    for (const key of ["'DeepSeek API key'", "'Paste the API key'", "'Paid in so far (optional)'",
      "'Key saved", "'Key read from the DeepSeek Harness", "'Key read from DEEPSEEK_API_KEY",
      "'No DeepSeek key found", "'Paid in so far is optional", "'Paid in so far has to be a number'"]) {
      assert.ok(table.includes(key), `the ${name} table has no ${key}`);
    }
  }
});

test('A remainder in money is written as money', () => {
  // The line used to read `40.29 CNY left`, which is a number with a currency code after it. A sum of
  // money is written as one, so the currency never goes through the unit-word lookup.
  assert.ok(/if\(unit==='CNY'\|\|unit==='USD'\) return money\(w\.remaining,unit\);/.test(notch),
    'a money remainder must go through money() rather than the unit text');
});

test('A window with no denominator draws no ring', () => {
  // The balance case: a 0 % arc over a full account is a picture of something untrue rather than a
  // missing picture, so the card gives it a line and leaves the ring alone — in the pill too, and in
  // the weekly selector, which must not mistake it for a weekly window.
  assert.ok(/if\(w\.unmetered\)\{/.test(notch), 'the card must branch on unmetered');
  assert.ok(/else if\(h && h\.unmetered\) pct\.textContent/.test(notch),
    'the pill must not print a percentage for a window that has none');
  assert.ok(/filter\(w=>w\.count==null&&!w\.unmetered\)/.test(notch),
    'an unmetered window must not be read as a weekly one');
});
