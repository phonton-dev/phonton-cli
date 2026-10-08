'use strict';

// A small double-entry ledger: parse a plain-text journal, keep balanced
// transactions, and produce balances, reports, and CSV exports.
//
// Journal format, one transaction per block:
//
//   2026-06-01 Coffee beans
//     expenses:food        12.50
//     assets:checking     -12.50
//
// Amounts are in the ledger's single currency. Every transaction must
// balance to zero.

const DATE_RE = /^(\d{4})-(\d{2})-(\d{2})$/;
const ACCOUNT_RE = /^[a-z][a-z0-9_-]*(:[a-z0-9_-]+)*$/;
const ROOTS = ['assets', 'liabilities', 'equity', 'income', 'expenses'];
const EPSILON = 0.005;

class LedgerError extends Error {
  constructor(message, line) {
    super(line === undefined ? message : `line ${line}: ${message}`);
    this.name = 'LedgerError';
    this.line = line;
  }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

function parseDate(text, line) {
  const m = DATE_RE.exec(text);
  if (!m) {
    throw new LedgerError(`bad date ${JSON.stringify(text)}`, line);
  }
  const [year, month, day] = [Number(m[1]), Number(m[2]), Number(m[3])];
  const date = new Date(Date.UTC(year, month - 1, day));
  if (date.getUTCMonth() !== month - 1 || date.getUTCDate() !== day) {
    throw new LedgerError(`no such date ${text}`, line);
  }
  return text;
}

function parseAmount(text, line) {
  const cleaned = text.trim();
  if (!/^-?\d+(\.\d{1,2})?$/.test(cleaned)) {
    throw new LedgerError(`bad amount ${JSON.stringify(text)}`, line);
  }
  return Number(cleaned);
}

function parseAccount(text, line) {
  const name = text.trim();
  if (!ACCOUNT_RE.test(name)) {
    throw new LedgerError(`bad account ${JSON.stringify(name)}`, line);
  }
  const root = name.split(':')[0];
  if (!ROOTS.includes(root)) {
    throw new LedgerError(`unknown account root ${root}`, line);
  }
  return name;
}

function parsePosting(raw, line) {
  const parts = raw.trim().split(/\s{2,}/);
  if (parts.length !== 2) {
    throw new LedgerError('posting needs an account and an amount', line);
  }
  return { account: parseAccount(parts[0], line), amount: parseAmount(parts[1], line) };
}

function parseHeader(raw, line) {
  const trimmed = raw.trim();
  const space = trimmed.indexOf(' ');
  if (space === -1) {
    throw new LedgerError('transaction needs a date and a description', line);
  }
  return {
    date: parseDate(trimmed.slice(0, space), line),
    description: trimmed.slice(space + 1).trim(),
  };
}

function parseJournal(text) {
  const transactions = [];
  let current = null;
  const lines = text.split(/\r?\n/);
  lines.forEach((raw, index) => {
    const line = index + 1;
    const content = raw.replace(/;.*$/, '');
    if (content.trim() === '') {
      if (current) {
        transactions.push(current);
        current = null;
      }
      return;
    }
    if (/^\s/.test(content)) {
      if (!current) {
        throw new LedgerError('posting outside a transaction', line);
      }
      current.postings.push(parsePosting(content, line));
      return;
    }
    if (current) {
      transactions.push(current);
    }
    current = { ...parseHeader(content, line), postings: [], line };
  });
  if (current) {
    transactions.push(current);
  }
  return transactions;
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

function formatAmount(amount) {
  const rounded = Math.round(amount * 100) / 100;
  return rounded.toFixed(2);
}

function padRight(text, width) {
  return text.length >= width ? text : text + ' '.repeat(width - text.length);
}

function padLeft(text, width) {
  return text.length >= width ? text : ' '.repeat(width - text.length) + text;
}

function formatTransaction(tx) {
  const lines = [`${tx.date} ${tx.description}`];
  for (const p of tx.postings) {
    lines.push(`  ${padRight(p.account, 24)}${padLeft(formatAmount(p.amount), 12)}`);
  }
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

function transactionTotal(tx) {
  return tx.postings.reduce((sum, p) => sum + p.amount, 0);
}

function assertBalanced(tx) {
  if (tx.postings.length < 2) {
    throw new LedgerError(`"${tx.description}" needs at least two postings`, tx.line);
  }
  const total = transactionTotal(tx);
  if (Math.abs(total) > EPSILON) {
    throw new LedgerError(
      `"${tx.description}" is off by ${formatAmount(total)}`,
      tx.line
    );
  }
}

class Ledger {
  constructor() {
    this.transactions = [];
  }

  static fromJournal(text) {
    const ledger = new Ledger();
    for (const tx of parseJournal(text)) {
      ledger.add(tx);
    }
    return ledger;
  }

  add(tx) {
    assertBalanced(tx);
    const copy = {
      date: tx.date,
      description: tx.description,
      postings: tx.postings.map((p) => ({ account: p.account, amount: p.amount })),
      line: tx.line,
    };
    // Keep transactions in date order; equal dates keep insertion order.
    let at = this.transactions.length;
    while (at > 0 && this.transactions[at - 1].date > copy.date) {
      at -= 1;
    }
    this.transactions.splice(at, 0, copy);
    return copy;
  }

  accounts() {
    const names = new Set();
    for (const tx of this.transactions) {
      for (const p of tx.postings) {
        names.add(p.account);
      }
    }
    return [...names].sort();
  }

  balance(account) {
    let total = 0;
    for (const tx of this.transactions) {
      for (const p of tx.postings) {
        if (p.account === account || p.account.startsWith(`${account}:`)) {
          total += p.amount;
        }
      }
    }
    return Math.round(total * 100) / 100;
  }

  between(from, to) {
    return this.transactions.filter((tx) => tx.date >= from && tx.date <= to);
  }

  search(term) {
    const needle = term.toLowerCase();
    return this.transactions.filter(
      (tx) =>
        tx.description.toLowerCase().includes(needle) ||
        tx.postings.some((p) => p.account.includes(needle))
    );
  }

  toJournal() {
    return this.transactions.map(formatTransaction).join('\n\n') + '\n';
  }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

function balanceReport(ledger) {
  const rows = ledger.accounts().map((account) => ({
    account,
    balance: ledger.balance(account),
  }));
  const width = Math.max(7, ...rows.map((r) => r.account.length));
  const lines = rows.map(
    (r) => `${padRight(r.account, width)}  ${padLeft(formatAmount(r.balance), 12)}`
  );
  lines.push(`${padRight('', width)}  ${'-'.repeat(12)}`);
  const net = rows.reduce((sum, r) => sum + r.balance, 0);
  lines.push(`${padRight('net', width)}  ${padLeft(formatAmount(net), 12)}`);
  return lines.join('\n');
}

function monthlyTotals(ledger, root) {
  const totals = new Map();
  for (const tx of ledger.transactions) {
    const month = tx.date.slice(0, 7);
    for (const p of tx.postings) {
      if (p.account === root || p.account.startsWith(`${root}:`)) {
        totals.set(month, (totals.get(month) ?? 0) + p.amount);
      }
    }
  }
  return [...totals.entries()]
    .sort(([a], [b]) => (a < b ? -1 : 1))
    .map(([month, total]) => ({ month, total: Math.round(total * 100) / 100 }));
}

function budgetCheck(ledger, budgets, month) {
  const over = [];
  for (const [account, limit] of Object.entries(budgets)) {
    const spent = ledger.transactions
      .filter((tx) => tx.date.startsWith(month))
      .flatMap((tx) => tx.postings)
      .filter((p) => p.account === account || p.account.startsWith(`${account}:`))
      .reduce((sum, p) => sum + p.amount, 0);
    if (spent > limit + EPSILON) {
      over.push({ account, limit, spent: Math.round(spent * 100) / 100 });
    }
  }
  return over.sort((a, b) => b.spent - b.limit - (a.spent - a.limit));
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

function csvExport(ledger) {
  const rows = [['date', 'description', 'account', 'amount']];
  for (const tx of ledger.transactions) {
    for (const p of tx.postings) {
      rows.push([tx.date, tx.description, p.account, formatAmount(p.amount)]);
    }
  }
  return rows.map((r) => r.join(',')).join('\n') + '\n';
}

// ---------------------------------------------------------------------------
// Recurring entries
// ---------------------------------------------------------------------------

function addMonths(date, months) {
  const [year, month, day] = date.split('-').map(Number);
  const target = new Date(Date.UTC(year, month - 1 + months, 1));
  const lastDay = new Date(
    Date.UTC(target.getUTCFullYear(), target.getUTCMonth() + 1, 0)
  ).getUTCDate();
  target.setUTCDate(Math.min(day, lastDay));
  return target.toISOString().slice(0, 10);
}

function expandRecurring(template, count) {
  if (!Number.isInteger(count) || count < 1) {
    throw new LedgerError('count must be a positive integer');
  }
  const out = [];
  for (let i = 0; i < count; i += 1) {
    out.push({
      date: addMonths(template.date, i),
      description: template.description,
      postings: template.postings.map((p) => ({ ...p })),
    });
  }
  return out;
}

module.exports = {
  Ledger,
  LedgerError,
  parseJournal,
  parseAmount,
  parseDate,
  formatAmount,
  formatTransaction,
  balanceReport,
  monthlyTotals,
  budgetCheck,
  csvExport,
  addMonths,
  expandRecurring,
};
