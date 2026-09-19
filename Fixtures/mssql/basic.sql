-- MSSQL fixture — sqlite/mysql/postgres와 같은 모양: 순환 FK(a↔b), 자기
-- 참조(loop_self), view, trigger, procedure, function, 고립 테이블.
-- CREATE TRIGGER/PROCEDURE/FUNCTION은 배치 첫 문장이어야 해 GO로 끊는다.

CREATE TABLE customers (
    id   INT PRIMARY KEY,
    name NVARCHAR(100) NOT NULL
);

CREATE TABLE orders (
    id          INT PRIMARY KEY,
    customer_id INT NOT NULL,
    total       DECIMAL(10,2) NOT NULL,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
);

CREATE TABLE products (
    id  INT PRIMARY KEY,
    sku NVARCHAR(50) NOT NULL
);
CREATE UNIQUE INDEX idx_products_sku ON products(sku);

CREATE TABLE order_items (
    id         INT PRIMARY KEY,
    order_id   INT NOT NULL,
    product_id INT NOT NULL,
    CONSTRAINT order_items_fk_orders FOREIGN KEY (order_id) REFERENCES orders(id),
    CONSTRAINT order_items_fk_products FOREIGN KEY (product_id) REFERENCES products(id)
);

-- T-SQL은 존재하지 않는 테이블을 참조하는 FK를 거부한다 — 순환 FK는
-- 테이블을 먼저 만들고 ALTER TABLE로 얹는다.
CREATE TABLE a (
    id   INT PRIMARY KEY,
    b_id INT NULL
);

CREATE TABLE b (
    id   INT PRIMARY KEY,
    a_id INT NULL
);
GO
ALTER TABLE a ADD CONSTRAINT a_b_fk FOREIGN KEY (b_id) REFERENCES b(id);
ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a(id);

CREATE TABLE loop_self (
    id        INT PRIMARY KEY,
    parent_id INT NULL,
    CONSTRAINT loop_self_fk FOREIGN KEY (parent_id) REFERENCES loop_self(id)
);

CREATE TABLE standalone (
    id   INT PRIMARY KEY,
    note NVARCHAR(100) NULL
);

CREATE TABLE tickets (
    id   INT PRIMARY KEY,
    note NVARCHAR(100) NULL
);
GO

CREATE VIEW order_totals AS
    SELECT o.id AS order_id, c.name AS customer_name, o.total
    FROM orders o JOIN customers c ON c.id = o.customer_id;
GO

-- trigger: INSERT 시 고객 행을 건드린다 — inserted 의사 테이블은
-- 정점이 아니라 간선은 UPDATE 대상 customers로만 간다.
CREATE TRIGGER trg_orders_touch ON orders AFTER INSERT AS
BEGIN
    UPDATE customers SET name = name
    WHERE id IN (SELECT customer_id FROM inserted);
END;
GO

CREATE PROCEDURE touch_customer @id INT AS
BEGIN
    UPDATE customers SET name = name WHERE id = @id;
END;
GO

CREATE FUNCTION order_count() RETURNS INT AS
BEGIN
    RETURN (SELECT COUNT(*) FROM orders);
END;
GO
