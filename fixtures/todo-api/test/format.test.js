const test = require('node:test');
const assert = require('node:assert');
const { formatTodo, formatList } = require('../src/format');

test('formats a todo', () => {
  assert.strictEqual(formatTodo({ id: 1, title: 'a', done: false, due: null }), '[ ] #1 a');
});

test('formats an empty list', () => {
  assert.strictEqual(formatList([]), 'nothing to do');
});
