'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { parsePort } = require('../src/port');

test('parses a plain port', () => {
  assert.strictEqual(parsePort('8080'), 8080);
});

test('rejects ports outside 1-65535', () => {
  assert.throws(() => parsePort('0'));
  assert.throws(() => parsePort('70000'));
});

test('rejects trailing garbage', () => {
  assert.throws(() => parsePort('80abc'));
});
