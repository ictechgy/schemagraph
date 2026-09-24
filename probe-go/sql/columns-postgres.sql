-- 관계 하나의 컬럼을 읽는다. 테이블·뷰는 information_schema 권한 규칙 그대로다.
-- materialized view는 information_schema.columns에 없어 pg_attribute에서 같은
-- 권한 조건·같은 data_type 표기로 읽는다. 빠지면 가시성 검사가 소유자 scan에서도
-- MV 컬럼을 미수집으로 세어 카탈로그 전체를 불완전으로 만든다.
SELECT column_name::text AS column_name, data_type::text AS data_type,
       is_nullable::text AS is_nullable, column_default::text AS column_default,
       ordinal_position::int4 AS ordinal_position
FROM information_schema.columns
WHERE table_schema = :schema AND table_name = :table
UNION ALL
SELECT a.attname::text,
       CASE WHEN t.typtype = 'd' THEN
              CASE WHEN bt.typelem <> 0 AND bt.typlen = -1 THEN 'ARRAY'
                   WHEN nbt.nspname = 'pg_catalog' THEN pg_catalog.format_type(t.typbasetype, NULL)
                   ELSE 'USER-DEFINED' END
            ELSE
              CASE WHEN t.typelem <> 0 AND t.typlen = -1 THEN 'ARRAY'
                   WHEN nt.nspname = 'pg_catalog' THEN pg_catalog.format_type(a.atttypid, NULL)
                   ELSE 'USER-DEFINED' END
       END,
       CASE WHEN a.attnotnull OR (t.typtype = 'd' AND t.typnotnull) THEN 'NO' ELSE 'YES' END,
       NULL::text,
       a.attnum::int4
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
JOIN pg_catalog.pg_namespace nt ON nt.oid = t.typnamespace
LEFT JOIN (pg_catalog.pg_type bt JOIN pg_catalog.pg_namespace nbt ON nbt.oid = bt.typnamespace)
  ON t.typtype = 'd' AND bt.oid = t.typbasetype
WHERE n.nspname = :schema AND c.relname = :table AND c.relkind = 'm'
  AND a.attnum > 0 AND NOT a.attisdropped
  AND (pg_catalog.pg_has_role(c.relowner, 'USAGE')
       OR pg_catalog.has_column_privilege(c.oid, a.attnum, 'SELECT, INSERT, UPDATE, REFERENCES'))
ORDER BY ordinal_position
