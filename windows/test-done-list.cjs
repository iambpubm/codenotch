// Run with node --test test-done-list.cjs from windows/.
//
// 已完成的列表是这一版新加的那一段，它要跨三处对名字：页面里的 `invoke`、Rust 侧的命令签名、
// 以及 `generate_handler!` 的登记表。三者任何一个写错都不是编译错误 —— 点下去什么都不发生，
// 而这恰好是这一版要修的那类 bug（云端待办点不动）。
//
// 另一半是「云端条目也能勾」这件事本身：卡片那一行以前带 `data-readonly`，点击在第一行就被
// 拦掉了。这条测试守住它不再回来。
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const settings = readFileSync(join(__dirname, 'codenotch/ui/settings.html'), 'utf8');
const notch = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');
const main = readFileSync(join(__dirname, 'codenotch/src/main.rs'), 'utf8');

function braced(src, open) {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) return src.slice(open, i + 1);
  }
  return '';
}
function bracketed(src, open) {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '[') depth++;
    else if (src[i] === ']' && --depth === 0) return src.slice(open, i + 1);
  }
  return '';
}
const handlerList = () => bracketed(main, main.indexOf('[', main.indexOf('generate_handler!')));

/** `fn name(...)` 的参数名，按顺序。`app` 是 Tauri 注入的，页面不传。 */
function signature(name) {
  const at = main.indexOf(`fn ${name}(`);
  assert.ok(at > 0, `${name} is gone from main.rs`);
  const open = main.indexOf('(', at);
  let depth = 0, end = open;
  for (let i = open; i < main.length; i++) {
    if (main[i] === '(') depth++;
    else if (main[i] === ')' && --depth === 0) { end = i; break; }
  }
  return [...main.slice(open + 1, end).matchAll(/(\w+)\s*:/g)]
    .map(m => m[1])
    .filter(n => n !== 'app');
}
/** `invoke('cmd', { … })` 的对象键。简写的 `{id, title}` 也算。
    按最外层逗号切段再取 `:` 前面那一串：正则数不清 `{id: b.dataset.undo}` 里的 `undo`
    是属性名而不是键，也数不清逗号被上一次匹配吃掉之后紧跟的那个简写键。 */
function callKeys(src, cmd) {
  // 卡片那一页包了一层 callq（失败会在屏幕上说出来），设置页直接用 invoke
  const m = new RegExp(`(?:invoke|callq)\\('${cmd}',\\s*\\{`).exec(src);
  assert.ok(m, `nothing calls ${cmd}`);
  const text = braced(src, m.index + m[0].length - 1);
  let depth = 0, buf = '';
  const out = [];
  const push = () => {
    const key = buf.trim().replace(/^\{/, '').split(':')[0].trim();
    if (/^\w+$/.test(key)) out.push(key);
    buf = '';
  };
  for (const ch of text) {
    if ('{[('.includes(ch)) depth++;
    else if ('}])'.includes(ch)) depth--;
    if (ch === ',' && depth === 1) { push(); continue; }
    buf += ch;
  }
  push();
  return new Set(out);
}
const sameSet = (a, b) => a.size === b.size && [...a].every(x => b.has(x));

test('The done list calls commands that exist and are registered', () => {
  const handler = handlerList();
  for (const cmd of ['set_todo_done', 'update_todo', 'delete_todo']) {
    assert.ok(new RegExp(`\\b${cmd}\\b`).test(handler), `${cmd} is not registered`);
  }
});

test('Every argument the page sends matches the command signature', () => {
  for (const [cmd, src] of [['set_todo_done', settings], ['update_todo', settings], ['toggle_todo', notch]]) {
    const want = new Set(signature(cmd));
    const got = callKeys(src, cmd);
    assert.ok(sameSet(want, got), `${cmd}: Rust wants ${[...want]}, the page sends ${[...got]}`);
  }
});

test('The done list is fed by doneRows, the field the view actually returns', () => {
  assert.ok(/doneRows/.test(settings), 'settings never reads doneRows');
  assert.ok(/pub done_rows: Vec<Row>/.test(readFileSync(join(__dirname, 'codenotch/src/todos.rs'), 'utf8')),
    'todos::View lost done_rows');
});

test('A card row from the cloud is clickable again', () => {
  assert.ok(!/data-readonly/.test(notch), 'the card still marks cloud rows read-only');
  assert.ok(!/dataset\.readonly/.test(notch), 'the click handler still returns early on cloud rows');
  assert.ok(/data-cloud/.test(notch), 'the cloud marker should stay, as a marker');
  // 「先落地再改」是后端的事：前端只发 id
  assert.ok(/callq\('toggle_todo',\{id:row\.dataset\.todo\}\)/.test(notch),
    'the card no longer toggles the row it was clicked on');
});

test('Every settings language words the done list', () => {
  const tables = ['PT_BR_STATIC', 'RU_STATIC', 'ZH_STATIC', 'ZH_HANT_STATIC', 'JA_STATIC', 'KO_STATIC', 'UK_STATIC'];
  const keys = ['Done', 'Nothing done yet.', 'Edit', 'Save', 'Cancel', 'Reopen', 'Title'];
  for (const name of tables) {
    const at = settings.indexOf(`const ${name} = {`);
    assert.ok(at > 0, `${name} is gone`);
    const table = braced(settings, settings.indexOf('{', at));
    for (const key of keys) {
      assert.ok(table.includes(`"${key}"`), `${name} has no "${key}"`);
    }
  }
});
