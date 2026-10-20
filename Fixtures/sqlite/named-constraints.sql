-- schemagraph 검증 fixture: 이름 있는 PK·FK, 같은 대상 FK, 다중 컬럼 FK, 인용·대소문자,
-- WITHOUT ROWID, DDL 안의 주석. Scripts/verify-fixtures.sh가 세 수집기(네이티브·Go·JDBC)의
-- 참조 관계가 같은지 본다. 제약 이름은 수집기마다 규칙이 다르다 — JDBC는 sqlite-jdbc가
-- DDL에서 읽은 이름을, 네이티브·Go는 {table}_pk·{table}_fk_{id}를 쓴다.
-- probe의 SQLiteConstraintsTest도 이 파일로 드라이버 메타데이터와 같은 행을 내는지 본다.

CREATE TABLE accounts (
    id INTEGER,
    region TEXT NOT NULL,
    CONSTRAINT pk_accounts PRIMARY KEY (id)
);

CREATE TABLE "Regions" (
    "Code" TEXT,
    "Zone" TEXT,
    CONSTRAINT "pk_Regions" PRIMARY KEY ("Code", "Zone")
) WITHOUT ROWID;

CREATE TABLE invoices (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    approver_id INTEGER,
    region_code TEXT,
    region_zone TEXT,
    -- 같은 대상(accounts)으로 가는 이름 있는 참조 두 개
    CONSTRAINT fk_invoices_account FOREIGN KEY (account_id) REFERENCES accounts(id),
    CONSTRAINT fk_invoices_approver FOREIGN KEY (approver_id) REFERENCES accounts(id),
    /* 다중 컬럼, 인용·대소문자 혼합 */
    CONSTRAINT "fk_invoices_Region"
        FOREIGN KEY (region_code, region_zone) REFERENCES "Regions"("Code", "Zone")
);

-- 같은 대상으로 가는 이름 없는 참조 두 개: JDBC는 한 제약으로 묶는다(기존 동작).
CREATE TABLE invoice_lines (
    invoice_id INTEGER NOT NULL REFERENCES invoices(id),
    line_no INTEGER NOT NULL,
    reviewer_id INTEGER REFERENCES accounts(id),
    second_reviewer_id INTEGER REFERENCES accounts(id),
    PRIMARY KEY (invoice_id, line_no)
);
