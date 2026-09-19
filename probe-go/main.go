// schemagraph-probe-go — JVM 없는 카탈로그 프로브. Oracle 전용.
//
// JVM 프로브(probe/)는 번들 드라이버로 대부분의 DB를 커버하지만 Oracle만은
// ojdbc가 OTN 라이선스라 jar을 직접 내려받아 --driver로 넘겨야 했다.
// go-ora는 pure Go Oracle 드라이버라 이 바이너리 하나면 JVM도 외부 jar도
// 없이 Oracle 카탈로그를 CatalogDocument v1로 뱉을 수 있다.
//
//	engine 쪽 소비는 JVM 프로브와 동일:
//	  schemagraph-probe-go --url oracle://u:p@host:1521/FREEPDB1 -o doc.json
//	  schemagraph scan --document doc.json -o graph.json
//
// 수확 의미론은 probe/Extractor.kt의 oracle 분기와 1:1 대응시킨다 —
// 둘이 다른 문서를 내면 같은 DB에서 그래프가 갈라진다.
package main

import (
	"database/sql"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"sort"
	"strings"

	_ "github.com/sijms/go-ora/v2"
)

const documentVersion = 1

// ---- document.rs / probe Model.kt와 1:1 대응하는 와이어 타입 ----

type CatalogDocument struct {
	Version     int         `json:"version"`
	Dialect     string      `json:"dialect"`
	Reader      string      `json:"reader"`
	Schemas     []SchemaDoc `json:"schemas"`
	Limitations []string    `json:"limitations"`
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
	Name    string    `json:"name"`
	Unique  bool      `json:"unique"`
	Columns []string  `json:"columns"`
	Usage   *UsageDoc `json:"usage,omitempty"`
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
}

// Oracle 카탈로그가 노출하는 시스템 스키마 — probe/Extractor.kt의 목록과 동일.
// PUBLIC은 시노님이 수천 개라 커서를 고갈시킨다 — 항상 억제한다.
// DVF/DVSYS(Database Vault)는 실검증에서 잡음으로 확인돼 추가됐다.
var systemSchemas = map[string]bool{
	"SYS": true, "SYSTEM": true, "OUTLN": true, "DBSNMP": true, "XDB": true,
	"WMSYS": true, "CTXSYS": true, "ORDSYS": true, "MDSYS": true, "OLAPSYS": true,
	"APPQOSSYS": true, "AUDSYS": true, "GSMADMIN_INTERNAL": true, "LBACSYS": true,
	"REMOTE_SCHEDULER_AGENT": true,
	"DIP":                    true, "ORACLE_OCM": true, "PUBLIC": true, "DVF": true, "DVSYS": true,
}

type harvester struct {
	db           *sql.DB
	schemaFilter map[string]bool
	limitations  []string
}

// bestEffort — 수확 실패를 limitation으로 변환한다. Kotlin Extractor의 규칙과
// 같다: 쿼리가 안 먹는 DB는 그 사실을 숨기지 않고 신고해야 한다.
func (h *harvester) bestEffort(label, query string, row func(rows *sql.Rows) error) {
	rows, err := h.db.Query(query)
	if err != nil {
		h.limitations = append(h.limitations,
			fmt.Sprintf("%s 원문 미수확(%s) — 간선이 빠질 수 있다", label, oneLine(err.Error())))
		return
	}
	defer rows.Close()
	for rows.Next() {
		if err := row(rows); err != nil {
			h.limitations = append(h.limitations,
				fmt.Sprintf("%s 행 읽기 실패(%s) — 간선이 빠질 수 있다", label, oneLine(err.Error())))
			return
		}
	}
	if err := rows.Err(); err != nil {
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
		url       = flag.String("url", "", "oracle://user:pass@host:port/service")
		schemaArg = flag.String("schema", "", "comma-separated schema allowlist (default: non-system all)")
		output    = flag.String("o", "catalog.json", "output path ('-' stdout)")
		format    = flag.String("format", "json", "json | ndjson")
	)
	flag.Parse()
	if *url == "" {
		fmt.Fprintln(os.Stderr, "error: --url이 필요하다 (oracle://user:pass@host:port/service)")
		os.Exit(2)
	}

	db, err := sql.Open("oracle", *url)
	if err != nil {
		fatal("드라이버 열기 실패", err)
	}
	defer db.Close()
	if err := db.Ping(); err != nil {
		fatal("접속 실패", err)
	}

	h := &harvester{db: db, schemaFilter: map[string]bool{}}
	if *schemaArg != "" {
		for _, s := range strings.Split(*schemaArg, ",") {
			if s = strings.TrimSpace(s); s != "" {
				h.schemaFilter[s] = true
			}
		}
	}

	doc := h.extract()
	doc.Limitations = sortedDistinct(doc.Limitations)

	var text string
	switch *format {
	case "json":
		b, err := json.MarshalIndent(doc, "", "  ")
		if err != nil {
			fatal("직렬화 실패", err)
		}
		text = string(b)
	case "ndjson":
		text = toNDJSON(doc)
	default:
		fatal("--format은 json|ndjson 중 하나다", fmt.Errorf("%s", *format))
	}
	if *output == "-" {
		fmt.Println(text)
	} else if err := os.WriteFile(*output, []byte(text+"\n"), 0o644); err != nil {
		fatal("쓰기 실패", err)
	}
}

func fatal(msg string, err error) {
	fmt.Fprintf(os.Stderr, "error: %s: %s\n", msg, oneLine(err.Error()))
	os.Exit(2)
}

func sortedDistinct(in []string) []string {
	seen := map[string]bool{}
	out := in[:0]
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
		Version: documentVersion, Dialect: "oracle", Reader: "probe-go",
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
	h.bestEffort("objects",
		`SELECT OWNER, TABLE_NAME, 'table' FROM ALL_TABLES WHERE TEMPORARY = 'N'
		 UNION ALL SELECT OWNER, VIEW_NAME, 'view' FROM ALL_VIEWS
		 UNION ALL SELECT OWNER, MVIEW_NAME, 'materialized-view' FROM ALL_MVIEWS
		 UNION ALL SELECT OWNER, SYNONYM_NAME, 'synonym' FROM ALL_SYNONYMS`,
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
	h.bestEffort("columns",
		`SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, COLUMN_ID
		 FROM ALL_TAB_COLUMNS ORDER BY OWNER, TABLE_NAME, COLUMN_ID`,
		func(rs *sql.Rows) error {
			var schema, table, name, dtype, nullable string
			var ord int
			if err := rs.Scan(&schema, &table, &name, &dtype, &nullable, &ord); err != nil {
				return err
			}
			key := schema + "." + table
			out[key] = append(out[key], ColumnDoc{
				Name: name, DataType: dtype,
				Nullable: nullable == "Y", Ordinal: ord,
			})
			return nil
		})
	defaults := map[string]*string{}
	h.bestEffort("column defaults",
		`SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_DEFAULT FROM ALL_TAB_COLUMNS`,
		func(rs *sql.Rows) error {
			var schema, table, name string
			var def sql.NullString
			if err := rs.Scan(&schema, &table, &name, &def); err != nil {
				return err
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
	h.bestEffort("primary keys",
		`SELECT c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.COLUMN_NAME, cc.POSITION
		 FROM ALL_CONSTRAINTS c
		 JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER
		   AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND cc.TABLE_NAME = c.TABLE_NAME
		 WHERE c.CONSTRAINT_TYPE = 'P'
		 ORDER BY c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION`,
		func(rs *sql.Rows) error {
			var schema, table, cname, col string
			var p int
			if err := rs.Scan(&schema, &table, &cname, &col, &p); err != nil {
				return err
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
	h.bestEffort("foreign keys",
		`SELECT c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.COLUMN_NAME, cc.POSITION,
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
		 ORDER BY c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION`,
		func(rs *sql.Rows) error {
			var r row
			if err := rs.Scan(&r.schema, &r.table, &r.name, &r.col, &r.pos,
				&r.rSchema, &r.rTable, &r.rCol); err != nil {
				return err
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
	h.bestEffort("indexes",
		`SELECT i.OWNER, i.TABLE_NAME, i.INDEX_NAME, i.UNIQUENESS,
		        ic.COLUMN_NAME, ic.COLUMN_POSITION
		 FROM ALL_INDEXES i
		 JOIN ALL_IND_COLUMNS ic ON ic.INDEX_OWNER = i.OWNER
		   AND ic.INDEX_NAME = i.INDEX_NAME
		 ORDER BY i.OWNER, i.TABLE_NAME, i.INDEX_NAME, ic.COLUMN_POSITION`,
		func(rs *sql.Rows) error {
			var schema, table, name, uniq, col string
			var pos int
			if err := rs.Scan(&schema, &table, &name, &uniq, &col, &pos); err != nil {
				return err
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
	h.bestEffort("views",
		"SELECT OWNER, VIEW_NAME, TEXT FROM ALL_VIEWS",
		func(rs *sql.Rows) error {
			var schema, name string
			var text sql.NullString
			if err := rs.Scan(&schema, &name, &text); err != nil {
				return err
			}
			if text.Valid {
				out[schema+"."+name] = text.String
			}
			return nil
		})
	h.bestEffort("materialized views",
		"SELECT OWNER, MVIEW_NAME, QUERY FROM ALL_MVIEWS",
		func(rs *sql.Rows) error {
			var schema, name string
			var text sql.NullString
			if err := rs.Scan(&schema, &name, &text); err != nil {
				return err
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
	h.bestEffort("triggers",
		"SELECT OWNER, TABLE_NAME, TRIGGER_NAME, TRIGGER_BODY FROM ALL_TRIGGERS",
		func(rs *sql.Rows) error {
			var schema, table, name string
			var body sql.NullString
			if err := rs.Scan(&schema, &table, &name, &body); err != nil {
				return err
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
// PACKAGE와 PACKAGE BODY는 하나의 정점으로 합치고 BODY 몸체를 우선한다 —
// Kotlin Extractor의 규칙과 같다(멤버별 귀속은 엔진이 limitation으로 남긴다).
func (h *harvester) collectRoutines() map[string][]RoutineDoc {
	type key struct{ schema, name, typ string }
	bodies := map[key]*strings.Builder{}
	h.bestEffort("routines",
		`SELECT o.OWNER, o.OBJECT_NAME, o.OBJECT_TYPE, s.LINE, s.TEXT
		 FROM ALL_OBJECTS o
		 JOIN ALL_SOURCE s ON s.OWNER = o.OWNER
		   AND s.NAME = o.OBJECT_NAME AND s.TYPE = o.OBJECT_TYPE
		 WHERE o.OBJECT_TYPE IN ('PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY')
		 ORDER BY o.OWNER, o.OBJECT_NAME, s.LINE`,
		func(rs *sql.Rows) error {
			var schema, name, typ, text string
			var line int
			if err := rs.Scan(&schema, &name, &typ, &line, &text); err != nil {
				return err
			}
			k := key{schema, name, typ}
			if bodies[k] == nil {
				bodies[k] = &strings.Builder{}
			}
			bodies[k].WriteString(text)
			return nil
		})

	params := map[string]string{}
	h.bestEffort("routine parameters",
		`SELECT OWNER, OBJECT_NAME, DATA_TYPE FROM ALL_ARGUMENTS
		 WHERE POSITION > 0 AND PACKAGE_NAME IS NULL
		 ORDER BY OWNER, OBJECT_NAME, SEQUENCE`,
		func(rs *sql.Rows) error {
			var schema, name, dtype string
			if err := rs.Scan(&schema, &name, &dtype); err != nil {
				return err
			}
			k := schema + "." + name
			if cur := params[k]; cur != "" {
				params[k] = cur + ", " + strings.ToLower(dtype)
			} else {
				params[k] = strings.ToLower(dtype)
			}
			return nil
		})

	type pair struct{ schema, name string }
	grouped := map[pair]key{}
	for k := range bodies {
		p := pair{k.schema, k.name}
		cur, ok := grouped[p]
		if !ok || (k.typ == "PACKAGE BODY" && cur.typ == "PACKAGE") {
			grouped[p] = k
		}
	}
	out := map[string][]RoutineDoc{}
	plsql := "plsql"
	for p, k := range grouped {
		if !h.keepSchema(p.schema) {
			continue
		}
		kind := "package"
		switch k.typ {
		case "PROCEDURE":
			kind = "procedure"
		case "FUNCTION":
			kind = "function"
		}
		body := bodies[k].String()
		var sig *string
		if s := params[p.schema+"."+p.name]; s != "" {
			sig = &s
		}
		out[p.schema] = append(out[p.schema], RoutineDoc{
			Name: p.name, Kind: kind, Language: &plsql, Body: &body, Signature: sig,
		})
	}
	return out
}

// toNDJSON — engine/source/src/ndjson.rs 및 probe/Model.kt의 toNdjson과
// 같은 레이아웃: document 헤더 → 스키마별 schema 행 + object·routine 행.
func toNDJSON(doc CatalogDocument) string {
	var b strings.Builder
	line := func(v any) {
		j, _ := json.Marshal(v)
		b.Write(j)
		b.WriteByte('\n')
	}
	line(map[string]any{
		"type": "document", "version": doc.Version, "dialect": doc.Dialect,
		"reader": doc.Reader, "limitations": doc.Limitations,
	})
	for _, s := range doc.Schemas {
		line(map[string]any{"type": "schema", "name": s.Name})
		for _, o := range s.Objects {
			line(map[string]any{"type": "object", "schema": s.Name, "data": o})
		}
		for _, r := range s.Routines {
			line(map[string]any{"type": "routine", "schema": s.Name, "data": r})
		}
	}
	return b.String()
}
