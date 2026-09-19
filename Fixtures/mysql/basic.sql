-- schemagraph 검증 fixture (mysql): sqlite/postgres basic과 같은 모양 +
-- MySQL 특유 요소(trigger BEGIN..END, procedure, function)를 더한다.
-- Scripts/verify-fixtures.sh가 mysql 클라이언트로 이 파일을 적용한다 —
-- DELIMITER는 클라이언트 명령이라 psql류가 아닌 mysql로만 먹인다.
-- 재실행해도 되도록 먼저 전부 지운다 — SG_MYSQL_URL은 폐기용 DB여야 한다.

DROP TABLE IF EXISTS order_items, orders, tickets, standalone, loop_self, a, b, products, customers;
DROP VIEW IF EXISTS order_totals;
DROP TRIGGER IF EXISTS trg_orders_touch;
DROP PROCEDURE IF EXISTS touch_customer;
DROP FUNCTION IF EXISTS order_count;

CREATE TABLE customers (
    id INT PRIMARY KEY,
    name VARCHAR(100) NOT NULL
);

CREATE TABLE products (
    id INT PRIMARY KEY,
    sku VARCHAR(50) UNIQUE NOT NULL
);

CREATE TABLE orders (
    id INT PRIMARY KEY,
    customer_id INT NOT NULL,
    total DECIMAL(10,2) NOT NULL DEFAULT 0,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
);

CREATE TABLE order_items (
    id INT PRIMARY KEY,
    order_id INT NOT NULL,
    product_id INT NOT NULL,
    FOREIGN KEY (order_id) REFERENCES orders(id),
    FOREIGN KEY (product_id) REFERENCES products(id)
);

-- a <-> b: object 레벨 순환. 테이블을 먼저 만들고 FK를 나중에 붙인다.
CREATE TABLE a (
    id INT PRIMARY KEY,
    b_id INT
);

CREATE TABLE b (
    id INT PRIMARY KEY,
    a_id INT
);

ALTER TABLE a ADD CONSTRAINT a_b_fk FOREIGN KEY (b_id) REFERENCES b(id);
ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a(id);

-- loop_self: 자기 참조 FK (크기 1 순환).
CREATE TABLE loop_self (
    id INT PRIMARY KEY,
    parent_id INT,
    FOREIGN KEY (parent_id) REFERENCES loop_self(id)
);

-- 고립 테이블.
CREATE TABLE standalone (
    id INT PRIMARY KEY,
    note VARCHAR(200)
);

CREATE INDEX idx_orders_customer ON orders(customer_id);

-- AUTO_INCREMENT는 PK를 만든다(MySQL엔 시퀀스 객체가 없다 — MariaDB만).
CREATE TABLE tickets (
    id INT AUTO_INCREMENT PRIMARY KEY,
    note VARCHAR(100)
);

DELIMITER //

-- 트리거: BEGIN..END 몸체 — 파서가 껍질을 벗겨 안쪽 UPDATE를 파싱한다.
CREATE TRIGGER trg_orders_touch
AFTER INSERT ON orders
FOR EACH ROW
BEGIN
    UPDATE customers SET name = name WHERE id = NEW.customer_id;
END//

-- 프로시저: 단일 문장 몸체도 파싱된다 — writes 간선이 나와야 한다.
CREATE PROCEDURE touch_customer(IN cid INT)
    UPDATE customers SET name = name WHERE id = cid//

-- 함수: RETURN이 항상 들어가서 지금 파서는 몸체 파싱에 실패한다 —
-- 성공을 위장하지 않고 limitation이 나오는지 검증하는 대상이다.
CREATE FUNCTION order_count() RETURNS INT
    READS SQL DATA
    RETURN (SELECT COUNT(*) FROM orders)//

DELIMITER ;

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.name AS customer_name, o.total
FROM orders o JOIN customers c ON c.id = o.customer_id;
