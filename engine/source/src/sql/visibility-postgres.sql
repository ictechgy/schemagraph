-- information_schema의 권한 필터와 독립인 원본 카탈로그 이름을 비교한다.
SELECT c.relname AS relation_name, a.attname AS column_name
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_attribute a
  ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
WHERE n.nspname = :schema AND c.relkind IN ('r', 'p', 'f', 'v', 'm')
ORDER BY c.relname, a.attnum
