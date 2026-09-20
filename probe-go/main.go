// schemagraph-probe-go — JVM 없는 SQLite·PostgreSQL·MySQL·Oracle·SQL Server 프로브.
//
// JVM 프로브(probe/)는 번들 드라이버로 대부분의 DB를 커버하지만 Oracle만은
// ojdbc가 OTN 라이선스라 jar을 직접 내려받아 --driver로 넘겨야 했고,
// JVM 자체가 없는 환경엔 애초에 프로브가 없었다. go-ora·go-mssqldb는 pure
// Go 드라이버라 이 바이너리 하나면 JVM도 외부 jar도 없이 카탈로그를
// CatalogDocument v1로 뱉을 수 있다.
//
//	engine 쪽 소비는 JVM 프로브와 동일:
//	  schemagraph-probe-go --url oracle://u:p@host:1521/FREEPDB1 -o doc.json
//	  schemagraph-probe-go --url sqlserver://u:p@host:1433/db -o doc.json
//	  schemagraph scan --document doc.json -o graph.json
//
// 수확 의미론은 probe/Extractor.kt의 방언 분기와 1:1 대응시킨다 —
// 둘이 다른 문서를 내면 같은 DB에서 그래프가 갈라진다.
package main

import (
	"database/sql"
	"flag"
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"

	_ "github.com/microsoft/go-mssqldb"
	_ "github.com/sijms/go-ora/v2"
)

const documentVersion = 1

// ---- document.rs / probe Model.kt와 1:1 대응하는 와이어 타입 ----

type CatalogDocument struct {
	Version      int                 `json:"version"`
	Dialect      string              `json:"dialect"`
	Reader       string              `json:"reader"`
	Schemas      []SchemaDoc         `json:"schemas"`
	Limitations  []string            `json:"limitations"`
	Context      *CollectionContext  `json:"context,omitempty"`
	Dependencies []CatalogDependency `json:"dependencies,omitempty"`
}

type SchemaDoc struct {
	Name     string       `json:"name"`
	Objects  []ObjectDoc  `json:"objects"`
	Routines []RoutineDoc `json:"routines"`
}

type ObjectDoc struct {
	Name        string          `json:"name"`
	Kind        string          `json:"kind"`
	Columns     []ColumnDoc     `json:"columns"`
	Constraints []ConstraintDoc `json:"constraints"`
	Indexes     []IndexDoc      `json:"indexes"`
	Triggers    []TriggerDoc    `json:"triggers"`
	Body        *string         `json:"body,omitempty"`
	Usage       *UsageDoc       `json:"usage,omitempty"`
}

type ColumnDoc struct {
	Name       string  `json:"name"`
	DataType   string  `json:"data_type"`
	Nullable   bool    `json:"nullable"`
	Default    *string `json:"default,omitempty"`
	Ordinal    int     `json:"ordinal"`
	PkPosition int     `json:"pk_position"`
}

type ConstraintDoc struct {
	Name       string         `json:"name"`
	Kind       string         `json:"kind"`
	Columns    []string       `json:"columns"`
	Referenced *ReferencedDoc `json:"referenced,omitempty"`
}

type ReferencedDoc struct {
	Schema  *string  `json:"schema,omitempty"`
	Table   string   `json:"table"`
	Columns []string `json:"columns"`
}

type IndexDoc struct {
	Name               string    `json:"name"`
	Unique             bool      `json:"unique"`
	Columns            []string  `json:"columns"`
	Usage              *UsageDoc `json:"usage,omitempty"`
	DefinitionComplete *bool     `json:"definition_complete,omitempty"`
	HasPredicate       *bool     `json:"has_predicate,omitempty"`
	Predicate          *string   `json:"predicate,omitempty"`
}

type UsageDoc struct {
	Since   *string  `json:"since,omitempty"`
	Reads   int64    `json:"reads"`
	Writes  int64    `json:"writes"`
	TotalMs *float64 `json:"total_ms,omitempty"`
	SelfMs  *float64 `json:"self_ms,omitempty"`
}

type TriggerDoc struct {
	Name string  `json:"name"`
	Body *string `json:"body,omitempty"`
}

type RoutineDoc struct {
	Name      string    `json:"name"`
	Kind      string    `json:"kind"`
	Language  *string   `json:"language,omitempty"`
	Body      *string   `json:"body,omitempty"`
	Signature *string   `json:"signature,omitempty"`
	Usage     *UsageDoc `json:"usage,omitempty"`
	// 패키지 멤버면 부모 패키지 이름 — Kotlin RoutineDoc.memberOf와 같은 키.
	MemberOf *string `json:"member_of,omitempty"`
	Source   *string `json:"source,omitempty"`
}

func boolValue(value bool) *bool { return &value }

// Oracle 카탈로그가 노출하는 시스템 스키마 — probe/Extractor.kt의 목록과 동일.
// PUBLIC은 시노님이 수천 개라 커서를 고갈시킨다 — 항상 억제한다.
// DVF/DVSYS(Database Vault)는 실검증에서 잡음으로 확인돼 추가됐다.
var systemSchemas = map[string]bool{
	"SYS": true, "SYSTEM": true, "OUTLN": true, "DBSNMP": true, "XDB": true,
	"WMSYS": true, "CTXSYS": true, "ORDSYS": true, "MDSYS": true, "OLAPSYS": true,
	"APPQOSSYS": true, "AUDSYS": true, "GSMADMIN_INTERNAL": true, "LBACSYS": true,
	"REMOTE_SCHEDULER_AGENT": true,
	"DIP":                    true, "ORACLE_OCM": true, "PUBLIC": true, "DVF": true, "DVSYS": true,
	// MSSQL의 시스템 스키마 — sys는 Oracle의 SYS와 같은 이름이라 이미 위에 있다.
	"INFORMATION_SCHEMA": true,
}

type harvester struct {
	db                  *sql.DB
	dialect             string
	schemaFilter        map[string]bool
	limitations         []string
	activeSchema        *string
	catalogDependencies bool
	catalogIncomplete   bool
	sourceID            string
	databaseName        *string
	contextReady        bool
}

// bestEffort — 수확 실패를 limitation으로 변환한다. Kotlin Extractor의 규칙과
// 같다: 쿼리가 안 먹는 DB는 그 사실을 숨기지 않고 신고해야 한다.
func (h *harvester) bestEffort(label, query string, row func(rows *sql.Rows) error) {
	h.bestEffortArgs(label, query, nil, row)
}

func (h *harvester) bestEffortArgs(label, query string, args []any, row func(rows *sql.Rows) error) {
	rows, err := h.db.Query(query, args...)
	if err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations,
			fmt.Sprintf("%s 원문 미수확(%s) — 간선이 빠질 수 있다", label, oneLine(err.Error())))
		return
	}
	defer rows.Close()
	for rows.Next() {
		if err := row(rows); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations,
				fmt.Sprintf("%s 행 읽기 실패(%s) — 간선이 빠질 수 있다", label, oneLine(err.Error())))
			return
		}
	}
	if err := rows.Err(); err != nil {
		h.catalogIncomplete = true
		h.limitations = append(h.limitations,
			fmt.Sprintf("%s 원문 미수확(%s) — 간선이 빠질 수 있다", label, oneLine(err.Error())))
	}
}

func oneLine(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

func (h *harvester) keepSchema(s string) bool {
	return !systemSchemas[strings.ToUpper(s)] &&
		(len(h.schemaFilter) == 0 || h.schemaFilter[s])
}

func ns(s sql.NullString) *string {
	if !s.Valid {
		return nil
	}
	v := s.String
	return &v
}

func main() {
	var (
		rawURL              = flag.String("url", "", "sqlite:, postgres://, mysql://, oracle://, sqlserver://")
		schemaArg           = flag.String("schema", "", "comma-separated schema allowlist (default: non-system schemas)")
		output              = flag.String("o", "catalog.json", "output path ('-' for stdout)")
		format              = flag.String("format", "json", "json | ndjson")
		wireVersion         = flag.Int("document-version", 1, "catalog wire version: 1 or 2")
		catalogDependencies = flag.Bool("catalog-dependencies", false, "collect DB dependency catalog facts")
		sourceID            = flag.String("source-id", "", "logical source label for snapshot comparison (never a URL)")
	)
	flag.Parse()
	sourceIDProvided := false
	flag.Visit(func(f *flag.Flag) {
		if f.Name == "source-id" {
			sourceIDProvided = true
		}
	})
	if sourceIDProvided && !validSourceID(*sourceID) {
		fatal("invalid source label", fmt.Errorf("use a logical label, not a connection URL"))
	}
	if *rawURL == "" {
		fatal("connection URL is required", fmt.Errorf("set --url to a supported database URL"))
	}
	if err := validateDocumentVersion(*wireVersion); err != nil {
		fatal("invalid document version", err)
	}
	if *format != "json" && *format != "ndjson" {
		fatal("invalid output format", fmt.Errorf("choose json or ndjson"))
	}
	spec, err := parseConnection(*rawURL)
	if err != nil {
		fatal("invalid connection settings", err)
	}
	db, err := sql.Open(spec.driver, spec.dsn)
	if err != nil {
		fatal("database driver setup failed", err)
	}
	defer db.Close()
	if err := db.Ping(); err != nil {
		fatal("database connection failed", err)
	}
	h := &harvester{db: db, dialect: spec.dialect, schemaFilter: map[string]bool{}, catalogDependencies: *catalogDependencies, sourceID: *sourceID}
	h.configureReadOnly()
	if *schemaArg == "" && spec.schema != "" {
		h.schemaFilter[spec.schema] = true
	}
	for _, schema := range strings.Split(*schemaArg, ",") {
		if schema = strings.TrimSpace(schema); schema != "" {
			h.schemaFilter[schema] = true
		}
	}
	if *format == "ndjson" {
		if *output == "-" {
			if err := h.streamNDJSON(os.Stdout, *wireVersion); err != nil {
				fatal("document streaming failed", err)
			}
		} else {
			file, err := os.OpenFile(*output, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
			if err != nil {
				fatal("document output failed", err)
			}
			err = h.streamNDJSON(file, *wireVersion)
			closeErr := file.Close()
			if err != nil {
				fatal("document streaming failed", err)
			}
			if closeErr != nil {
				fatal("document output failed", closeErr)
			}
		}
		return
	}
	doc := h.extract()
	doc.Limitations = sortedDistinct(doc.Limitations)
	encoded, err := encodeDocument(doc, *wireVersion, *format)
	if err != nil {
		fatal("document serialization failed", err)
	}
	if *output == "-" {
		if _, err := os.Stdout.Write(encoded); err != nil {
			fatal("document output failed", err)
		}
	} else if err := os.WriteFile(*output, encoded, 0o644); err != nil {
		fatal("document write failed", err)
	}
}

func fatal(msg string, err error) {
	fmt.Fprintf(os.Stderr, "error: %s: %s\n", msg, oneLine(err.Error()))
	os.Exit(2)
}

func sortedDistinct(in []string) []string {
	seen := map[string]bool{}
	out := make([]string, 0, len(in))
	for _, s := range in {
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	sort.Strings(out)
	return out
}

func (h *harvester) extract() CatalogDocument {
	h.prepareCollectionContext()
	doc := h.extractCatalog()
	if h.catalogDependencies {
		for _, schema := range doc.Schemas {
			doc.Dependencies = append(doc.Dependencies, h.readCatalogDependencies(schema.Name)...)
		}
	}
	doc.Context = h.collectionContext(true)
	doc.Limitations = sortedDistinct(h.limitations)
	return doc
}

func (h *harvester) extractCatalog() CatalogDocument {
	switch h.dialect {
	case "postgres":
		return h.extractPostgres()
	case "mysql":
		return h.extractMySQL()
	case "sqlite":
		return h.extractSQLite()
	}
	if h.dialect == "sqlserver" {
		return h.extractMSSQL()
	}
	objects := h.collectObjects()
	views := h.collectViews()
	triggers := h.collectTriggers()
	routines := h.collectRoutines()

	schemaSet := map[string]bool{}
	for s := range objects {
		schemaSet[s] = true
	}
	for s := range routines {
		schemaSet[s] = true
	}
	schemas := make([]string, 0, len(schemaSet))
	for s := range schemaSet {
		schemas = append(schemas, s)
	}
	sort.Strings(schemas)

	doc := CatalogDocument{
		Version: documentVersion, Dialect: h.dialect, Reader: "probe-go",
		Schemas: []SchemaDoc{},
		// 빈 배열이 null로 직렬화되면 Kotlin 측 문서와 모양이 갈린다.
		Limitations: append([]string{}, h.limitations...),
	}
	for _, schema := range schemas {
		sd := SchemaDoc{Name: schema, Objects: []ObjectDoc{}, Routines: []RoutineDoc{}}
		for _, obj := range objects[schema] {
			if b, ok := views[schema+"."+obj.Name]; ok {
				obj.Body = &b
			}
			obj.Triggers = triggers[schema+"."+obj.Name]
			if obj.Triggers == nil {
				obj.Triggers = []TriggerDoc{}
			}
			sd.Objects = append(sd.Objects, obj)
		}
		rts := routines[schema]
		sort.Slice(rts, func(i, j int) bool {
			if rts[i].Name != rts[j].Name {
				return rts[i].Name < rts[j].Name
			}
			var a, b string
			if rts[i].Signature != nil {
				a = *rts[i].Signature
			}
			if rts[j].Signature != nil {
				b = *rts[j].Signature
			}
			return a < b
		})
		sd.Routines = rts
		if sd.Routines == nil {
			sd.Routines = []RoutineDoc{}
		}
		doc.Schemas = append(doc.Schemas, sd)
	}
	return doc
}

// collectObjects — ALL_TABLES/ALL_VIEWS/ALL_MVIEWS/ALL_SYNONYMS에서 객체를
// 수확한다. Kotlin은 JDBC getTables를 쓰지만 ojdbc가 같은 소스를 읽는다.
// TEMPORARY='Y'는 GLOBAL TEMPORARY라 Kotlin 경로에서 걸러지던 것과 맞춘다.
func (h *harvester) collectObjects() map[string][]ObjectDoc {
	type raw struct{ schema, name, kind string }
	var rows []raw
	query := `SELECT OWNER, TABLE_NAME, 'table' FROM ALL_TABLES WHERE TEMPORARY = 'N'
		 UNION ALL SELECT OWNER, VIEW_NAME, 'view' FROM ALL_VIEWS
		 UNION ALL SELECT OWNER, MVIEW_NAME, 'materialized-view' FROM ALL_MVIEWS
		 UNION ALL SELECT OWNER, SYNONYM_NAME, 'synonym' FROM ALL_SYNONYMS`
	var args []any
	if h.activeSchema != nil {
		query = `SELECT OWNER, TABLE_NAME, 'table' FROM ALL_TABLES WHERE TEMPORARY = 'N' AND OWNER = :schema
		 UNION ALL SELECT OWNER, VIEW_NAME, 'view' FROM ALL_VIEWS WHERE OWNER = :schema
		 UNION ALL SELECT OWNER, MVIEW_NAME, 'materialized-view' FROM ALL_MVIEWS WHERE OWNER = :schema
		 UNION ALL SELECT OWNER, SYNONYM_NAME, 'synonym' FROM ALL_SYNONYMS WHERE OWNER = :schema`
		args = []any{sql.Named("schema", *h.activeSchema)}
	}
	h.bestEffortArgs("objects", query, args,
		func(rs *sql.Rows) error {
			var r raw
			if err := rs.Scan(&r.schema, &r.name, &r.kind); err != nil {
				return err
			}
			if h.keepSchema(r.schema) {
				rows = append(rows, r)
			}
			return nil
		})

	columns := h.collectColumns()
	pkPos, pks := h.collectPrimaryKeys()
	fks := h.collectForeignKeys()
	indexes := h.collectIndexes()

	out := map[string][]ObjectDoc{}
	for _, r := range rows {
		key := r.schema + "." + r.name
		cols := columns[key]
		for i := range cols {
			cols[i].PkPosition = pkPos[key][cols[i].Name]
		}
		cons := append(append([]ConstraintDoc{}, pks[key]...), fks[key]...)
		sort.Slice(cons, func(i, j int) bool { return cons[i].Name < cons[j].Name })
		idx := []IndexDoc{}
		if r.kind == "table" || r.kind == "materialized-view" {
			idx = indexes[key]
		}
		if cols == nil {
			cols = []ColumnDoc{}
		}
		out[r.schema] = append(out[r.schema], ObjectDoc{
			Name: r.name, Kind: r.kind, Columns: cols,
			Constraints: cons, Indexes: idx, Triggers: []TriggerDoc{},
		})
	}
	for s := range out {
		sort.Slice(out[s], func(i, j int) bool { return out[s][i].Name < out[s][j].Name })
	}
	if len(out) == 0 {
		h.limitations = append(h.limitations,
			"카탈로그가 테이블/뷰를 하나도 주지 않았다 — 접근 권한을 확인해라")
	}
	return out
}

func (h *harvester) collectColumns() map[string][]ColumnDoc {
	// DATA_DEFAULT는 LONG이라 본문과 분리해 두 단계로 수확한다 — LONG을 못
	// 읽는 드라이버에서도 컬럼 목록은 살아남는다.
	out := map[string][]ColumnDoc{}
	query, args := h.oracleScopedQuery(`SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, COLUMN_ID
		 FROM ALL_TAB_COLUMNS ORDER BY OWNER, TABLE_NAME, COLUMN_ID`, "OWNER = :schema")
	h.bestEffortArgs("columns", query, args,
		func(rs *sql.Rows) error {
			var schema, table, name, dtype, nullable string
			var ord int
			if err := rs.Scan(&schema, &table, &name, &dtype, &nullable, &ord); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			key := schema + "." + table
			out[key] = append(out[key], ColumnDoc{
				Name: name, DataType: dtype,
				Nullable: nullable == "Y", Ordinal: ord,
			})
			return nil
		})
	defaults := map[string]*string{}
	query, args = h.oracleScopedQuery(`SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_DEFAULT FROM ALL_TAB_COLUMNS`, "OWNER = :schema")
	h.bestEffortArgs("column defaults", query, args,
		func(rs *sql.Rows) error {
			var schema, table, name string
			var def sql.NullString
			if err := rs.Scan(&schema, &table, &name, &def); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			defaults[schema+"."+table+"."+name] = ns(def)
			return nil
		})
	for key, cols := range out {
		for i := range cols {
			out[key][i].Default = defaults[key+"."+cols[i].Name]
		}
	}
	return out
}

func (h *harvester) collectPrimaryKeys() (map[string]map[string]int, map[string][]ConstraintDoc) {
	pos := map[string]map[string]int{}
	type pk struct {
		name string
		cols map[int]string
	}
	byTable := map[string]map[string]map[int]string{}
	query, args := h.oracleScopedQuery(`SELECT c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.COLUMN_NAME, cc.POSITION
		 FROM ALL_CONSTRAINTS c
		 JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER
		   AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND cc.TABLE_NAME = c.TABLE_NAME
		 WHERE c.CONSTRAINT_TYPE = 'P'
		 ORDER BY c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION`, "c.OWNER = :schema")
	h.bestEffortArgs("primary keys", query, args,
		func(rs *sql.Rows) error {
			var schema, table, cname, col string
			var p int
			if err := rs.Scan(&schema, &table, &cname, &col, &p); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			key := schema + "." + table
			if byTable[key] == nil {
				byTable[key] = map[string]map[int]string{}
			}
			if byTable[key][cname] == nil {
				byTable[key][cname] = map[int]string{}
			}
			byTable[key][cname][p] = col
			return nil
		})
	cons := map[string][]ConstraintDoc{}
	for key, byName := range byTable {
		if pos[key] == nil {
			pos[key] = map[string]int{}
		}
		for name, byPos := range byName {
			poss := make([]int, 0, len(byPos))
			for p := range byPos {
				poss = append(poss, p)
			}
			sort.Ints(poss)
			cols := make([]string, 0, len(poss))
			for i, p := range poss {
				cols = append(cols, byPos[p])
				pos[key][byPos[p]] = i + 1
			}
			cons[key] = append(cons[key], ConstraintDoc{Name: name, Kind: "pk", Columns: cols})
		}
	}
	return pos, cons
}

// collectForeignKeys — ALL_CONSTRAINTS(R) + ALL_CONS_COLUMNS. 참조 컬럼은
// R_CONSTRAINT_NAME의 컬럼을 POSITION으로 맞춘다 — JDBC getImportedKeys의
// KEY_SEQ 대응이다. 이름 없는 FK는 Oracle에서 불가능(제약은 항상 이름이 있다).
func (h *harvester) collectForeignKeys() map[string][]ConstraintDoc {
	type row struct {
		schema, table, name, col string
		pos                      int
		rSchema, rTable, rCol    sql.NullString
	}
	byName := map[string]map[int]row{}
	order := map[string][]string{}
	query, args := h.oracleScopedQuery(`SELECT c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.COLUMN_NAME, cc.POSITION,
		        rc.OWNER, rc.TABLE_NAME, rcc.COLUMN_NAME
		 FROM ALL_CONSTRAINTS c
		 JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER
		   AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND cc.TABLE_NAME = c.TABLE_NAME
		 JOIN ALL_CONSTRAINTS rc ON rc.OWNER = c.R_OWNER
		   AND rc.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME
		 LEFT JOIN ALL_CONS_COLUMNS rcc ON rcc.OWNER = rc.OWNER
		   AND rcc.CONSTRAINT_NAME = rc.CONSTRAINT_NAME
		   AND rcc.TABLE_NAME = rc.TABLE_NAME AND rcc.POSITION = cc.POSITION
		 WHERE c.CONSTRAINT_TYPE = 'R'
		 ORDER BY c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION`, "c.OWNER = :schema")
	h.bestEffortArgs("foreign keys", query, args,
		func(rs *sql.Rows) error {
			var r row
			if err := rs.Scan(&r.schema, &r.table, &r.name, &r.col, &r.pos,
				&r.rSchema, &r.rTable, &r.rCol); err != nil {
				return err
			}
			if !h.keepSchema(r.schema) {
				return nil
			}
			key := r.schema + "." + r.table
			if byName[key] == nil {
				byName[key] = map[int]row{}
			}
			// 제약 이름은 테이블 안에서 유일 — key에 제약 이름을 섞는다.
			fkey := key + "|" + r.name
			if byName[fkey] == nil {
				byName[fkey] = map[int]row{}
				order[key] = append(order[key], fkey)
			}
			byName[fkey][r.pos] = r
			return nil
		})
	out := map[string][]ConstraintDoc{}
	for key, fkeys := range order {
		for _, fkey := range fkeys {
			rows := byName[fkey]
			poss := make([]int, 0, len(rows))
			for p := range rows {
				poss = append(poss, p)
			}
			sort.Ints(poss)
			var cols, rcols []string
			var rSchema, rTable string
			for _, p := range poss {
				r := rows[p]
				cols = append(cols, r.col)
				rSchema = r.rSchema.String
				rTable = r.rTable.String
				rcols = append(rcols, r.rCol.String)
			}
			name := fkey[strings.Index(fkey, "|")+1:]
			var schemaPtr *string
			if rSchema != "" {
				schemaPtr = &rSchema
			}
			out[key] = append(out[key], ConstraintDoc{
				Name: name, Kind: "fk", Columns: cols,
				Referenced: &ReferencedDoc{Schema: schemaPtr, Table: rTable, Columns: rcols},
			})
		}
	}
	return out
}

func (h *harvester) collectIndexes() map[string][]IndexDoc {
	type idx struct {
		unique bool
		cols   map[int]string
	}
	byTable := map[string]map[string]*idx{}
	query, args := h.oracleScopedQuery(`SELECT i.OWNER, i.TABLE_NAME, i.INDEX_NAME, i.UNIQUENESS,
		        ic.COLUMN_NAME, ic.COLUMN_POSITION
		 FROM ALL_INDEXES i
		 JOIN ALL_IND_COLUMNS ic ON ic.INDEX_OWNER = i.OWNER
		   AND ic.INDEX_NAME = i.INDEX_NAME
		 ORDER BY i.OWNER, i.TABLE_NAME, i.INDEX_NAME, ic.COLUMN_POSITION`, "i.OWNER = :schema")
	h.bestEffortArgs("indexes", query, args,
		func(rs *sql.Rows) error {
			var schema, table, name, uniq, col string
			var pos int
			if err := rs.Scan(&schema, &table, &name, &uniq, &col, &pos); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			key := schema + "." + table
			if byTable[key] == nil {
				byTable[key] = map[string]*idx{}
			}
			g := byTable[key][name]
			if g == nil {
				g = &idx{unique: uniq == "UNIQUE", cols: map[int]string{}}
				byTable[key][name] = g
			}
			g.cols[pos] = col
			return nil
		})
	out := map[string][]IndexDoc{}
	for key, byName := range byTable {
		for name, g := range byName {
			poss := make([]int, 0, len(g.cols))
			for p := range g.cols {
				poss = append(poss, p)
			}
			sort.Ints(poss)
			cols := make([]string, 0, len(poss))
			for _, p := range poss {
				cols = append(cols, g.cols[p])
			}
			out[key] = append(out[key], IndexDoc{Name: name, Unique: g.unique, Columns: cols})
		}
		sort.Slice(out[key], func(i, j int) bool { return out[key][i].Name < out[key][j].Name })
	}
	return out
}

func (h *harvester) collectViews() map[string]string {
	out := map[string]string{}
	query, args := h.oracleScopedQuery("SELECT OWNER, VIEW_NAME, TEXT FROM ALL_VIEWS", "OWNER = :schema")
	h.bestEffortArgs("views", query, args,
		func(rs *sql.Rows) error {
			var schema, name string
			var text sql.NullString
			if err := rs.Scan(&schema, &name, &text); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			if text.Valid {
				out[schema+"."+name] = text.String
			}
			return nil
		})
	query, args = h.oracleScopedQuery("SELECT OWNER, MVIEW_NAME, QUERY FROM ALL_MVIEWS", "OWNER = :schema")
	h.bestEffortArgs("materialized views", query, args,
		func(rs *sql.Rows) error {
			var schema, name string
			var text sql.NullString
			if err := rs.Scan(&schema, &name, &text); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			if text.Valid {
				out[schema+"."+name] = text.String
			}
			return nil
		})
	return out
}

func (h *harvester) collectTriggers() map[string][]TriggerDoc {
	out := map[string][]TriggerDoc{}
	query, args := h.oracleScopedQuery("SELECT OWNER, TABLE_NAME, TRIGGER_NAME, TRIGGER_BODY FROM ALL_TRIGGERS", "OWNER = :schema")
	h.bestEffortArgs("triggers", query, args,
		func(rs *sql.Rows) error {
			var schema, table, name string
			var body sql.NullString
			if err := rs.Scan(&schema, &table, &name, &body); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			out[schema+"."+table] = append(out[schema+"."+table],
				TriggerDoc{Name: name, Body: ns(body)})
			return nil
		})
	for k := range out {
		sort.Slice(out[k], func(i, j int) bool { return out[k][i].Name < out[k][j].Name })
	}
	return out
}

// collectRoutines — ALL_OBJECTS가 kind를, ALL_SOURCE가 LINE순 몸체를 준다.
// PACKAGE는 스펙과 BODY를 따로 보관해 멤버 몸체를 멤버 헤더로 나눠 귀속한다 —
// Kotlin Extractor의 slicePackageBody와 같은 규칙이다(경계를 못 찾은 멤버는
// body 없이 limitation으로 센다).
func (h *harvester) collectRoutines() map[string][]RoutineDoc {
	type key struct{ schema, name, typ string }
	bodies := map[key]*strings.Builder{}
	query, args := h.oracleScopedQuery(`SELECT o.OWNER, o.OBJECT_NAME, o.OBJECT_TYPE, s.LINE, s.TEXT
		 FROM ALL_OBJECTS o
		 JOIN ALL_SOURCE s ON s.OWNER = o.OWNER
		   AND s.NAME = o.OBJECT_NAME AND s.TYPE = o.OBJECT_TYPE
		 WHERE o.OBJECT_TYPE IN ('PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY')
		 ORDER BY o.OWNER, o.OBJECT_NAME, s.LINE`, "o.OWNER = :schema")
	h.bestEffortArgs("routines", query, args,
		func(rs *sql.Rows) error {
			var schema, name, typ, text string
			var line int
			if err := rs.Scan(&schema, &name, &typ, &line, &text); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			k := key{schema, name, typ}
			if bodies[k] == nil {
				bodies[k] = &strings.Builder{}
			}
			bodies[k].WriteString(text)
			return nil
		})

	params := map[string]string{}
	query, args = h.oracleScopedQuery(`SELECT OWNER, OBJECT_NAME, DATA_TYPE FROM ALL_ARGUMENTS
		 WHERE POSITION > 0 AND PACKAGE_NAME IS NULL
		 ORDER BY OWNER, OBJECT_NAME, SEQUENCE`, "OWNER = :schema")
	h.bestEffortArgs("routine parameters", query, args,
		func(rs *sql.Rows) error {
			var schema, name, dtype string
			if err := rs.Scan(&schema, &name, &dtype); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			k := schema + "." + name
			if cur := params[k]; cur != "" {
				params[k] = cur + ", " + strings.ToLower(dtype)
			} else {
				params[k] = strings.ToLower(dtype)
			}
			return nil
		})

	// 패키지 멤버 — ALL_PROCEDURES가 SUBPROGRAM_ID(선언 순)로 멤버를 준다.
	// OVERLOAD는 같은 이름의 오버로드 구분자(NULL이면 비오버로드).
	type member struct {
		name     string
		overload string
	}
	pkgMembers := map[string][]member{}
	query, args = h.oracleScopedQuery(`SELECT OWNER, OBJECT_NAME, PROCEDURE_NAME, OVERLOAD
		 FROM ALL_PROCEDURES WHERE PROCEDURE_NAME IS NOT NULL
		 ORDER BY OWNER, OBJECT_NAME, SUBPROGRAM_ID`, "OWNER = :schema")
	h.bestEffortArgs("package members", query, args,
		func(rs *sql.Rows) error {
			var schema, pkg, mname string
			var ov sql.NullString
			if err := rs.Scan(&schema, &pkg, &mname, &ov); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			k := schema + "." + pkg
			pkgMembers[k] = append(pkgMembers[k], member{mname, ov.String})
			return nil
		})

	// 멤버 시그니처와 kind — PACKAGE_NAME이 있는 인자 행. POSITION=0은
	// 반환값이라 함수의 표시다. 키는 "owner.pkg.member[#overload]".
	memberParams := map[string]string{}
	memberFunctions := map[string]bool{}
	query, args = h.oracleScopedQuery(`SELECT OWNER, PACKAGE_NAME, OBJECT_NAME, DATA_TYPE, POSITION, OVERLOAD
		 FROM ALL_ARGUMENTS WHERE PACKAGE_NAME IS NOT NULL
		 ORDER BY OWNER, PACKAGE_NAME, OBJECT_NAME, OVERLOAD, SEQUENCE`, "OWNER = :schema")
	h.bestEffortArgs("package member arguments", query, args,
		func(rs *sql.Rows) error {
			var schema, pkg, mname, dtype string
			var pos int
			var ov sql.NullString
			if err := rs.Scan(&schema, &pkg, &mname, &dtype, &pos, &ov); err != nil {
				return err
			}
			if !h.keepSchema(schema) {
				return nil
			}
			k := schema + "." + pkg + "." + mname
			if ov.Valid {
				k += "#" + ov.String
			}
			if pos == 0 {
				memberFunctions[k] = true
			} else if cur := memberParams[k]; cur != "" {
				memberParams[k] = cur + ", " + strings.ToLower(dtype)
			} else {
				memberParams[k] = strings.ToLower(dtype)
			}
			return nil
		})

	specBodies := map[string]string{}
	implBodies := map[string]string{}
	out := map[string][]RoutineDoc{}
	plsql := "plsql"
	for k, b := range bodies {
		p := k.schema + "." + k.name
		switch k.typ {
		case "PACKAGE":
			specBodies[p] = b.String()
		case "PACKAGE BODY":
			implBodies[p] = b.String()
		default:
			if !h.keepSchema(k.schema) {
				continue
			}
			body := b.String()
			var sig *string
			if s := params[p]; s != "" {
				sig = &s
			}
			kind := "procedure"
			if k.typ == "FUNCTION" {
				kind = "function"
			}
			out[k.schema] = append(out[k.schema], RoutineDoc{
				Name: k.name, Kind: kind, Language: &plsql, Body: &body, Signature: sig,
			})
		}
	}
	pkgSet := map[string]bool{}
	for p := range specBodies {
		pkgSet[p] = true
	}
	for p := range implBodies {
		pkgSet[p] = true
	}
	pkgs := make([]string, 0, len(pkgSet))
	for p := range pkgSet {
		pkgs = append(pkgs, p)
	}
	sort.Strings(pkgs)
	for _, p := range pkgs {
		schema := strings.SplitN(p, ".", 2)[0]
		if !h.keepSchema(schema) {
			continue
		}
		spec, hasSpec := specBodies[p]
		impl, hasImpl := implBodies[p]
		var memberDocs []RoutineDoc
		if hasImpl && len(pkgMembers[p]) > 0 {
			names := map[string]bool{}
			for _, m := range pkgMembers[p] {
				names[m.name] = true
			}
			slices := slicePackageBodyLexical(impl, names)
			seen := map[string]int{}
			for _, m := range pkgMembers[p] {
				// 같은 이름의 n번째 오버로드는 본문의 n번째 헤더와 짝짓는다.
				nth := seen[m.name]
				seen[m.name]++
				ov := ""
				if m.overload != "" {
					ov = "#" + m.overload
				}
				mk := p + "." + m.name + ov
				kind := "procedure"
				if memberFunctions[mk] {
					kind = "function"
				}
				slice, ok := slices[m.name+"#"+strconv.Itoa(nth)]
				var body *string
				if ok {
					body = &slice
				} else {
					h.limitations = append(h.limitations,
						mk+": 패키지 본문에서 멤버 경계를 못 찾음 — 해당 멤버의 몸체 간선 없음")
				}
				var sig *string
				if s := memberParams[mk]; s != "" {
					sig = &s
				}
				pname := strings.SplitN(p, ".", 2)[1]
				memberDocs = append(memberDocs, RoutineDoc{
					Name: m.name, Kind: kind, Language: &plsql,
					Body: body, Signature: sig, MemberOf: &pname,
				})
			}
		}
		// 멤버를 냈으면 패키지 몸체는 스펙이 대표다 — 실행 문장은 멤버
		// 몸체에 있다. 못 냈으면 옛 동작(BODY 통째 귀속)을 유지한다.
		var body string
		if len(memberDocs) > 0 {
			if hasSpec {
				body = spec
			} else {
				body = impl
			}
		} else if hasImpl {
			body = impl
		} else {
			body = spec
		}
		pname := strings.SplitN(p, ".", 2)[1]
		out[schema] = append(out[schema], RoutineDoc{
			Name: pname, Kind: "package", Language: &plsql, Body: &body,
		})
		out[schema] = append(out[schema], memberDocs...)
	}
	return out
}
