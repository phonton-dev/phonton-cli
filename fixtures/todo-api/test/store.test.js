const test = require('node:test');
const assert = require('node:assert');
const { TodoStore } = require('../src/store');

const fixedNow = () => new Date('2026-06-01T12:00:00Z');

test('creates and lists todos', () => {
  const s = new TodoStore(fixedNow);
  s.create({ title: ' write docs ' });
  assert.deepStrictEqual(s.list().map((t) => t.title), ['write docs']);
});

test('rejects empty titles', () => {
  const s = new TodoStore(fixedNow);
  assert.throws(() => s.create({ title: '  ' }), /invalid todo/);
});

test('rejects due dates in the past', () => {
  const s = new TodoStore(fixedNow);
  assert.throws(() => s.create({ title: 'late', due: '2026-05-31T00:00:00Z' }), (err) => {
    assert.deepStrictEqual(err.details, ['due must not be in the past']);
    return true;
  });
});

test('accepts due dates in the future', () => {
  const s = new TodoStore(fixedNow);
  const t = s.create({ title: 'soon', due: '2026-06-02T00:00:00Z' });
  assert.strictEqual(t.due, '2026-06-02T00:00:00Z');
});
