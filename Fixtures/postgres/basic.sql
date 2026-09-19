-- schemagraph 검증 fixture (postgres): sqlite basic과 같은 모양 +
-- PG 전용 요소(function 몸체, 시퀀스)를 더한다.
-- Scripts/verify-fixtures.sh가 psql로 이 파일을 적용해 골든과 비교한다.
-- 재실행해도 되도록 먼저 전부 지운다 — SG_PG_URL은 폐기용 DB여야 한다.

DROP SCHEMA public CASCADE;
CREATE SCHEMA public;

CREATE TABLE customers (
    id integer PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE products (
    id integer PRIMARY KEY,
    sku text UNIQUE NOT NULL
);

CREATE TABLE orders (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES customers(id),
    total numeric NOT NULL DEFAULT 0
);

CREATE TABLE order_items (
    id integer PRIMARY KEY,
    order_id integer NOT NULL REFERENCES orders(id),
    product_id integer NOT NULL REFERENCES products(id)
);

-- a <-> b: object 레벨 순환. PG는 선언 시점에 대상이 있어야 하므로
-- 테이블을 먼저 만들고 FK를 나중에 붙인다.
CREATE TABLE a (
    id integer PRIMARY KEY,
    b_id integer
);

CREATE TABLE b (
    id integer PRIMARY KEY,
    a_id integer
);

ALTER TABLE a ADD CONSTRAINT a_b_fk FOREIGN KEY (b_id) REFERENCES b(id);
ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a(id);

-- loop_self: 자기 참조 FK (크기 1 순환).
CREATE TABLE loop_self (
    id integer PRIMARY KEY,
    parent_id integer REFERENCES loop_self(id)
);

-- 고립 테이블.
CREATE TABLE standalone (
    id integer PRIMARY KEY,
    note text
);

CREATE INDEX idx_orders_customer ON orders(customer_id);

-- serial은 시퀀스 정점을 만든다.
CREATE TABLE tickets (
    id serial PRIMARY KEY,
    note text
);

-- 트리거 + 함수: PG는 trigger가 함수를 호출한다.
CREATE FUNCTION trg_orders_touch_fn() RETURNS trigger AS $$
BEGIN
    UPDATE customers SET name = name WHERE id = NEW.customer_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_orders_touch
AFTER INSERT ON orders
FOR EACH ROW EXECUTE FUNCTION trg_orders_touch_fn();

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.name AS customer_name, o.total
FROM orders o JOIN customers c ON c.id = o.customer_id;
