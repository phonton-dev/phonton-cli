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
    formatList(todos).split('\\n').map((l) => l.split(' ')[3]),
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
  // ledger: one 339-line module, so edits land mid-file.
  {
    id: "ledger-thousands",
    fixture: "ledger",
    goal: "Add an optional second argument to formatAmount: with `{ grouped: true }` it groups thousands with commas, e.g. 1234567.5 becomes `1,234,567.50` and -1200 becomes `-1,200.00`. Without it the output is unchanged.",
    hidden: {
      "test/hidden-thousands.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { formatAmount } = require('../src/ledger');
test('hidden: grouped thousands', () => {
  assert.strictEqual(formatAmount(1234567.5, { grouped: true }), '1,234,567.50');
  assert.strictEqual(formatAmount(-1200, { grouped: true }), '-1,200.00');
  assert.strictEqual(formatAmount(999.999, { grouped: true }), '1,000.00');
  assert.strictEqual(formatAmount(12.5, { grouped: true }), '12.50');
});
test('hidden: default output unchanged', () => {
  assert.strictEqual(formatAmount(1234567.5), '1234567.50');
});
`,
    },
  },
  {
    id: "ledger-parens",
    fixture: "ledger",
    goal: "Let journal amounts use accounting-style negatives in parentheses: `(12.50)` means -12.50. A sign inside the parentheses, like `(-3)`, is a bad amount. Other amount rules stay the same.",
    hidden: {
      "test/hidden-parens.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { parseAmount, Ledger } = require('../src/ledger');
test('hidden: parenthesized negatives', () => {
  assert.strictEqual(parseAmount('(12.50)'), -12.5);
  assert.strictEqual(parseAmount('7'), 7);
  assert.throws(() => parseAmount('(-3)'));
  assert.throws(() => parseAmount('(3'));
  const l = Ledger.fromJournal('2026-06-01 Tea\\n  expenses:food  4\\n  assets:checking  (4)\\n');
  assert.strictEqual(l.balance('assets'), -4);
});
`,
    },
  },
  {
    id: "ledger-balance-on",
    fixture: "ledger",
    goal: "Add a balanceOn(account, date) method to Ledger that works like balance(account) but counts only transactions dated on or before `date` (YYYY-MM-DD).",
    hidden: {
      "test/hidden-balance-on.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { Ledger } = require('../src/ledger');
const J = '2026-06-01 A\\n  expenses:food  10\\n  assets:checking  -10\\n\\n2026-06-05 B\\n  expenses:food:out  5\\n  assets:checking  -5\\n';
test('hidden: balanceOn cuts off by date, inclusive', () => {
  const l = Ledger.fromJournal(J);
  assert.strictEqual(l.balanceOn('expenses', '2026-05-31'), 0);
  assert.strictEqual(l.balanceOn('expenses', '2026-06-01'), 10);
  assert.strictEqual(l.balanceOn('expenses', '2026-06-05'), 15);
  assert.strictEqual(l.balanceOn('assets:checking', '2026-12-31'), -15);
});
`,
    },
  },
  {
    id: "ledger-csv-quote",
    fixture: "ledger",
    goal: "Make csvExport quote any field that contains a comma, a double quote, or a newline, doubling embedded double quotes (RFC 4180). Other fields stay unquoted.",
    hidden: {
      "test/hidden-csv.test.js": `const test = require('node:test');
const assert = require('node:assert');
const { Ledger, csvExport } = require('../src/ledger');
test('hidden: csv quoting', () => {
  const l = Ledger.fromJournal('2026-06-01 Tea, "fancy"\\n  expenses:food  4\\n  assets:checking  -4\\n');
  const rows = csvExport(l).split('\\n');
  assert.strictEqual(rows[0], 'date,description,account,amount');
  assert.strictEqual(rows[1], '2026-06-01,"Tea, ""fancy""",expenses:food,4.00');
});
`,
    },
  },
  // shop: one 955-line, 28 KB module, larger than the worker inline limit.
  {
    id: "shop-coupon-expiry",
    fixture: "shop",
    goal: "In applyCoupon, reject a coupon whose `expires` date is before `today` with a ShopError whose message is `coupon <CODE> expired`. A coupon is still valid on its expires date, and coupons without `expires` never expire.",
    hidden: {
      "test/hidden-coupon-expiry.test.js": `const test = require('node:test');
const assert = require('node:assert');
const shop = require('../src/shop');
function cart() {
  const catalog = new shop.Catalog();
  catalog.add({ sku: 'MUG-0001', name: 'Mug', price: 1000 });
  const c = new shop.Cart(catalog);
  c.add('MUG-0001');
  return c;
}
const coupons = [
  { code: 'OLD', kind: 'fixed', cents: 100, expires: '2026-05-31' },
  { code: 'EDGE', kind: 'fixed', cents: 100, expires: '2026-06-01' },
  { code: 'FOREVER', kind: 'fixed', cents: 100 },
];
test('hidden: expired coupons are rejected', () => {
  assert.throws(() => shop.applyCoupon(cart(), coupons, 'old', '2026-06-01'), /coupon OLD expired/);
});
test('hidden: valid on the expiry date and without expiry', () => {
  assert.strictEqual(shop.applyCoupon(cart(), coupons, 'edge', '2026-06-01').code, 'EDGE');
  assert.strictEqual(shop.applyCoupon(cart(), coupons, 'forever', '2030-01-01').code, 'FOREVER');
});
`,
    },
  },
  {
    id: "shop-cancel-restock",
    fixture: "shop",
    goal: "When OrderBook.cancel cancels an order, release the stock that order reserved so it can be sold again.",
    hidden: {
      "test/hidden-cancel.test.js": `const test = require('node:test');
const assert = require('node:assert');
const shop = require('../src/shop');
test('hidden: cancelling releases reserved stock', () => {
  const catalog = new shop.Catalog();
  catalog.add({ sku: 'TEE-0001', name: 'Tee', price: 2500 });
  const stock = new shop.Stock();
  stock.receive('TEE-0001', 5);
  const book = new shop.OrderBook(stock);
  const cart = new shop.Cart(catalog);
  cart.add('TEE-0001', 2);
  const order = book.place(cart, '2026-06-01');
  assert.strictEqual(stock.available('TEE-0001'), 3);
  book.cancel(order.id, '2026-06-01');
  assert.strictEqual(stock.available('TEE-0001'), 5);
  assert.strictEqual(stock.levels.get('TEE-0001'), 5);
});
`,
    },
  },
  {
    id: "shop-top-ties",
    fixture: "shop",
    goal: "Make topProducts order products with the same quantity by SKU, alphabetically, and leave out refunded orders as well as cancelled ones.",
    hidden: {
      "test/hidden-top.test.js": `const test = require('node:test');
const assert = require('node:assert');
const shop = require('../src/shop');
test('hidden: ties by sku, refunded excluded', () => {
  const orders = [
    { status: 'paid', items: [{ sku: 'PEN-0001', qty: 2 }, { sku: 'CAP-0001', qty: 2 }] },
    { status: 'refunded', items: [{ sku: 'MUG-0001', qty: 9 }] },
    { status: 'cancelled', items: [{ sku: 'TEE-0001', qty: 9 }] },
    { status: 'placed', items: [{ sku: 'BAG-0001', qty: 2 }] },
  ];
  assert.deepStrictEqual(shop.topProducts(orders, 3), [
    { sku: 'BAG-0001', qty: 2 },
    { sku: 'CAP-0001', qty: 2 },
    { sku: 'PEN-0001', qty: 2 },
  ]);
});
`,
    },
  },
  {
    id: "shop-review-duplicate",
    fixture: "shop",
    goal: "Reviews.add must reject a second review by the same customer for the same SKU with a ShopError whose code is `duplicate_review`. Different customers, or the same customer on another SKU, are fine.",
    hidden: {
      "test/hidden-review.test.js": `const test = require('node:test');
const assert = require('node:assert');
const shop = require('../src/shop');
test('hidden: one review per customer per sku', () => {
  const catalog = new shop.Catalog();
  catalog.add({ sku: 'MUG-0001', name: 'Mug', price: 1000 });
  catalog.add({ sku: 'TEE-0001', name: 'Tee', price: 1000 });
  const r = new shop.Reviews(catalog);
  r.add({ sku: 'MUG-0001', customer: 'ana', stars: 5, text: 'ok', date: '2026-06-01' });
  assert.throws(
    () => r.add({ sku: 'MUG-0001', customer: 'ana', stars: 4, text: 'again', date: '2026-06-02' }),
    (e) => e.code === 'duplicate_review'
  );
  r.add({ sku: 'MUG-0001', customer: 'bo', stars: 4, text: 'ok', date: '2026-06-02' });
  r.add({ sku: 'TEE-0001', customer: 'ana', stars: 3, text: 'ok', date: '2026-06-02' });
  assert.strictEqual(r.items.length, 3);
});
`,
    },
  },
];
