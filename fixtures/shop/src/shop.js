'use strict';

// shop.js: catalog, stock, carts, pricing, orders, shipping and reports for a
// small web shop. It grew one feature at a time and was never split up.
//
// Money is held in integer cents everywhere. Dates are YYYY-MM-DD strings in
// the shop's own time zone; the caller passes "today" explicitly so the code
// stays deterministic.

// ---------------------------------------------------------------------------
// Reference data
// ---------------------------------------------------------------------------

const CURRENCIES = {
  USD: { symbol: '$', name: 'US dollar', minorUnits: 2 },
  EUR: { symbol: '€', name: 'Euro', minorUnits: 2 },
  GBP: { symbol: '£', name: 'Pound sterling', minorUnits: 2 },
  CHF: { symbol: 'CHF ', name: 'Swiss franc', minorUnits: 2 },
  SEK: { symbol: 'kr ', name: 'Swedish krona', minorUnits: 2 },
  NOK: { symbol: 'kr ', name: 'Norwegian krone', minorUnits: 2 },
  DKK: { symbol: 'kr ', name: 'Danish krone', minorUnits: 2 },
  PLN: { symbol: 'zł ', name: 'Polish złoty', minorUnits: 2 },
  CZK: { symbol: 'Kč ', name: 'Czech koruna', minorUnits: 2 },
  HUF: { symbol: 'Ft ', name: 'Hungarian forint', minorUnits: 2 },
  CAD: { symbol: 'CA$', name: 'Canadian dollar', minorUnits: 2 },
  AUD: { symbol: 'A$', name: 'Australian dollar', minorUnits: 2 },
  NZD: { symbol: 'NZ$', name: 'New Zealand dollar', minorUnits: 2 },
  JPY: { symbol: '¥', name: 'Japanese yen', minorUnits: 0 },
  KRW: { symbol: '₩', name: 'South Korean won', minorUnits: 0 },
  INR: { symbol: '₹', name: 'Indian rupee', minorUnits: 2 },
  BRL: { symbol: 'R$', name: 'Brazilian real', minorUnits: 2 },
  MXN: { symbol: 'MX$', name: 'Mexican peso', minorUnits: 2 },
  ZAR: { symbol: 'R ', name: 'South African rand', minorUnits: 2 },
  SGD: { symbol: 'S$', name: 'Singapore dollar', minorUnits: 2 },
};

// VAT / sales tax in basis points (1/100 of a percent), by region code.
const TAX_RATES = {
  'US-AL': 400, 'US-AK': 0, 'US-AZ': 560, 'US-AR': 650, 'US-CA': 725,
  'US-CO': 290, 'US-CT': 635, 'US-DE': 0, 'US-FL': 600, 'US-GA': 400,
  'US-HI': 400, 'US-ID': 600, 'US-IL': 625, 'US-IN': 700, 'US-IA': 600,
  'US-KS': 650, 'US-KY': 600, 'US-LA': 445, 'US-ME': 550, 'US-MD': 600,
  'US-MA': 625, 'US-MI': 600, 'US-MN': 688, 'US-MS': 700, 'US-MO': 423,
  'US-MT': 0, 'US-NE': 550, 'US-NV': 685, 'US-NH': 0, 'US-NJ': 663,
  'US-NM': 488, 'US-NY': 400, 'US-NC': 475, 'US-ND': 500, 'US-OH': 575,
  'US-OK': 450, 'US-OR': 0, 'US-PA': 600, 'US-RI': 700, 'US-SC': 600,
  'US-SD': 420, 'US-TN': 700, 'US-TX': 625, 'US-UT': 610, 'US-VT': 600,
  'US-VA': 530, 'US-WA': 650, 'US-WV': 600, 'US-WI': 500, 'US-WY': 400,
  AT: 2000, BE: 2100, BG: 2000, HR: 2500, CY: 1900, CZ: 2100, DK: 2500,
  EE: 2200, FI: 2550, FR: 2000, DE: 1900, GR: 2400, HU: 2700, IE: 2300,
  IT: 2200, LV: 2100, LT: 2100, LU: 1700, MT: 1800, NL: 2100, PL: 2300,
  PT: 2300, RO: 1900, SK: 2300, SI: 2200, ES: 2100, SE: 2500, GB: 2000,
  CH: 810, NO: 2500, CA: 500, AU: 1000, NZ: 1500, JP: 1000, SG: 900,
};

// Shipping zones: countries, then base cost and per-kilogram cost in cents.
const SHIPPING_ZONES = {
  domestic: { countries: ['US'], base: 499, perKg: 120 },
  canada: { countries: ['CA'], base: 999, perKg: 250 },
  europe: {
    countries: [
      'AT', 'BE', 'BG', 'HR', 'CY', 'CZ', 'DK', 'EE', 'FI', 'FR', 'DE', 'GR',
      'HU', 'IE', 'IT', 'LV', 'LT', 'LU', 'MT', 'NL', 'PL', 'PT', 'RO', 'SK',
      'SI', 'ES', 'SE', 'GB', 'CH', 'NO',
    ],
    base: 1499,
    perKg: 400,
  },
  pacific: { countries: ['AU', 'NZ', 'JP', 'SG'], base: 1999, perKg: 550 },
};

const MESSAGES = {
  en: {
    'cart.empty': 'Your cart is empty.',
    'cart.added': 'Added {qty} × {name} to your cart.',
    'cart.removed': 'Removed {name} from your cart.',
    'stock.low': 'Only {qty} left in stock.',
    'stock.out': '{name} is out of stock.',
    'coupon.applied': 'Coupon {code} applied.',
    'coupon.invalid': 'Coupon {code} is not valid.',
    'order.placed': 'Order {id} placed. Thank you!',
    'order.cancelled': 'Order {id} was cancelled.',
    'order.refunded': 'Order {id} was refunded.',
    'shipping.free': 'Free shipping.',
    'shipping.cost': 'Shipping: {cost}.',
  },
  de: {
    'cart.empty': 'Ihr Warenkorb ist leer.',
    'cart.added': '{qty} × {name} zum Warenkorb hinzugefügt.',
    'cart.removed': '{name} aus dem Warenkorb entfernt.',
    'stock.low': 'Nur noch {qty} auf Lager.',
    'stock.out': '{name} ist ausverkauft.',
    'coupon.applied': 'Gutschein {code} eingelöst.',
    'coupon.invalid': 'Gutschein {code} ist ungültig.',
    'order.placed': 'Bestellung {id} aufgegeben. Danke!',
    'order.cancelled': 'Bestellung {id} wurde storniert.',
    'order.refunded': 'Bestellung {id} wurde erstattet.',
    'shipping.free': 'Kostenloser Versand.',
    'shipping.cost': 'Versand: {cost}.',
  },
  fr: {
    'cart.empty': 'Votre panier est vide.',
    'cart.added': '{qty} × {name} ajouté au panier.',
    'cart.removed': '{name} retiré du panier.',
    'stock.low': 'Plus que {qty} en stock.',
    'stock.out': '{name} est en rupture de stock.',
    'coupon.applied': 'Code {code} appliqué.',
    'coupon.invalid': "Le code {code} n'est pas valide.",
    'order.placed': 'Commande {id} passée. Merci !',
    'order.cancelled': 'La commande {id} a été annulée.',
    'order.refunded': 'La commande {id} a été remboursée.',
    'shipping.free': 'Livraison gratuite.',
    'shipping.cost': 'Livraison : {cost}.',
  },
};

class ShopError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'ShopError';
    this.code = code;
  }
}

// ---------------------------------------------------------------------------
// Money and text helpers
// ---------------------------------------------------------------------------

function toCents(text) {
  const m = /^(-)?(\d+)(?:\.(\d{1,2}))?$/.exec(String(text).trim());
  if (!m) {
    throw new ShopError('bad_amount', `bad amount ${JSON.stringify(text)}`);
  }
  const cents = Number(m[2]) * 100 + Number((m[3] ?? '0').padEnd(2, '0'));
  return m[1] ? -cents : cents;
}

function formatMoney(cents, currency = 'USD') {
  const info = CURRENCIES[currency];
  if (!info) {
    throw new ShopError('bad_currency', `unknown currency ${currency}`);
  }
  const negative = cents < 0;
  const abs = Math.abs(cents);
  const text =
    info.minorUnits === 0
      ? String(Math.round(abs / 100))
      : `${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, '0')}`;
  return `${negative ? '-' : ''}${info.symbol}${text}`;
}

function percentOf(cents, basisPoints) {
  // Round half away from zero, like the payment provider does.
  const raw = (cents * basisPoints) / 10000;
  return Math.sign(raw) * Math.round(Math.abs(raw));
}

function message(locale, key, params = {}) {
  const table = MESSAGES[locale] ?? MESSAGES.en;
  const template = table[key] ?? MESSAGES.en[key];
  if (template === undefined) {
    throw new ShopError('bad_message', `unknown message ${key}`);
  }
  return template.replace(/\{(\w+)\}/g, (_, name) =>
    params[name] === undefined ? `{${name}}` : String(params[name])
  );
}

function slugify(text) {
  return text
    .toLowerCase()
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

function compareDates(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

const SKU_RE = /^[A-Z]{3}-\d{4}$/;

class Catalog {
  constructor() {
    this.products = new Map();
  }

  add(product) {
    if (!SKU_RE.test(product.sku)) {
      throw new ShopError('bad_sku', `bad sku ${product.sku}`);
    }
    if (this.products.has(product.sku)) {
      throw new ShopError('duplicate_sku', `duplicate sku ${product.sku}`);
    }
    if (!Number.isInteger(product.price) || product.price < 0) {
      throw new ShopError('bad_price', `bad price for ${product.sku}`);
    }
    const entry = {
      sku: product.sku,
      name: product.name,
      slug: slugify(product.name),
      price: product.price,
      weightGrams: product.weightGrams ?? 0,
      tags: [...(product.tags ?? [])].sort(),
      active: product.active !== false,
    };
    this.products.set(entry.sku, entry);
    return entry;
  }

  get(sku) {
    const product = this.products.get(sku);
    if (!product) {
      throw new ShopError('unknown_sku', `unknown sku ${sku}`);
    }
    return product;
  }

  search(query) {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    return [...this.products.values()]
      .filter((p) => p.active)
      .filter((p) =>
        words.every((w) => p.name.toLowerCase().includes(w) || p.tags.includes(w))
      )
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  byTag(tag) {
    return [...this.products.values()].filter((p) => p.active && p.tags.includes(tag));
  }

  setActive(sku, active) {
    this.get(sku).active = active;
  }
}

// ---------------------------------------------------------------------------
// Stock
// ---------------------------------------------------------------------------

class Stock {
  constructor() {
    this.levels = new Map();
    this.reserved = new Map();
  }

  receive(sku, qty) {
    if (!Number.isInteger(qty) || qty <= 0) {
      throw new ShopError('bad_qty', `bad quantity ${qty}`);
    }
    this.levels.set(sku, (this.levels.get(sku) ?? 0) + qty);
  }

  available(sku) {
    return (this.levels.get(sku) ?? 0) - (this.reserved.get(sku) ?? 0);
  }

  reserve(sku, qty) {
    if (this.available(sku) < qty) {
      throw new ShopError('out_of_stock', `not enough stock for ${sku}`);
    }
    this.reserved.set(sku, (this.reserved.get(sku) ?? 0) + qty);
  }

  release(sku, qty) {
    const held = this.reserved.get(sku) ?? 0;
    this.reserved.set(sku, Math.max(0, held - qty));
  }

  commit(sku, qty) {
    this.release(sku, qty);
    this.levels.set(sku, (this.levels.get(sku) ?? 0) - qty);
  }

  lowStock(threshold) {
    return [...this.levels.keys()]
      .filter((sku) => this.available(sku) <= threshold)
      .sort();
  }
}

// ---------------------------------------------------------------------------
// Carts
// ---------------------------------------------------------------------------

class Cart {
  constructor(catalog, { currency = 'USD', region = 'US-CA', country = 'US' } = {}) {
    this.catalog = catalog;
    this.currency = currency;
    this.region = region;
    this.country = country;
    this.lines = new Map();
    this.coupons = [];
  }

  add(sku, qty = 1) {
    const product = this.catalog.get(sku);
    if (!product.active) {
      throw new ShopError('inactive', `${product.name} is not for sale`);
    }
    if (!Number.isInteger(qty) || qty <= 0) {
      throw new ShopError('bad_qty', `bad quantity ${qty}`);
    }
    this.lines.set(sku, (this.lines.get(sku) ?? 0) + qty);
  }

  remove(sku) {
    this.lines.delete(sku);
  }

  setQuantity(sku, qty) {
    if (qty === 0) {
      this.remove(sku);
      return;
    }
    if (!this.lines.has(sku)) {
      throw new ShopError('not_in_cart', `${sku} is not in the cart`);
    }
    this.lines.set(sku, qty);
  }

  items() {
    return [...this.lines.entries()].map(([sku, qty]) => {
      const product = this.catalog.get(sku);
      return { sku, name: product.name, qty, unitPrice: product.price, total: product.price * qty };
    });
  }

  subtotal() {
    return this.items().reduce((sum, line) => sum + line.total, 0);
  }

  weightGrams() {
    return this.items().reduce(
      (sum, line) => sum + this.catalog.get(line.sku).weightGrams * line.qty,
      0
    );
  }

  isEmpty() {
    return this.lines.size === 0;
  }
}

// ---------------------------------------------------------------------------
// Pricing: coupons, tiers, tax
// ---------------------------------------------------------------------------

// Coupon shapes:
//   { code, kind: 'percent', basisPoints }        e.g. 1000 = 10% off
//   { code, kind: 'fixed', cents }                fixed amount off
//   { code, kind: 'tag', tag, basisPoints }       percent off tagged items
// Optional fields: minSubtotal (cents), expires (YYYY-MM-DD, last valid day).
function couponDiscount(cart, coupon) {
  const subtotal = cart.subtotal();
  if (coupon.minSubtotal !== undefined && subtotal < coupon.minSubtotal) {
    return 0;
  }
  switch (coupon.kind) {
    case 'percent':
      return percentOf(subtotal, coupon.basisPoints);
    case 'fixed':
      return Math.min(coupon.cents, subtotal);
    case 'tag': {
      const tagged = cart
        .items()
        .filter((line) => cart.catalog.get(line.sku).tags.includes(coupon.tag))
        .reduce((sum, line) => sum + line.total, 0);
      return percentOf(tagged, coupon.basisPoints);
    }
    default:
      throw new ShopError('bad_coupon', `unknown coupon kind ${coupon.kind}`);
  }
}

function applyCoupon(cart, coupons, code, today) {
  const coupon = coupons.find((c) => c.code === code.toUpperCase());
  if (!coupon) {
    throw new ShopError('unknown_coupon', `unknown coupon ${code}`);
  }
  if (cart.coupons.some((c) => c.code === coupon.code)) {
    throw new ShopError('duplicate_coupon', `coupon ${coupon.code} already applied`);
  }
  if (cart.coupons.length > 0 && !coupon.stackable) {
    throw new ShopError('not_stackable', `coupon ${coupon.code} cannot be combined`);
  }
  cart.coupons.push(coupon);
  return coupon;
}

// Volume tiers: buy more of one SKU, pay less per unit.
const TIERS = [
  { minQty: 50, basisPoints: 1500 },
  { minQty: 20, basisPoints: 1000 },
  { minQty: 10, basisPoints: 500 },
];

function tierDiscount(cart) {
  let discount = 0;
  for (const line of cart.items()) {
    const tier = TIERS.find((t) => line.qty >= t.minQty);
    if (tier) {
      discount += percentOf(line.total, tier.basisPoints);
    }
  }
  return discount;
}

function taxRate(region) {
  const rate = TAX_RATES[region];
  if (rate === undefined) {
    throw new ShopError('unknown_region', `no tax rate for ${region}`);
  }
  return rate;
}

function priceCart(cart) {
  const subtotal = cart.subtotal();
  const tiers = tierDiscount(cart);
  const coupons = cart.coupons.reduce((sum, c) => sum + couponDiscount(cart, c), 0);
  const discount = Math.min(subtotal, tiers + coupons);
  const taxable = subtotal - discount;
  const tax = percentOf(taxable, taxRate(cart.region));
  return { subtotal, discount, tax, total: taxable + tax };
}

// ---------------------------------------------------------------------------
// Shipping
// ---------------------------------------------------------------------------

function zoneFor(country) {
  for (const [name, zone] of Object.entries(SHIPPING_ZONES)) {
    if (zone.countries.includes(country)) {
      return name;
    }
  }
  throw new ShopError('no_shipping', `we do not ship to ${country}`);
}

function shippingCost(cart) {
  if (cart.isEmpty()) {
    return 0;
  }
  const zone = SHIPPING_ZONES[zoneFor(cart.country)];
  const kg = Math.ceil(cart.weightGrams() / 1000);
  return zone.base + zone.perKg * kg;
}

function shippingLabel(cart, locale = 'en') {
  const cost = shippingCost(cart);
  return cost === 0
    ? message(locale, 'shipping.free')
    : message(locale, 'shipping.cost', { cost: formatMoney(cost, cart.currency) });
}

// ---------------------------------------------------------------------------
// Orders
// ---------------------------------------------------------------------------

// placed -> paid -> shipped -> delivered
//    \-> cancelled      \-> refunded (from paid, shipped or delivered)
const TRANSITIONS = {
  placed: ['paid', 'cancelled'],
  paid: ['shipped', 'refunded'],
  shipped: ['delivered', 'refunded'],
  delivered: ['refunded'],
  cancelled: [],
  refunded: [],
};

class OrderBook {
  constructor(stock) {
    this.stock = stock;
    this.orders = [];
    this.nextId = 1000;
  }

  place(cart, today) {
    if (cart.isEmpty()) {
      throw new ShopError('empty_cart', 'cart is empty');
    }
    const items = cart.items();
    const reserved = [];
    try {
      for (const line of items) {
        this.stock.reserve(line.sku, line.qty);
        reserved.push(line);
      }
    } catch (err) {
      for (const line of reserved) {
        this.stock.release(line.sku, line.qty);
      }
      throw err;
    }
    const price = priceCart(cart);
    const order = {
      id: `O-${this.nextId++}`,
      date: today,
      items,
      currency: cart.currency,
      country: cart.country,
      coupons: cart.coupons.map((c) => c.code),
      ...price,
      shipping: shippingCost(cart),
      status: 'placed',
      history: [{ status: 'placed', date: today }],
    };
    order.grandTotal = order.total + order.shipping;
    this.orders.push(order);
    return order;
  }

  get(id) {
    const order = this.orders.find((o) => o.id === id);
    if (!order) {
      throw new ShopError('unknown_order', `unknown order ${id}`);
    }
    return order;
  }

  transition(id, status, today) {
    const order = this.get(id);
    if (!TRANSITIONS[order.status].includes(status)) {
      throw new ShopError('bad_transition', `cannot go from ${order.status} to ${status}`);
    }
    order.status = status;
    order.history.push({ status, date: today });
    return order;
  }

  pay(id, today) {
    const order = this.transition(id, 'paid', today);
    for (const line of order.items) {
      this.stock.commit(line.sku, line.qty);
    }
    return order;
  }

  cancel(id, today) {
    return this.transition(id, 'cancelled', today);
  }

  refund(id, today) {
    const order = this.transition(id, 'refunded', today);
    order.refundedCents = order.grandTotal;
    return order;
  }

  byStatus(status) {
    return this.orders.filter((o) => o.status === status);
  }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

function dailySales(orders) {
  const days = new Map();
  for (const order of orders) {
    if (order.status === 'cancelled' || order.status === 'refunded') {
      continue;
    }
    const day = days.get(order.date) ?? { date: order.date, orders: 0, cents: 0 };
    day.orders += 1;
    day.cents += order.grandTotal;
    days.set(order.date, day);
  }
  return [...days.values()].sort((a, b) => compareDates(a.date, b.date));
}

function topProducts(orders, n = 5) {
  const units = new Map();
  for (const order of orders) {
    if (order.status === 'cancelled') {
      continue;
    }
    for (const line of order.items) {
      units.set(line.sku, (units.get(line.sku) ?? 0) + line.qty);
    }
  }
  return [...units.entries()]
    .map(([sku, qty]) => ({ sku, qty }))
    .sort((a, b) => b.qty - a.qty)
    .slice(0, n);
}

function averageOrderValue(orders) {
  const counted = orders.filter((o) => o.status !== 'cancelled' && o.status !== 'refunded');
  if (counted.length === 0) {
    return 0;
  }
  const total = counted.reduce((sum, o) => sum + o.grandTotal, 0);
  return Math.round(total / counted.length);
}

function couponUsage(orders) {
  const usage = {};
  for (const order of orders) {
    for (const code of order.coupons) {
      usage[code] = (usage[code] ?? 0) + 1;
    }
  }
  return usage;
}

// ---------------------------------------------------------------------------
// Gift cards
// ---------------------------------------------------------------------------

const GIFT_CODE_RE = /^GC-[A-Z0-9]{4}-[A-Z0-9]{4}$/;

class GiftCards {
  constructor() {
    this.cards = new Map();
    this.ledger = [];
  }

  issue(code, cents, today) {
    if (!GIFT_CODE_RE.test(code)) {
      throw new ShopError('bad_gift_code', `bad gift card code ${code}`);
    }
    if (this.cards.has(code)) {
      throw new ShopError('duplicate_gift_code', `gift card ${code} exists`);
    }
    if (!Number.isInteger(cents) || cents <= 0) {
      throw new ShopError('bad_amount', `bad gift card amount ${cents}`);
    }
    this.cards.set(code, { code, balance: cents, issued: today, frozen: false });
    this.ledger.push({ code, date: today, delta: cents, reason: 'issue' });
    return this.cards.get(code);
  }

  balance(code) {
    const card = this.cards.get(code);
    if (!card) {
      throw new ShopError('unknown_gift_card', `unknown gift card ${code}`);
    }
    return card.balance;
  }

  redeem(code, cents, today) {
    const card = this.cards.get(code);
    if (!card) {
      throw new ShopError('unknown_gift_card', `unknown gift card ${code}`);
    }
    if (card.frozen) {
      throw new ShopError('frozen_gift_card', `gift card ${code} is frozen`);
    }
    const used = Math.min(card.balance, cents);
    card.balance -= used;
    this.ledger.push({ code, date: today, delta: -used, reason: 'redeem' });
    return used;
  }

  freeze(code) {
    const card = this.cards.get(code);
    if (!card) {
      throw new ShopError('unknown_gift_card', `unknown gift card ${code}`);
    }
    card.frozen = true;
  }

  outstanding() {
    return [...this.cards.values()].reduce((sum, card) => sum + card.balance, 0);
  }

  history(code) {
    return this.ledger.filter((entry) => entry.code === code);
  }
}

// ---------------------------------------------------------------------------
// Loyalty points
// ---------------------------------------------------------------------------

// One point per whole currency unit spent; tiers multiply points earned.
const LOYALTY_TIERS = [
  { name: 'gold', minPoints: 5000, multiplier: 2 },
  { name: 'silver', minPoints: 1000, multiplier: 1.5 },
  { name: 'bronze', minPoints: 0, multiplier: 1 },
];

const POINT_VALUE_CENTS = 1;

class Loyalty {
  constructor() {
    this.accounts = new Map();
  }

  account(customer) {
    if (!this.accounts.has(customer)) {
      this.accounts.set(customer, { customer, points: 0, lifetime: 0, events: [] });
    }
    return this.accounts.get(customer);
  }

  tier(customer) {
    const { lifetime } = this.account(customer);
    return LOYALTY_TIERS.find((t) => lifetime >= t.minPoints).name;
  }

  earn(customer, order) {
    const acct = this.account(customer);
    const tier = LOYALTY_TIERS.find((t) => t.name === this.tier(customer));
    const points = Math.floor((order.total / 100) * tier.multiplier);
    acct.points += points;
    acct.lifetime += points;
    acct.events.push({ order: order.id, points });
    return points;
  }

  spend(customer, points) {
    const acct = this.account(customer);
    if (!Number.isInteger(points) || points <= 0) {
      throw new ShopError('bad_points', `bad points ${points}`);
    }
    if (acct.points < points) {
      throw new ShopError('not_enough_points', `${customer} has ${acct.points} points`);
    }
    acct.points -= points;
    acct.events.push({ order: null, points: -points });
    return points * POINT_VALUE_CENTS;
  }

  leaderboard(n = 10) {
    return [...this.accounts.values()]
      .sort((a, b) => b.lifetime - a.lifetime || a.customer.localeCompare(b.customer))
      .slice(0, n)
      .map(({ customer, lifetime }) => ({ customer, lifetime }));
  }
}

// ---------------------------------------------------------------------------
// Reviews
// ---------------------------------------------------------------------------

const BANNED_WORDS = ['spam', 'scam', 'fake'];

class Reviews {
  constructor(catalog) {
    this.catalog = catalog;
    this.items = [];
    this.nextId = 1;
  }

  add({ sku, customer, stars, text, date }) {
    this.catalog.get(sku);
    if (!Number.isInteger(stars) || stars < 1 || stars > 5) {
      throw new ShopError('bad_stars', 'stars must be 1 to 5');
    }
    const body = (text ?? '').trim();
    if (body.length > 2000) {
      throw new ShopError('review_too_long', 'review is longer than 2000 characters');
    }
    const flagged = BANNED_WORDS.some((w) => body.toLowerCase().includes(w));
    const review = {
      id: this.nextId++,
      sku,
      customer,
      stars,
      text: body,
      date,
      status: flagged ? 'held' : 'published',
    };
    this.items.push(review);
    return review;
  }

  published(sku) {
    return this.items
      .filter((r) => r.sku === sku && r.status === 'published')
      .sort((a, b) => compareDates(b.date, a.date) || b.id - a.id);
  }

  rating(sku) {
    const list = this.published(sku);
    if (list.length === 0) {
      return null;
    }
    const sum = list.reduce((total, r) => total + r.stars, 0);
    return Math.round((sum / list.length) * 10) / 10;
  }

  approve(id) {
    const review = this.items.find((r) => r.id === id);
    if (!review) {
      throw new ShopError('unknown_review', `unknown review ${id}`);
    }
    review.status = 'published';
    return review;
  }

  histogram(sku) {
    const counts = { 1: 0, 2: 0, 3: 0, 4: 0, 5: 0 };
    for (const review of this.published(sku)) {
      counts[review.stars] += 1;
    }
    return counts;
  }
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

class AuditLog {
  constructor(limit = 1000) {
    this.limit = limit;
    this.entries = [];
  }

  record(actor, action, details = {}, date) {
    const entry = { actor, action, details: { ...details }, date };
    this.entries.push(entry);
    if (this.entries.length > this.limit) {
      this.entries.splice(0, this.entries.length - this.limit);
    }
    return entry;
  }

  byActor(actor) {
    return this.entries.filter((e) => e.actor === actor);
  }

  between(from, to) {
    return this.entries.filter((e) => e.date >= from && e.date <= to);
  }

  summary() {
    const counts = {};
    for (const entry of this.entries) {
      counts[entry.action] = (counts[entry.action] ?? 0) + 1;
    }
    return Object.entries(counts)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([action, count]) => `${action}: ${count}`)
      .join('\n');
  }
}

// ---------------------------------------------------------------------------
// Import / export
// ---------------------------------------------------------------------------

function parseCsvLine(line) {
  const fields = [];
  let field = '';
  let quoted = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (quoted) {
      if (ch === '"' && line[i + 1] === '"') {
        field += '"';
        i += 1;
      } else if (ch === '"') {
        quoted = false;
      } else {
        field += ch;
      }
    } else if (ch === '"') {
      quoted = true;
    } else if (ch === ',') {
      fields.push(field);
      field = '';
    } else {
      field += ch;
    }
  }
  fields.push(field);
  return fields;
}

function importCatalogCsv(catalog, text) {
  const [header, ...rows] = text.trim().split(/\r?\n/);
  const columns = parseCsvLine(header);
  const expected = ['sku', 'name', 'price', 'weight_grams', 'tags'];
  if (columns.join(',') !== expected.join(',')) {
    throw new ShopError('bad_csv', `expected columns ${expected.join(',')}`);
  }
  const added = [];
  rows.forEach((row, index) => {
    const [sku, name, price, weight, tags] = parseCsvLine(row);
    try {
      added.push(
        catalog.add({
          sku,
          name,
          price: toCents(price),
          weightGrams: Number(weight),
          tags: tags ? tags.split('|') : [],
        })
      );
    } catch (err) {
      throw new ShopError(err.code ?? 'bad_csv', `row ${index + 2}: ${err.message}`);
    }
  });
  return added;
}

function exportOrdersCsv(orders) {
  const rows = [['id', 'date', 'status', 'country', 'items', 'total']];
  for (const order of orders) {
    rows.push([
      order.id,
      order.date,
      order.status,
      order.country,
      String(order.items.reduce((n, line) => n + line.qty, 0)),
      (order.grandTotal / 100).toFixed(2),
    ]);
  }
  return rows.map((r) => r.join(',')).join('\n') + '\n';
}

module.exports = {
  CURRENCIES,
  TAX_RATES,
  SHIPPING_ZONES,
  ShopError,
  toCents,
  formatMoney,
  percentOf,
  message,
  slugify,
  Catalog,
  Stock,
  Cart,
  couponDiscount,
  applyCoupon,
  tierDiscount,
  taxRate,
  priceCart,
  zoneFor,
  shippingCost,
  shippingLabel,
  OrderBook,
  dailySales,
  topProducts,
  averageOrderValue,
  couponUsage,
  GiftCards,
  Loyalty,
  Reviews,
  AuditLog,
  parseCsvLine,
  importCatalogCsv,
  exportOrdersCsv,
};
