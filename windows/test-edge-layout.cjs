// Run with node --test test-edge-layout.cjs from windows/.
//
// The notch is drawn for the right-hand edge and mirrored or turned onto the other three. Nothing
// checks that turn: Rust only positions the window, and a rule written for one orientation and
// forgotten in the other still parses, still builds, and still ships — it just draws something
// wrong. That is what the hairline between the providers and the counts did on the top edge: the
// pill lies flat and becomes a row, the hairline stayed a 38 px horizontal dash across it.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const notch = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');

function rule(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  // No line anchor: `top` and `bottom` usually share one block
  // (`body[data-edge="top"] .sep,body[data-edge="bottom"] .sep{…}`), so the second one is never at
  // the start of a line.
  const match = notch.match(new RegExp(`${escaped}[^{]*\\{([\\s\\S]*?)\\}`));
  assert.ok(match, `missing the ${selector} rule`);
  return match[1];
}

test('the hairline between the providers and the counts turns with the pill', () => {
  // Upright edges: the pill is a column, so the hairline is a rule across its width.
  const upright = rule('.sep');
  assert.match(upright, /width:calc\(var\(--ring\) - 14px\)/, 'the hairline spans the ring');
  assert.match(upright, /height:1px/, 'and is one pixel tall');

  // Flat edges: the same hairline has to stand up, and give up the same 14 px, or the business
  // group gets a different amount of air depending on which edge the notch is on.
  for (const edge of ['top', 'bottom']) {
    const flat = rule(`body[data-edge="${edge}"] .sep`);
    assert.match(flat, /width:1px/, `${edge}: the hairline is still a horizontal rule`);
    assert.match(flat, /height:calc\(var\(--ring\) - 14px\)/, `${edge}: the hairline lost its length`);
  }
});

test('a flat pill is a row, which is why the hairline has to turn', () => {
  const flat = notch.match(
    /body\[data-edge="top"\] #pill,body\[data-edge="bottom"\] #pill\{([\s\S]*?)\}/,
  );
  assert.ok(flat, 'the flat pill lost its own rule');
  assert.match(flat[1], /flex-direction:row/, 'the flat pill stopped being a row');
  assert.match(flat[1], /width:auto/, 'its thickness comes from the cells, not a fixed column');
});

test('every orientation-dependent piece of chrome has a rule for both flat edges', () => {
  // Each of these is a shape on the bar that cannot be right on the top and bottom at once: the
  // outline the folded pill leaves behind, the status rail inside it, and the hairline. A missing
  // one of these is a shape pointing the wrong way, with no error anywhere.
  for (const selector of ['#rest', '#rail', '.sep']) {
    for (const edge of ['top', 'bottom']) {
      const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const re = new RegExp(`body\\[data-edge="${edge}"\\][^{,]*${escaped}\\s*[,{]`);
      assert.ok(re.test(notch), `${selector} has no rule for the ${edge} edge`);
    }
  }
});
