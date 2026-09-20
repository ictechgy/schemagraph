-- DB 링크 이름도 보존해 엔진이 로컬 동명 객체로 오귀속하지 않게 한다.
SELECT OWNER AS source_schema, NAME AS source_name,
       CASE TYPE WHEN 'VIEW' THEN 'view' WHEN 'MATERIALIZED VIEW' THEN 'materialized-view'
            WHEN 'FUNCTION' THEN 'function' WHEN 'PROCEDURE' THEN 'procedure'
            WHEN 'SYNONYM' THEN 'synonym' ELSE 'package' END AS source_kind,
       CAST(NULL AS VARCHAR2(4000)) AS source_signature,
       REFERENCED_OWNER AS target_schema, REFERENCED_NAME AS target_name,
       CASE REFERENCED_TYPE WHEN 'TABLE' THEN 'table' WHEN 'VIEW' THEN 'view'
            WHEN 'MATERIALIZED VIEW' THEN 'materialized-view' WHEN 'FUNCTION' THEN 'function'
            WHEN 'PROCEDURE' THEN 'procedure' WHEN 'PACKAGE' THEN 'package'
            WHEN 'SYNONYM' THEN 'synonym' WHEN 'SEQUENCE' THEN 'sequence' WHEN 'TYPE' THEN 'type' END AS target_kind,
       CAST(NULL AS VARCHAR2(4000)) AS target_signature,
       CAST(NULL AS VARCHAR2(128)) AS target_member,
       REFERENCED_LINK_NAME AS target_database, DEPENDENCY_TYPE AS dependency_type
FROM ALL_DEPENDENCIES
WHERE OWNER=:schema
  AND TYPE IN ('VIEW','MATERIALIZED VIEW','FUNCTION','PROCEDURE','PACKAGE','PACKAGE BODY','SYNONYM')
  AND REFERENCED_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','FUNCTION','PROCEDURE','PACKAGE','SYNONYM','SEQUENCE','TYPE')
  AND REFERENCED_OWNER NOT IN ('SYS','SYSTEM','PUBLIC')
ORDER BY source_name, source_kind, target_schema, target_name, target_kind
