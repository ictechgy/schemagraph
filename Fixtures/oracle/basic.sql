-- Oracle fixture — sqlite/mysql/postgres/mssql과 같은 모양: 순환 FK(a↔b),
-- 자기 참조(loop_self), view, trigger, procedure, function, 고립 테이블.
-- JDBC는 한 번에 한 문장만 실행하므로 모든 문장을 / 로 끊는다(sqlplus 관례).

CREATE TABLE customers (
    id   NUMBER PRIMARY KEY,
    name VARCHAR2(100) NOT NULL
);
/
CREATE TABLE orders (
    id          NUMBER PRIMARY KEY,
    customer_id NUMBER NOT NULL,
    total       NUMBER(10,2) NOT NULL,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
);
/
CREATE TABLE products (
    id  NUMBER PRIMARY KEY,
    sku VARCHAR2(50) NOT NULL
);
/
CREATE UNIQUE INDEX idx_products_sku ON products(sku);
/
CREATE TABLE order_items (
    id         NUMBER PRIMARY KEY,
    order_id   NUMBER NOT NULL,
    product_id NUMBER NOT NULL,
    CONSTRAINT order_items_fk_orders FOREIGN KEY (order_id) REFERENCES orders(id),
    CONSTRAINT order_items_fk_products FOREIGN KEY (product_id) REFERENCES products(id)
);
/
-- Oracle도 존재하지 않는 테이블을 참조하는 FK를 거부한다 — 순환 FK는
-- 테이블을 먼저 만들고 ALTER TABLE로 얹는다.
CREATE TABLE a (id NUMBER PRIMARY KEY, b_id NUMBER);
/
CREATE TABLE b (id NUMBER PRIMARY KEY, a_id NUMBER);
/
ALTER TABLE a ADD CONSTRAINT a_b_fk FOREIGN KEY (b_id) REFERENCES b(id);
/
ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a(id);
/
CREATE TABLE loop_self (
    id        NUMBER PRIMARY KEY,
    parent_id NUMBER CONSTRAINT loop_self_fk REFERENCES loop_self(id)
);
/
CREATE TABLE standalone (
    id   NUMBER PRIMARY KEY,
    note VARCHAR2(100)
);
/
CREATE TABLE tickets (
    id   NUMBER PRIMARY KEY,
    note VARCHAR2(100)
);
/
CREATE VIEW order_totals AS
    SELECT o.id AS order_id, c.name AS customer_name, o.total
    FROM orders o JOIN customers c ON c.id = o.customer_id;
/
-- trigger: INSERT 시 고객 행을 건드린다 — :NEW 바인드 변수는 정점이 아니라
-- 간선은 UPDATE 대상 customers로만 간다.
CREATE OR REPLACE TRIGGER trg_orders_touch
    AFTER INSERT ON orders FOR EACH ROW
BEGIN
    UPDATE customers SET name = name WHERE id = :NEW.customer_id;
END;
/
CREATE OR REPLACE PROCEDURE touch_customer(cid IN NUMBER) AS
BEGIN
    UPDATE customers SET name = name WHERE id = cid;
END;
/
CREATE OR REPLACE FUNCTION order_count RETURN NUMBER AS
    n NUMBER;
BEGIN
    SELECT COUNT(*) INTO n FROM orders;
    RETURN n;
END;
/
