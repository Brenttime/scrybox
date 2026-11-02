import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { PHONE_QUERY, SIDE_RAIL_QUERY, FOLD_SPREAD_QUERY, layoutViewportWidth } from './phoneLayout.js';

const fakeWin = (innerWidth, phone) => ({ innerWidth, matchMedia: () => ({ matches: phone }) });

test('phone layout caps the layout width at 768', () => {
  assert.equal(layoutViewportWidth(fakeWin(890, true)), 768);
  assert.equal(layoutViewportWidth(fakeWin(466, true)), 466);
  assert.equal(layoutViewportWidth(fakeWin(890, false)), 890);
});

test('CSS phone/desktop media queries match PHONE_QUERY', () => {
  const css = readFileSync(new URL('../index.css', import.meta.url), 'utf8');
  assert.ok(css.includes(`@media ${PHONE_QUERY}`));
  assert.ok(!/@media \(max-width: 768px\) \{/.test(css), 'bare 768 query left over');
  assert.ok(!/@media \(min-width: 769px\) \{/.test(css), 'bare 769 query left over');
});

test('iPhone Duo side-rail and fold-spread queries match the theme CSS', () => {
  const css = readFileSync(new URL('../theme-manabox.css', import.meta.url), 'utf8').replace(/\s+/g, ' ');
  assert.ok(css.includes(`@media ${SIDE_RAIL_QUERY.replace(/, /g, ', ')} {`.replace(/\s+/g, ' ')), 'side rail query drifted');
  assert.ok(css.includes(`@media ${FOLD_SPREAD_QUERY} {`), 'fold spread query drifted');
});
