const test = require('node:test');
const assert = require('node:assert');
const {
  Ledger,
  LedgerError,
  parseJournal,
  formatAmount,
  balanceReport,
  monthlyTotals,
  budgetCheck,
  csvExport,
  addMonths,
  expandRecurring,
} = require('../src/ledger');

const JOURNAL = `2026-06-01 Coffee beans
  expenses:food        12.50
  assets:checking     -12.50

2026-06-03 Salary ; monthly
  assets:checking   2000
  income:salary    -2000

2026-05-28 Rent
  expenses:rent      900
  assets:checking   -900
`;

test('parses a journal into balanced transactions', () => {
  const txs = parseJournal(JOURNAL);
  assert.strictEqual(txs.length, 3);
  assert.deepStrictEqual(txs[1].postings, [
    { account: 'assets:checking', amount: 2000 },
    { account: 'income:salary', amount: -2000 },
  ]);
});

test('rejects unbalanced transactions with the line number', () => {
  assert.throws(
    () => Ledger.fromJournal('2026-06-01 Oops\n  expenses:food  5\n  assets:checking  -4\n'),
    (e) => e instanceof LedgerError && /line 1: "Oops" is off by 1.00/.test(e.message)
  );
});

test('keeps transactions in date order and sums parent accounts', () => {
  const ledger = Ledger.fromJournal(JOURNAL);
  assert.deepStrictEqual(ledger.transactions.map((t) => t.description), ['Rent', 'Coffee beans', 'Salary']);
  assert.strictEqual(ledger.balance('assets'), 1087.5);
  assert.strictEqual(ledger.balance('expenses'), 912.5);
});

test('formats amounts with two decimals', () => {
  assert.strictEqual(formatAmount(3), '3.00');
  assert.strictEqual(formatAmount(-12.5), '-12.50');
});

test('balance report ends with a zero net', () => {
  const report = balanceReport(Ledger.fromJournal(JOURNAL));
  assert.match(report, /assets:checking\s+1087\.50/);
  assert.match(report.split('\n').pop(), /net\s+0\.00$/);
});

test('monthly totals and budgets', () => {
  const ledger = Ledger.fromJournal(JOURNAL);
  assert.deepStrictEqual(monthlyTotals(ledger, 'expenses'), [
    { month: '2026-05', total: 900 },
    { month: '2026-06', total: 12.5 },
  ]);
  assert.deepStrictEqual(budgetCheck(ledger, { 'expenses:food': 10 }, '2026-06'), [
    { account: 'expenses:food', limit: 10, spent: 12.5 },
  ]);
});

test('csv export has one row per posting', () => {
  const csv = csvExport(Ledger.fromJournal(JOURNAL));
  assert.strictEqual(csv.split('\n')[1], '2026-05-28,Rent,expenses:rent,900.00');
});

test('recurring entries clamp to month end', () => {
  assert.strictEqual(addMonths('2026-01-31', 1), '2026-02-28');
  const out = expandRecurring(
    { date: '2026-01-31', description: 'Gym', postings: [{ account: 'expenses:gym', amount: 30 }] },
    3
  );
  assert.deepStrictEqual(out.map((t) => t.date), ['2026-01-31', '2026-02-28', '2026-03-31']);
});
