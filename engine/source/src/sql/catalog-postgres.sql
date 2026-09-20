-- 스키마 이름은 수집기가 바인딩한다. 읽기/쓰기 의미는 이 쿼리에서 추론하지 않는다.
SELECT * FROM (
    SELECT sn.nspname AS source_schema, s.relname AS source_name,
           CASE s.relkind WHEN 'm' THEN 'materialized-view' ELSE 'view' END AS source_kind,
           NULL::text AS source_signature,
           tn.nspname AS target_schema, t.relname AS target_name,
           CASE t.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized-view'
                WHEN 'S' THEN 'sequence' ELSE 'table' END AS target_kind,
           NULL::text AS target_signature, a.attname AS target_member,
           NULL::text AS target_database, d.deptype::text AS dependency_type
    FROM pg_depend d
    JOIN pg_rewrite r ON d.classid='pg_rewrite'::regclass AND d.objid=r.oid
    JOIN pg_class s ON s.oid=r.ev_class
    JOIN pg_namespace sn ON sn.oid=s.relnamespace
    JOIN pg_class t ON d.refclassid='pg_class'::regclass AND d.refobjid=t.oid
    JOIN pg_namespace tn ON tn.oid=t.relnamespace
    LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=d.refobjsubid AND d.refobjsubid>0
    WHERE s.relkind IN ('v','m') AND t.relkind IN ('r','p','f','v','m','S')
      AND d.deptype='n' AND tn.nspname NOT IN ('pg_catalog','information_schema')
    UNION ALL
    SELECT sn.nspname, s.relname,
           CASE s.relkind WHEN 'm' THEN 'materialized-view' ELSE 'view' END,
           NULL::text, pn.nspname, p.proname,
           CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END,
           pg_get_function_identity_arguments(p.oid), NULL::text, NULL::text, d.deptype::text
    FROM pg_depend d
    JOIN pg_rewrite r ON d.classid='pg_rewrite'::regclass AND d.objid=r.oid
    JOIN pg_class s ON s.oid=r.ev_class
    JOIN pg_namespace sn ON sn.oid=s.relnamespace
    JOIN pg_proc p ON d.refclassid='pg_proc'::regclass AND d.refobjid=p.oid
    JOIN pg_namespace pn ON pn.oid=p.pronamespace
    WHERE s.relkind IN ('v','m') AND p.prokind IN ('f','p') AND d.deptype='n'
      AND pn.nspname NOT IN ('pg_catalog','information_schema')
    UNION ALL
    SELECT sn.nspname, s.proname,
           CASE s.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END,
           pg_get_function_identity_arguments(s.oid), tn.nspname, t.relname,
           CASE t.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized-view'
                WHEN 'S' THEN 'sequence' ELSE 'table' END,
           NULL::text, a.attname, NULL::text, d.deptype::text
    FROM pg_depend d
    JOIN pg_proc s ON d.classid='pg_proc'::regclass AND d.objid=s.oid
    JOIN pg_namespace sn ON sn.oid=s.pronamespace
    JOIN pg_class t ON d.refclassid='pg_class'::regclass AND d.refobjid=t.oid
    JOIN pg_namespace tn ON tn.oid=t.relnamespace
    LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=d.refobjsubid AND d.refobjsubid>0
    WHERE s.prokind IN ('f','p') AND t.relkind IN ('r','p','f','v','m','S')
      AND d.deptype='n' AND tn.nspname NOT IN ('pg_catalog','information_schema')
) dependencies
WHERE source_schema=:schema
ORDER BY source_name, source_kind, source_signature, target_schema, target_name, target_member
