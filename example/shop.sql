-- Sample database for sqlitefuse: a small shop.
-- Build it with: sqlite3 shop.db < shop.sql

PRAGMA foreign_keys = ON;

-- Rowid table with an AUTOINCREMENT key (also creates sqlite_sequence).
CREATE TABLE customers (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT NOT NULL,
    email   TEXT COLLATE NOCASE,
    country TEXT,
    avatar  BLOB
);

INSERT INTO customers (name, email, country, avatar) VALUES
    ('María López',   'maria@example.com',  'AR', x'89504E470D0A1A0A'),
    ('José Pérez',    'jose@example.com',   'AR', NULL),
    ('Ana Souza',     'ana@example.com',    'BR', NULL),
    ('Kenji Tanaka',  NULL,                 'JP', NULL),
    ('Zoë O''Brien',  'zoe@example.com',    'IE', NULL);

CREATE TABLE products (
    sku         TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    price       REAL NOT NULL,
    stock       INTEGER NOT NULL DEFAULT 0,
    -- Generated column: shows up in the JSON like any other column.
    stock_value REAL GENERATED ALWAYS AS (price * stock) VIRTUAL
);

INSERT INTO products (sku, name, price, stock) VALUES
    ('MUG-01',  'Ceramic mug',          12.5,  40),
    ('TEA-GRN', 'Green tea, 100 g',      8.0,  120),
    ('TEA-BLK', 'Black tea, 100 g',      7.25, 0),
    ('KTL-02',  'Electric kettle',      49.99, 7),
    ('YRB-MAT', 'Yerba mate, 1 kg',      9.9,  64),
    ('BMB-01',  'Bombilla "classic"',    4.0,  15);

-- STRICT table: column types are enforced.
CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    placed_at   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'shipped'))
) STRICT;

INSERT INTO orders (id, customer_id, placed_at, status) VALUES
    (1001, 1, '2026-11-02T10:15:00Z', 'shipped'),
    (1002, 2, '2026-11-03T18:40:00Z', 'paid'),
    (1003, 1, '2026-11-10T09:05:00Z', 'pending'),
    (1004, 3, '2026-11-11T14:22:00Z', 'shipped'),
    (1005, 4, '2026-11-15T07:58:00Z', 'paid');

-- WITHOUT ROWID table: the file is ordered by the primary key (order_id, sku).
CREATE TABLE order_items (
    order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    sku      TEXT NOT NULL REFERENCES products(sku),
    qty      INTEGER NOT NULL,
    PRIMARY KEY (order_id, sku)
) WITHOUT ROWID;

INSERT INTO order_items (order_id, sku, qty) VALUES
    (1001, 'YRB-MAT', 2), (1001, 'BMB-01', 1),
    (1002, 'TEA-GRN', 3),
    (1003, 'MUG-01', 2),  (1003, 'TEA-BLK', 1),
    (1004, 'KTL-02', 1),  (1004, 'TEA-GRN', 1),
    (1005, 'YRB-MAT', 1);

-- Views always show up, next to the tables.
CREATE VIEW order_totals AS
SELECT o.id AS order_id,
       c.name AS customer,
       o.status,
       round(sum(i.qty * p.price), 2) AS total
FROM orders o
JOIN customers c ON c.id = o.customer_id
JOIN order_items i ON i.order_id = o.id
JOIN products p ON p.sku = i.sku
GROUP BY o.id;

-- Full-text index: the virtual table is not listed, its shadow tables
-- (reviews_content, reviews_data, ...) show up as hidden files.
CREATE VIRTUAL TABLE reviews USING fts5(sku, body);
INSERT INTO reviews (sku, body) VALUES
    ('YRB-MAT', 'Great yerba, smooth and not too dusty.'),
    ('KTL-02',  'Boils fast. A bit loud.'),
    ('MUG-01',  'Nice mug, keeps tea warm.');

-- Indexes don't get files of their own. Each table lists its indexes in the
-- user.sqlite.indexes extended attribute, and they appear as rows in
-- .sqlite_schema.jsonl. products(sku) and order_items(order_id, sku) also
-- have automatic indexes for their primary keys.
CREATE UNIQUE INDEX customers_email ON customers(email);
CREATE INDEX customers_lower_name ON customers(lower(name));
CREATE INDEX orders_customer ON orders(customer_id, placed_at);
CREATE INDEX orders_pending ON orders(placed_at) WHERE status = 'pending';
CREATE INDEX order_items_sku ON order_items(sku);

-- Triggers are listed in the user.sqlite.triggers extended attribute of
-- their table.
CREATE TRIGGER order_items_stock AFTER INSERT ON order_items
BEGIN
    UPDATE products SET stock = stock - NEW.qty WHERE sku = NEW.sku;
END;

CREATE TRIGGER orders_keep_shipped BEFORE UPDATE OF status ON orders
WHEN OLD.status = 'shipped'
BEGIN
    SELECT RAISE(ABORT, 'shipped orders cannot change status');
END;

-- Statistics tables (sqlite_stat1, sqlite_stat4) also become hidden files.
ANALYZE;
