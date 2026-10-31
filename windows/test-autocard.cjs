// Run with node --test test-autocard.cjs from windows/.
//
// 「越临近截止，卡片自己撑开」这条规则同时被三方拉扯：待办每 20 秒重算一次、看门狗 300ms
// 判一次指针在不在、悬停折叠又有自己的 450ms 宽限。上一版三方各写各的，结果是那张待办卡
// 每 20 秒被弹出来、300ms 后被收掉 —— 屏幕上一闪一闪，而且条子从此不再自动隐藏。
//
// 这里不是在测一个纯函数，是在测**页面里真正跑的那段源码**：把 notch.html 里从 `const
// AUTO_LIFE=` 到 `applyAutoCard` 结束那一段整块抠出来，喂一个假 DOM 跑。重写一份等价的
// 逻辑来测是没有意义的 —— 会分叉的正是那两份。
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const notch = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');

function block() {
  const start = notch.indexOf('const AUTO_LIFE=');
  assert.ok(start > 0, 'no AUTO_LIFE block in notch.html');
  const end = notch.indexOf('// 「现在别自作主张」的几种情形', start);
  assert.ok(end > start, 'the auto-card block has no end marker');
  return notch.slice(start, end);
}

/** 一个假得刚刚好的页面环境：卡片在不在、弹过几次、被安排折叠几次。 */
function make() {
  let timerSeq = 0;
  const api = {
    card: {
      on: false,
      classList: {
        contains: () => api.card.on,
        add: () => { api.card.on = true; },
        remove: () => { api.card.on = false; }
      },
      style: { setProperty() {}, removeProperty() {} }
    },
    shown: 0,
    folds: 0,
    showCard() { api.shown += 1; api.card.on = true; },
    hideCard() { api.card.on = false; },
    scheduleFold() { api.folds += 1; },
    funcBusy: () => false,
    setTimeout: () => ++timerSeq,
    clearTimeout: () => {},
    todoAutoOpen: true,
    folded: false,
    carrying: false,
    dragging: false,
    menuOpen: false,
    hovered: null,
    todos: { rows: [], overdue: 0, expansion: 0.6 }
  };
  const run = new Function('api', `with(api){ ${block()};
return {applyAutoCard, dismissAuto, topTodoId, state:()=>({autoCard, autoKey, autoTimer})}; }`);
  api.api = run(api);
  return api;
}

/** 一次「重算」：真实页面里每 20 秒走一遍这个顺序。 */
function tick(env) { env.api.applyAutoCard(); }

test('a to-do opens the card once', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 3600, done: false }];
  tick(env);
  assert.equal(env.shown, 1, '该弹一次');
  assert.equal(env.api.state().autoCard, true);
});

test('recomputing every 20 seconds does not reopen it after the watchdog closed it', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 3600, done: false }];
  tick(env);
  assert.equal(env.shown, 1);
  // 看门狗说指针不在：这一版走 dismissAuto，把「已提示」一起记下
  env.api.dismissAuto();
  assert.equal(env.card.on, false, '卡片收了');
  assert.equal(env.api.state().autoCard, false, '标记也清了');
  for (let i = 0; i < 20; i++) tick(env);
  assert.equal(env.shown, 1, '同一件事不该再弹 —— 这就是原来每 20 秒闪一次的那个 bug');
});

test('a new, more urgent item gets its own one-time notice', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 3600, done: false }];
  tick(env);
  env.api.dismissAuto();
  env.todos.rows = [
    { id: 'a', left: 3600, done: false },
    { id: 'b', left: 600, done: false }
  ];
  tick(env);
  assert.equal(env.shown, 2, '换了更急的那条，允许再提示一次');
  assert.equal(env.api.topTodoId(), 'b', '挑的该是最急的那条');
});

test('a folded notch is not opened by itself', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 600, done: false }];
  env.folded = true;
  tick(env);
  assert.equal(env.shown, 0, '折叠着就别弹');
});

test('nothing urgent means no card at all', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 3600 * 24, done: false }];
  env.todos.expansion = 0;
  tick(env);
  assert.equal(env.shown, 0);
});

test('an item with no deadline never drives the card', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: null, done: false }];
  tick(env);
  assert.equal(env.api.topTodoId(), '', '没有截止时间的不算「最急」');
  assert.equal(env.shown, 0);
});

test('the card closes itself when nothing is urgent any more', () => {
  const env = make();
  env.todos.rows = [{ id: 'a', left: 600, done: false }];
  tick(env);
  assert.equal(env.shown, 1);
  env.todos.rows = [];
  env.todos.expansion = 0;
  tick(env);
  assert.equal(env.card.on, false, '不急了就收');
  assert.equal(env.api.state().autoCard, false);
});

// 接线：状态机再对，只要有一条收起路径没接上 dismissAuto，标记就会残留、下一轮又弹。
test('every collapse path goes through dismissAuto', () => {
  const paths = [
    ["listen('pointer_left'", 'the watchdog'],
    ["document.addEventListener('mouseout'", 'the pointer leaving the page'],
    ['function setFolded(f)', 'folding the notch']
  ];
  for (const [anchor, what] of paths) {
    const at = notch.indexOf(anchor);
    assert.ok(at > 0, `${what} is gone from notch.html`);
    const tail = notch.slice(at, at + 900);
    assert.ok(/dismissAuto\(\)/.test(tail), `${what} does not call dismissAuto()`);
  }
  // 自弹的那张还在屏上时不折叠，否则「折叠收卡片 → 下一轮又弹」又是一个循环
  assert.ok(/function foldAllowed\(\)\{ return onHover&&!pointerIn&&!autoCard/.test(notch),
    'foldAllowed does not stay folded while the auto card is up');
});
