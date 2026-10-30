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
      "'Save'", "'Remove'", "'Credential saved",
      // The row now offers to take the credential out of the running app instead of asking a person
      // to go and find one. A table missing these prints English in the middle of a translated pane.
      "'Paste a credential, or read the one WorkBuddy is using",
      "'Read from WorkBuddy'", "'Reading", "'Reads the credential the running WorkBuddy app is using",
      "'Nothing to read"]) {
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

test('Every card language can word the to-do card', () => {
  // 这张卡是新拼的，它用到的每个 ui() 键都得在七张表里，否则中文界面里会露出半句英文，
  // 而运行时只是「查不到就退回英文」，永远不会报错。
  const keys = ['todoRing', 'todoTitle', 'todoNone', 'todoLocal', 'todoRemote',
    'todoNoDate', 'todoDone', 'todoLeft', 'todoLate', 'units'];
  for (const lang of UI_LANGS) {
    const table = block(notch, `\n  ${lang}:{locale:`);
    for (const key of keys) {
      assert.ok(new RegExp('(^|[,\\s])' + key + ':').test(table), `the ${lang} UI table has no ${key}`);
    }
  }
});

test('Every settings language can word the to-do pane', () => {
  // 这一页的键更多，而且大多只在一个分支里用到（错误提示）。漏一条不会挂运行时，
  // 只会在中文设置页里冒出一句英文 —— 这条测试就是那份清单。
  const keys = ["'To-do'", "'The list'", "'Sort'", "'Due first'", "'Priority first'",
    "'Open the card as a deadline nears'", "'Add'", "'What needs doing?'", "'Due'", "'Priority'",
    "'None'", "'High'", "'Medium'", "'Low'", "'A to-do needs a title.'", "'On the notch'",
    "'No open to-dos.'", "'Delete'", "'Passphrase'",
    // 同步区：状态一句话就有十四种说法，漏一条只会在某种语言里冒出一句英文
    "'Remember it'", "'Forget it'", "'Sync now'", "'Last sync'", "'Not yet'",
    "'Remembered'", "'Not remembered'", "'Synced.'", "'Waiting to push.'", "'Syncing…'",
    "'Sync failed.'", "'No passphrase remembered, so nothing is synced.'",
    "'Could not reach the gateway. It will try again.'",
    "'The workbench returned a file this build cannot read, so nothing was changed.'",
    "'The sync stopped on this machine. Nothing was sent or changed.'",
    "'Remembered. It syncs on its own.'",
    "'The passphrase did not open the workbench.'", "'No CyberBrain account id is set.'",
    "'Type the passphrase first.'",
    "'No date'", "'Overdue by {t}'", "'Due in {t}'"];
  const names = ['PT_BR', 'RU', 'ZH', 'ZH_HANT', 'JA', 'KO', 'UK'];
  for (const name of names) {
    const table = block(settings, `const ${name}_STATIC = {\n`, '\n};\n');
    for (const key of keys) {
      // 键名可能用单引号也可能用双引号包着（两种写法在文件里都有先例），所以两种都认，
      // 认不出的才是真的漏了。
      const bare = key.slice(1, -1);
      assert.ok(table.includes("'" + bare + "':") || table.includes('"' + bare + '":'),
        `the ${name} table has no ${key}`);
    }
  }
});

test('The two sync captions are translated word for word', () => {
  // 这两句是这一页最长的英文，而且不走 ui()：applyStaticUi 是逐字匹配整个 text node 的，
  // 所以英文改了一个词、七张表没跟着改，整段就静默退回英文 —— 没有任何地方会报错。
  // 断言直接从 HTML 里取原文，而不是在测试里抄一份：抄的那份会和 HTML 一起变旧。
  const captions = [...settings.matchAll(/<div class="item cap">((?:The passphrase |While the passphrase )[^<]*)<\/div>/g)]
    .map(m => m[1].replace(/\s+/g, ' ').trim());
  assert.equal(captions.length, 2, 'the to-do pane should carry two sync captions');
  const names = ['PT_BR', 'RU', 'ZH', 'ZH_HANT', 'JA', 'KO', 'UK'];
  for (const name of names) {
    const table = block(settings, `const ${name}_STATIC = {\n`, '\n};\n');
    for (const text of captions) {
      assert.ok(table.includes('"' + text + '":'),
        `the ${name} table has no caption starting "${text.slice(0, 40)}"`);
    }
  }
});
