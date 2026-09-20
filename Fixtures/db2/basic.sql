-- Db2 LUW 실제 카탈로그·몸체 검증용 fixture. 폐기용 DB와 외부 JDBC 드라이버를 쓴다.
--#SET TERMINATOR @
CREATE SCHEMA SGFIX@
SET CURRENT SCHEMA SGFIX@

CREATE TABLE customers (
    id INTEGER NOT NULL PRIMARY KEY,
    name VARCHAR(80) NOT NULL
 )@

CREATE TABLE orders (
    id INTEGER NOT NULL PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    total DECIMAL(12,2) NOT NULL,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
 )@

INSERT INTO customers(id, name) VALUES (1, 'fixture')@

CREATE VIEW order_totals AS
    SELECT c.id AS customer_id, c.name, SUM(o.total) AS total
    FROM customers c JOIN orders o ON o.customer_id = c.id
    GROUP BY c.id, c.name@

CREATE TRIGGER orders_touch
    AFTER INSERT ON orders
    REFERENCING NEW AS n
    FOR EACH ROW MODE DB2SQL
    UPDATE customers SET name = 'observed' WHERE id = n.customer_id@

CREATE FUNCTION add_one(value INTEGER)
    RETURNS INTEGER
    LANGUAGE SQL
    RETURN value + 1@

CREATE FUNCTION overloaded(value INTEGER)
    RETURNS INTEGER
    LANGUAGE SQL
    RETURN value@

CREATE FUNCTION overloaded(value VARCHAR(80))
    RETURNS VARCHAR(80)
    LANGUAGE SQL
    RETURN value@

CREATE FUNCTION external_marker(value INTEGER)
    RETURNS INTEGER
    LANGUAGE C
    PARAMETER STYLE SQL
    NO SQL
    EXTERNAL NAME 'sgfix!external_marker'@

CREATE PROCEDURE touch_customer(IN customer INTEGER)
    LANGUAGE SQL
    MODIFIES SQL DATA
    UPDATE customers SET name = 'touched' WHERE id = customer@

CREATE SEQUENCE order_sequence AS INTEGER START WITH 1 INCREMENT BY 1@
