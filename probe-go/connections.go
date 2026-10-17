package main

import (
	"context"
	"database/sql"
	"fmt"
	"net"
	"net/url"
	"strings"

	mysqlDriver "github.com/go-sql-driver/mysql"
	_ "github.com/jackc/pgx/v5/stdlib"
	_ "modernc.org/sqlite"
)

type connectionSpec struct {
	driver, dialect, dsn, schema string
}

// URL을 직접 받거나 환경 변수에서 한 번만 읽어 접속 입력을 확정한다.
func resolveConnectionURL(rawURL, envName string, lookup func(string) (string, bool)) (string, error) {
	if rawURL != "" && envName != "" {
		return "", fmt.Errorf("set only one of --url or --url-env")
	}
	if rawURL != "" {
		return rawURL, nil
	}
	if envName == "" {
		return "", fmt.Errorf("set one of --url or --url-env")
	}
	value, ok := lookup(envName)
	if !ok {
		return "", fmt.Errorf("environment variable for --url-env is not set")
	}
	if value == "" {
		return "", fmt.Errorf("environment variable for --url-env is empty")
	}
	return value, nil
}

// 접속 문자열을 드라이버 형식으로만 바꾼다. 오류에는 비밀번호가 든 원문을 싣지 않는다.
func parseConnection(raw string) (connectionSpec, error) {
	if strings.HasPrefix(raw, "sqlite:") || strings.HasPrefix(raw, "file:") {
		return sqliteConnection(raw)
	}
	u, err := url.Parse(raw)
	if err != nil {
		return connectionSpec{}, fmt.Errorf("invalid database URL; check percent encoding, host, and port")
	}
	switch strings.ToLower(u.Scheme) {
	case "oracle":
		return oracleConnection(raw, u), nil
	case "postgres", "postgresql":
		return connectionSpec{driver: "pgx", dialect: "postgres", dsn: raw}, nil
	case "mysql", "mariadb":
		return mysqlConnection(u)
	case "sqlserver", "mssql":
		u.Scheme = "sqlserver"
		if u.Path != "" && u.Query().Get("database") == "" {
			query := u.Query()
			query.Set("database", strings.TrimPrefix(u.Path, "/"))
			u.Path, u.RawPath, u.RawQuery = "", "", query.Encode()
		}
		return connectionSpec{driver: "sqlserver", dialect: "sqlserver", dsn: u.String()}, nil
	default:
		return connectionSpec{}, fmt.Errorf("unsupported database URL; use sqlite:, postgres://, mysql://, oracle://, or sqlserver://")
	}
}

// oraclePrefetchRows는 사용자가 PREFETCH_ROWS를 정하지 않았을 때 왕복당 받을 행 수다.
// ALL_SOURCE는 소스 한 줄이 한 행이라, go-ora 기본값(25를 행 크기로 다시 계산해 원문
// 조회는 왕복당 약 31행)이면 대형 PL/SQL 스키마의 수집 시간을 RTT가 정한다.
// 행 최대 약 4KB × 1000행이라 버퍼 상한은 작다. JDBC 프로브의 ORACLE_FETCH_ROWS와 같다.
const oraclePrefetchRows = "1000"

// Oracle URL은 원문 바이트(비밀번호 인코딩 포함)를 그대로 두고 PREFETCH_ROWS가 없을 때만
// 질의 끝에 덧붙인다. go-ora는 질의 키를 대문자로 바꿔 비교하므로 키는 대소문자 없이 찾는다.
func oracleConnection(raw string, u *url.URL) connectionSpec {
	spec := connectionSpec{driver: "oracle", dialect: "oracle", dsn: raw}
	for key := range u.Query() {
		if strings.EqualFold(key, "PREFETCH_ROWS") {
			return spec
		}
	}
	// url.Parse처럼 첫 '#' 앞이 질의를 담는 부분이다.
	base, fragment, hasFragment := strings.Cut(raw, "#")
	separator := "&"
	switch {
	case !strings.Contains(base, "?"):
		separator = "?"
	case strings.HasSuffix(base, "?") || strings.HasSuffix(base, "&"):
		separator = ""
	}
	spec.dsn = base + separator + "PREFETCH_ROWS=" + oraclePrefetchRows
	if hasFragment {
		spec.dsn += "#" + fragment
	}
	return spec
}

func mysqlConnection(u *url.URL) (connectionSpec, error) {
	if u.Hostname() == "" {
		return connectionSpec{}, fmt.Errorf("MySQL URL requires a host")
	}
	config := mysqlDriver.NewConfig()
	config.Net = "tcp"
	port := u.Port()
	if port == "" {
		port = "3306"
	}
	config.Addr = net.JoinHostPort(u.Hostname(), port)
	config.DBName = strings.TrimPrefix(u.Path, "/")
	if u.User != nil {
		config.User = u.User.Username()
		config.Passwd, _ = u.User.Password()
	}
	config.Params = map[string]string{}
	for key, values := range u.Query() {
		if len(values) != 1 {
			return connectionSpec{}, fmt.Errorf("duplicate MySQL URL option; specify each option once")
		}
		config.Params[key] = values[0]
	}
	return connectionSpec{driver: "mysql", dialect: "mysql", dsn: config.FormatDSN(), schema: config.DBName}, nil
}

// SQLite는 mode=ro로 고정해 오타 난 경로에 새 빈 DB를 만들지 않는다.
func sqliteConnection(raw string) (connectionSpec, error) {
	value := strings.TrimPrefix(raw, "sqlite:")
	if strings.HasPrefix(value, "//") {
		value = value[2:]
	}
	if !strings.HasPrefix(value, "file:") {
		value = "file:" + value
	}
	u, err := url.Parse(value)
	if err != nil || u.Path == "" && u.Opaque == "" || strings.Contains(value, ":memory:") {
		return connectionSpec{}, fmt.Errorf("SQLite probe requires an existing file URL; check its path and encoding")
	}
	query := u.Query()
	query.Set("mode", "ro")
	u.RawQuery = query.Encode()
	return connectionSpec{driver: "sqlite", dialect: "sqlite", dsn: u.String()}, nil
}

// catalogSession — 수확기가 쓰는 조회 연산이다. 운영 경로는 pinSession이 만든 고정 연결이고,
// 단위 테스트는 단일 연결 풀(*sql.DB)을 그대로 넘긴다.
type catalogSession interface {
	Query(query string, args ...any) (*sql.Rows, error)
	QueryRow(query string, args ...any) *sql.Row
	Exec(query string, args ...any) (sql.Result, error)
}

// pinnedSession — 모든 카탈로그 조회를 물리 연결 하나로 보낸다. *sql.Conn에는 context 없는
// 조회 메서드가 없어 수확기 호출 모양에 맞춰 감싼다.
type pinnedSession struct{ conn *sql.Conn }

// pinSession — 수집 전체를 연결 하나에 고정한다. database/sql 풀은 끊긴 연결을 새 물리 연결로
// 몰래 바꾸는데(ErrBadConn 재시도), 새 연결에는 configureReadOnly의 search_path·읽기 전용 설정이
// 없다. 고정 연결은 끊기면 교체되지 않아 이후 조회가 오류가 되고, bestEffort가 그 사실을
// limitation과 미완전으로 남긴다. JDBC 프로브가 Connection 하나로 수집하는 것과 같다.
func pinSession(db *sql.DB) (*pinnedSession, error) {
	conn, err := db.Conn(context.Background())
	if err != nil {
		return nil, err
	}
	return &pinnedSession{conn: conn}, nil
}

func (s *pinnedSession) Query(query string, args ...any) (*sql.Rows, error) {
	return s.conn.QueryContext(context.Background(), query, args...)
}

func (s *pinnedSession) QueryRow(query string, args ...any) *sql.Row {
	return s.conn.QueryRowContext(context.Background(), query, args...)
}

func (s *pinnedSession) Exec(query string, args ...any) (sql.Result, error) {
	return s.conn.ExecContext(context.Background(), query, args...)
}

// Close는 고정 연결을 풀에 돌려 db.Close가 서버 세션을 정상 종료하게 한다.
func (s *pinnedSession) Close() error {
	return s.conn.Close()
}

// 고정 세션(pinSession)에 읽기 전용 설정을 건다. 같은 물리 연결이 이후 모든 카탈로그 조회를 실행한다.
func (h *harvester) configureReadOnly() {
	if h.dialect == "postgres" {
		// 서버가 반환하는 뷰 정의를 스키마 한정 이름으로 고정한다.
		if _, err := h.db.Exec("SET search_path = pg_catalog"); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, "PostgreSQL definition namespace could not be pinned; object resolution may be incomplete")
		}
	}
	var statements []string
	switch h.dialect {
	case "postgres":
		statements = []string{"SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"}
	case "mysql":
		statements = []string{"SET SESSION transaction_read_only=1", "SET SESSION tx_read_only=1"}
	default:
		return
	}
	for _, statement := range statements {
		if _, err := h.db.Exec(statement); err == nil {
			return
		}
	}
	h.limitations = append(h.limitations, "read-only session mode was unavailable; only catalog queries were issued")
}
