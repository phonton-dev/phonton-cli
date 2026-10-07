// Smoke benchmark tasks. Each runs on a fresh git copy of a pinned fixture.
// `hidden` tests are written only after Phonton finishes, so the run never
// sees them; they decide acceptance.

export const TASKS = [
  {
    id: "todo-remove",
    fixture: "todo-api",
    goal: "Add a remove(id) method to TodoStore that deletes the todo and throws an Error with the message `no todo <id>` when the id does not exist.",
    hidden: {
      "test/hidden-remove.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { TodoStore } = require('../src/store');
const now = () => new Date('2026-06-01T12:00:00Z');
test('hidden: remove deletes an existing todo', () => {
  const s = new TodoStore(now);
  const a = s.create({ title: 'a' });
  s.create({ title: 'b' });
  s.remove(a.id);
  assert.deepStrictEqual(s.list({ includeDone: true }).map((t) => t.title), ['b']);
});
test('hidden: remove rejects unknown ids', () => {
  const s = new TodoStore(now);
  assert.throws(() => s.remove(99), /no todo 99/);
});
`,
    },
  },
  {
    id: "todo-count",
    fixture: "todo-api",
    goal: "Add a count() method to TodoStore that returns how many todos are not done.",
    hidden: {
      "test/hidden-count.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { TodoStore } = require('../src/store');
const now = () => new Date('2026-06-01T12:00:00Z');
test('hidden: count ignores done todos', () => {
  const s = new TodoStore(now);
  const a = s.create({ title: 'a' });
  s.create({ title: 'b' });
  s.create({ title: 'c' });
  s.complete(a.id);
  assert.strictEqual(s.count(), 2);
});
test('hidden: count of an empty store is zero', () => {
  assert.strictEqual(new TodoStore(now).count(), 0);
});
`,
    },
  },
  {
    id: "todo-sort",
    fixture: "todo-api",
    goal: "Make formatList list todos that have a due date first, earliest due date first, followed by todos without a due date in their original order. Keep the existing line format.",
    hidden: {
      "test/hidden-sort.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { formatList } = require('../src/format');
test('hidden: due todos first, earliest first, undated keep order', () => {
  const todos = [
    { id: 1, title: 'none-a', done: false, due: null },
    { id: 2, title: 'late', done: false, due: '2026-07-01T00:00:00Z' },
    { id: 3, title: 'none-b', done: false, due: null },
    { id: 4, title: 'soon', done: false, due: '2026-06-02T00:00:00Z' },
  ];
  assert.deepStrictEqual(
    formatList(todos).split('\\n').map((l) => l.split(' ')[2]),
    ['soon', 'late', 'none-a', 'none-b']
  );
});
test('hidden: empty list message unchanged', () => {
  assert.strictEqual(formatList([]), 'nothing to do');
});
`,
    },
  },
  {
    id: "todo-title-limit",
    fixture: "todo-api",
    goal: "Reduce the maximum todo title length from 120 to 80 characters; titles longer than that must be rejected with the existing error message format.",
    hidden: {
      "test/hidden-title.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { validateTodo } = require('../src/validate');
test('hidden: 80 characters allowed, 81 rejected', () => {
  assert.deepStrictEqual(validateTodo({ title: 'x'.repeat(80) }), []);
  assert.deepStrictEqual(validateTodo({ title: 'x'.repeat(81) }), ['title must be at most 80 characters']);
});
`,
    },
  },
];
