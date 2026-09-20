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

CREATE PROCEDURE audit_orders @n INT AS
BEGIN
    INSERT INTO tickets (note) VALUES ('audit');
END;
GO

-- T-SQL 절차형 구문 커버: SET @v=, IF..BEGIN, TRY/CATCH, EXEC 호출.
CREATE PROCEDURE touch_customer @id INT AS
BEGIN
    SET NOCOUNT ON;
    DECLARE @n INT;
    SET @n = (SELECT COUNT(*) FROM customers);
    IF @n > 0
    BEGIN
        UPDATE customers SET name = name WHERE id = @id;
    END
    BEGIN TRY
        EXEC audit_orders @n;
    END TRY
    BEGIN CATCH
        INSERT INTO tickets (note) VALUES ('err');
    END CATCH
END;
GO

-- WHILE..BEGIN과 CURSOR FOR SELECT — 블록 안의 DML이 writes로 귀속돼야 한다.
CREATE PROCEDURE drain_orders AS
BEGIN
    DECLARE c CURSOR FOR SELECT id FROM customers;
    DECLARE @i INT = 0;
    WHILE @i < 1
    BEGIN
        UPDATE orders SET customer_id = @i WHERE id = @i;
        SET @i = @i + 1;
    END
END;
GO

CREATE FUNCTION order_count() RETURNS INT AS
BEGIN
    RETURN (SELECT COUNT(*) FROM orders);
END;
GO

-- EXEC와 EXECUTE의 괄호·N 리터럴을 모두 검증한다.
CREATE PROCEDURE dynamic_touch AS
BEGIN
    EXEC(N'UPDATE customers SET name = N''BEGIN; END'' WHERE id = 1');
    EXECUTE(N'SELECT id FROM orders');
END;
GO

CREATE PROCEDURE dynamic_concat AS
BEGIN
    DECLARE @sql NVARCHAR(MAX) = N'UPDATE ' + N'customers' + N' SET name = name';
    EXEC(@sql);
END;
GO

CREATE PROCEDURE dynamic_reassign AS
BEGIN
    DECLARE @sql NVARCHAR(MAX) = N'DELETE FROM customers';
    SET @sql = N'DELETE FROM orders';
    EXEC(@sql);
END;
GO

-- 분기 뒤의 값은 어느 경로인지 모르면 보수적으로 미추출한다.
CREATE PROCEDURE dynamic_branch @flag INT AS
BEGIN
    DECLARE @sql NVARCHAR(MAX) = N'DELETE FROM customers';
    IF @flag = 1
    BEGIN
        SET @sql = N'DELETE FROM orders';
    END
    EXEC(@sql);
END;
GO

-- 실행할 이름에 변수가 섞이면 리터럴 접두부를 완성된 SQL로 보지 않는다.
CREATE PROCEDURE dynamic_cleanup @suffix NVARCHAR(32) AS
BEGIN
    EXEC(N'DELETE FROM customers' + @suffix);
END;
GO

EXEC dynamic_touch;
EXEC dynamic_concat;
EXEC dynamic_reassign;
EXEC dynamic_branch 0;
EXEC dynamic_cleanup N'';
GO
