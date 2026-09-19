-- schemagraph 검증 fixture: FK 체인 + 순환 + 자기참조 + 뷰 + 트리거.
-- Scripts/verify-fixtures.sh가 sqlite3로 이 파일을 적용해 골든과 비교한다.

CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE products (
    id INTEGER PRIMARY KEY,
    sku TEXT UNIQUE NOT NULL
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    total REAL NOT NULL DEFAULT 0
);

CREATE TABLE order_items (
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL REFERENCES orders(id),
    product_id INTEGER NOT NULL REFERENCES products(id)
);

-- a <-> b: object 레벨 순환.
CREATE TABLE a (
    id INTEGER PRIMARY KEY,
    b_id INTEGER REFERENCES b(id)
);

CREATE TABLE b (
    id INTEGER PRIMARY KEY,
    a_id INTEGER REFERENCES a(id)
);

-- loop_self: 자기 참조 FK (크기 1 순환).
CREATE TABLE loop_self (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES loop_self(id)
);

-- 고립 테이블: 어떤 의존성에도 안 끼는 정상 정점.
CREATE TABLE standalone (
    id INTEGER PRIMARY KEY,
    note TEXT
);

-- 선언되지 않은 참조: customer_id에 REFERENCES가 없다 — inferred
-- 휴리스틱(--inferred)의 이름 규칙 추정 대상이다.
CREATE TABLE shipments (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER,
    note TEXT
);

CREATE INDEX idx_orders_customer ON orders(customer_id);

CREATE TRIGGER trg_orders_touch
AFTER INSERT ON orders
BEGIN
    UPDATE customers SET name = name WHERE id = NEW.customer_id;
END;

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.name AS customer_name, o.total
FROM orders o JOIN customers c ON c.id = o.customer_id;
