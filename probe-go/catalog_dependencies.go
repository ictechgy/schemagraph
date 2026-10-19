package main

import (
	"database/sql"
	"embed"
	"fmt"
	"sort"
	"strings"
)

//go:embed sql/catalog-*.sql sql/visibility-postgres.sql sql/columns-postgres.sql
var catalogQueries embed.FS

// CollectionContext는 인증정보 없이 논리 DB와 수집 범위를 전달한다.
type CollectionContext struct {
	SourceID        string    `json:"source_id"`
	Database        *string   `json:"database,omitempty"`
	SchemaFilter    *[]string `json:"schema_filter,omitempty"`
	CatalogComplete bool      `json:"catalog_complete"`
}

// CatalogObjectRef는 DB가 제공한 이름이다. 그래프 id를 프로브에서 만들지 않는다.
type CatalogObjectRef struct {
	Schema    string  `json:"schema"`
	Name      string  `json:"name"`
	Kind      *string `json:"kind,omitempty"`
	Member    *string `json:"member,omitempty"`
	Signature *string `json:"signature,omitempty"`
	Database  *string `json:"database,omitempty"`
}

// CatalogDependency는 참조 종류 원문을 보존해 읽기/쓰기 판단을 엔진에 남긴다.
type CatalogDependency struct {
	Source         CatalogObjectRef `json:"source"`
	Target         CatalogObjectRef `json:"target"`
	Catalog        string           `json:"catalog"`
	DependencyType string           `json:"dependency_type"`
}

func validSourceID(value string) bool {
	if len(value) == 0 || len(value) > 128 {
		return false
	}
	for _, r := range value {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || strings.ContainsRune("_-./", r)) {
			return false
		}
	}
	return true
}

func (h *harvester) prepareCollectionContext() {
	if h.contextReady {
		return
	}
	h.contextReady = true
	if h.dialect == "sqlite" {
		value := "main"
		h.databaseName = &value
		return
	}
	query := ""
	switch h.dialect {
	case "postgres":
		query = "SELECT current_database()"
	case "mysql":
		query = "SELECT DATABASE()"
	case "sqlserver":
		query = "SELECT DB_NAME()"
	case "oracle":
		query = "SELECT SYS_CONTEXT('USERENV','DB_NAME') FROM dual"
	}
	if query == "" {
		return
	}
	var database sql.NullString
	if err := h.db.QueryRow(query).Scan(&database); err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, "database identity metadata unavailable")
		return
	}
	h.databaseName = ns(database)
}

func (h *harvester) collectionContext(complete bool) *CollectionContext {
	h.prepareCollectionContext()
	var filter *[]string
	if len(h.schemaFilter) > 0 {
		names := make([]string, 0, len(h.schemaFilter))
		for name := range h.schemaFilter {
			names = append(names, name)
		}
		sort.Strings(names)
		filter = &names
	}
	return &CollectionContext{SourceID: h.sourceID, Database: h.databaseName, SchemaFilter: filter, CatalogComplete: complete && !h.catalogIncomplete}
}

func (h *harvester) readCatalogDependencies(schema string) []CatalogDependency {
	catalog, placeholder := "", ""
	switch h.dialect {
	case "postgres":
		catalog = "pg_depend"
		placeholder = "$1"
	case "sqlserver":
		catalog = "sys.sql_expression_dependencies"
		placeholder = "@p1"
	case "oracle":
		catalog = "ALL_DEPENDENCIES"
		placeholder = ":1"
	default:
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, fmt.Sprintf("catalog dependency collection is unsupported for %s", h.dialect))
		return nil
	}
	raw, err := catalogQueries.ReadFile("sql/catalog-" + h.dialect + ".sql")
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations, "bundled dependency query is missing; rebuild the probe")
		return nil
	}
	lines := []string{}
	for _, line := range strings.Split(string(raw), "\n") {
		if !strings.HasPrefix(strings.TrimSpace(line), "--") {
			lines = append(lines, line)
		}
	}
	query := strings.ReplaceAll(strings.Join(lines, "\n"), ":schema", placeholder)
	dependencies := []CatalogDependency{}
	h.bestEffortArgs(catalog, query, []any{schema}, func(rows *sql.Rows) error {
		var ss, sn, sk, sg, ts, tn, tk, tg, tm, td, dt sql.NullString
		if err := rows.Scan(&ss, &sn, &sk, &sg, &ts, &tn, &tk, &tg, &tm, &td, &dt); err != nil {
			return err
		}
		if !ss.Valid || !sn.Valid || !ts.Valid || !tn.Valid {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, catalog+" returned a dependency without a source or target identity")
			return nil
		}
		dependencies = append(dependencies, CatalogDependency{Source: CatalogObjectRef{Schema: ss.String, Name: sn.String, Kind: ns(sk), Signature: ns(sg)}, Target: CatalogObjectRef{Schema: ts.String, Name: tn.String, Kind: ns(tk), Signature: ns(tg), Member: ns(tm), Database: ns(td)}, Catalog: catalog, DependencyType: dt.String})
		return nil
	})
	sortDependencies(dependencies)
	return dependencies
}

// keyedDependency는 정렬 키를 미리 만들어 둔 의존성이다.
type keyedDependency struct {
	key        string
	dependency CatalogDependency
}

// sortDependencies는 의존성을 dependencyKey 순으로 제자리 정렬한다. 키는 원소마다 한 번만 만든다.
// 조회의 ORDER BY는 DB 정렬 규칙(SQL Server의 대소문자 무시 정렬 등)을 따라 바이트 순서와 어긋날
// 수 있다. 그렇게 섞인 입력에서 비교 함수가 키(필드 14개 Join)를 매번 만들면 의존성 D개에 키
// 조립이 약 2·D·log2 D번 일어나, 수십만 행이면 정렬에만 초 단위가 든다. 비교 결과와 알고리즘이
// 예전과 같으므로 같은 입력의 출력 순서도 같다.
func sortDependencies(dependencies []CatalogDependency) {
	keyed := make([]keyedDependency, len(dependencies))
	for i, dependency := range dependencies {
		keyed[i] = keyedDependency{key: dependencyKey(dependency), dependency: dependency}
	}
	sort.Slice(keyed, func(i, j int) bool { return keyed[i].key < keyed[j].key })
	for i := range keyed {
		dependencies[i] = keyed[i].dependency
	}
}

func (h *harvester) streamDependencies(stream *ndjsonStreamWriter, schema string) error {
	if !h.catalogDependencies {
		return nil
	}
	for _, dependency := range h.readCatalogDependencies(schema) {
		if err := stream.line(map[string]any{"type": "dependency", "data": dependency}); err != nil {
			return err
		}
	}
	return nil
}

// 카탈로그 이름에는 NUL이 없으므로 구분자로 써도 필드 경계가 섞이지 않는다.
func dependencyKey(d CatalogDependency) string {
	value := func(p *string) string {
		if p == nil {
			return "0"
		}
		return "1" + *p
	}
	return strings.Join([]string{d.Source.Schema, d.Source.Name, value(d.Source.Kind), value(d.Source.Signature), value(d.Source.Member), value(d.Source.Database), d.Target.Schema, d.Target.Name, value(d.Target.Kind), value(d.Target.Signature), value(d.Target.Member), value(d.Target.Database), d.Catalog, d.DependencyType}, "\x00")
}
