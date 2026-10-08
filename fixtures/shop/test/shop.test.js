const test = require('node:test');
const assert = require('node:assert');
const shop = require('../src/shop');

const TODAY = '2026-06-01';

function setup() {
  const catalog = new shop.Catalog();
  catalog.add({ sku: 'MUG-0001', name: 'Blue mug', price: 1200, weightGrams: 400, tags: ['kitchen'] });
  catalog.add({ sku: 'TEE-0001', name: 'Logo tee', price: 2500, weightGrams: 200, tags: ['apparel'] });
  catalog.add({ sku: 'PEN-0001', name: 'Gel pen', price: 150, weightGrams: 20, tags: ['office'] });
  const stock = new shop.Stock();
  stock.receive('MUG-0001', 10);
  stock.receive('TEE-0001', 5);
  stock.receive('PEN-0001', 100);
  return { catalog, stock };
}

test('money helpers', () => {
  assert.strictEqual(shop.toCents('12.5'), 1250);
  assert.strictEqual(shop.formatMoney(123456), '$1234.56');
  assert.strictEqual(shop.formatMoney(5000, 'JPY'), '¥50');
  assert.strictEqual(shop.percentOf(1000, 725), 73);
  assert.strictEqual(shop.message('de', 'stock.low', { qty: 2 }), 'Nur noch 2 auf Lager.');
});

test('catalog search and slugs', () => {
  const { catalog } = setup();
  assert.deepStrictEqual(catalog.search('mug').map((p) => p.sku), ['MUG-0001']);
  assert.strictEqual(catalog.get('TEE-0001').slug, 'logo-tee');
  assert.throws(() => catalog.add({ sku: 'bad', name: 'x', price: 1 }), /bad sku/);
});

test('cart pricing with tiers, coupons and tax', () => {
  const { catalog } = setup();
  const cart = new shop.Cart(catalog, { region: 'US-CA' });
  cart.add('PEN-0001', 10);
  cart.add('MUG-0001');
  const coupons = [{ code: 'TENOFF', kind: 'percent', basisPoints: 1000 }];
  shop.applyCoupon(cart, coupons, 'tenoff', TODAY);
  assert.deepStrictEqual(shop.priceCart(cart), { subtotal: 2700, discount: 345, tax: 171, total: 2526 });
});

test('shipping by zone and weight', () => {
  const { catalog } = setup();
  const cart = new shop.Cart(catalog, { country: 'DE', region: 'DE' });
  cart.add('MUG-0001', 3);
  assert.strictEqual(shop.zoneFor('DE'), 'europe');
  assert.strictEqual(shop.shippingCost(cart), 1499 + 400 * 2);
});

test('orders reserve, pay and refund', () => {
  const { catalog, stock } = setup();
  const book = new shop.OrderBook(stock);
  const cart = new shop.Cart(catalog);
  cart.add('TEE-0001', 2);
  const order = book.place(cart, TODAY);
  assert.strictEqual(stock.available('TEE-0001'), 3);
  book.pay(order.id, TODAY);
  assert.strictEqual(stock.levels.get('TEE-0001'), 3);
  book.refund(order.id, '2026-06-02');
  assert.strictEqual(order.status, 'refunded');
  assert.throws(() => book.cancel(order.id, TODAY), /cannot go from refunded/);
});

test('reports', () => {
  const { catalog, stock } = setup();
  const book = new shop.OrderBook(stock);
  for (const [sku, qty] of [['PEN-0001', 5], ['MUG-0001', 1], ['PEN-0001', 2]]) {
    const cart = new shop.Cart(catalog);
    cart.add(sku, qty);
    book.place(cart, TODAY);
  }
  assert.deepStrictEqual(shop.topProducts(book.orders, 1), [{ sku: 'PEN-0001', qty: 7 }]);
  assert.strictEqual(shop.dailySales(book.orders)[0].orders, 3);
});

test('gift cards, loyalty and reviews', () => {
  const { catalog } = setup();
  const cards = new shop.GiftCards();
  cards.issue('GC-AB12-CD34', 5000, TODAY);
  assert.strictEqual(cards.redeem('GC-AB12-CD34', 6000, TODAY), 5000);
  const loyalty = new shop.Loyalty();
  assert.strictEqual(loyalty.earn('ana', { id: 'O-1', total: 12345 }), 123);
  const reviews = new shop.Reviews(catalog);
  reviews.add({ sku: 'MUG-0001', customer: 'ana', stars: 5, text: 'Great', date: TODAY });
  reviews.add({ sku: 'MUG-0001', customer: 'bo', stars: 4, text: 'fake!', date: TODAY });
  assert.strictEqual(reviews.rating('MUG-0001'), 5);
});

test('csv import and export', () => {
  const catalog = new shop.Catalog();
  shop.importCatalogCsv(catalog, 'sku,name,price,weight_grams,tags\nCAP-0001,"Cap, red",9.99,80,apparel|summer\n');
  assert.strictEqual(catalog.get('CAP-0001').price, 999);
  assert.deepStrictEqual(catalog.get('CAP-0001').tags, ['apparel', 'summer']);
});
