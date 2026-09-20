-- 저장된 SQL 식에 기록된 사용자 view/routine의 이름 의존성만 전달한다.
SELECT ss.name AS source_schema, s.name AS source_name,
       CASE WHEN s.type='V' THEN 'view' WHEN s.type IN ('P','PC') THEN 'procedure' ELSE 'function' END AS source_kind,
       CAST(NULL AS nvarchar(4000)) AS source_signature,
       COALESCE(d.referenced_schema_name, ts.name) AS target_schema,
       COALESCE(d.referenced_entity_name, t.name) AS target_name,
       CASE WHEN t.type='V' THEN 'view' WHEN t.type IN ('P','PC') THEN 'procedure'
            WHEN t.type IN ('FN','IF','TF','FS','FT') THEN 'function' WHEN t.type='SN' THEN 'synonym'
            WHEN t.type='SO' THEN 'sequence' WHEN t.type='U' THEN 'table' ELSE NULL END AS target_kind,
       CAST(NULL AS nvarchar(4000)) AS target_signature,
       tc.name AS target_member, d.referenced_database_name AS target_database,
       CASE WHEN d.is_schema_bound_reference=1 THEN 'schema-bound' ELSE 'by-name' END AS dependency_type
FROM sys.sql_expression_dependencies d
JOIN sys.objects s ON s.object_id=d.referencing_id
JOIN sys.schemas ss ON ss.schema_id=s.schema_id
LEFT JOIN sys.objects t ON t.object_id=d.referenced_id
LEFT JOIN sys.schemas ts ON ts.schema_id=t.schema_id
LEFT JOIN sys.columns tc ON tc.object_id=d.referenced_id AND tc.column_id=d.referenced_minor_id AND d.referenced_minor_id>0
WHERE ss.name=:schema AND s.type IN ('V','P','PC','FN','IF','TF','FS','FT')
  AND d.referenced_class=1 AND d.referenced_server_name IS NULL
  AND COALESCE(d.referenced_schema_name,ts.name) IS NOT NULL
  AND COALESCE(d.referenced_schema_name,ts.name) NOT IN ('sys','INFORMATION_SCHEMA')
ORDER BY source_name, target_schema, target_name, target_member
