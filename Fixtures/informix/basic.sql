-- Informix 실제 카탈로그·몸체 검증용 fixture. 폐기용 DB와 외부 JDBC 드라이버를 쓴다.
--#SET TERMINATOR @
CREATE TABLE customers (
    id SERIAL PRIMARY KEY,
    name VARCHAR(80) NOT NULL
)@

CREATE TABLE orders (
    id SERIAL PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    total DECIMAL(12,2) NOT NULL,
    FOREIGN KEY (customer_id) REFERENCES customers(id)
 )@

INSERT INTO customers(id, name) VALUES (1, 'fixture')@

CREATE VIEW order_totals (customer_id, name, total) AS
    SELECT c.id, c.name, SUM(o.total)
    FROM customers c, orders o
    WHERE o.customer_id = c.id
    GROUP BY c.id, c.name@

CREATE TRIGGER orders_touch
    INSERT ON orders
    REFERENCING NEW AS n
    FOR EACH ROW
    (UPDATE customers SET name = 'observed' WHERE id = n.customer_id)@

CREATE PROCEDURE add_one(value INTEGER)
    RETURNING INTEGER;
    RETURN value + 1;
END PROCEDURE@

CREATE FUNCTION overloaded(value INTEGER)
    RETURNING INTEGER;
    RETURN value;
END FUNCTION@

CREATE FUNCTION overloaded(value VARCHAR(80))
    RETURNING VARCHAR(80);
    RETURN value;
END FUNCTION@

CREATE FUNCTION long_literal()
    RETURNING LVARCHAR(2048);
    RETURN 'LONG_FRAGMENT_MARKER_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx';
END FUNCTION@

CREATE FUNCTION external_marker(value INTEGER)
    RETURNING INTEGER
    EXTERNAL NAME '/opt/schemagraph/external_marker'
    LANGUAGE C@

CREATE SEQUENCE order_sequence START WITH 1 INCREMENT BY 1@
