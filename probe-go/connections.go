package main

import (
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
		return connectionSpec{driver: "oracle", dialect: "oracle", dsn: raw}, nil
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

// 연결 하나로 고정해 읽기 전용 세션 설정이 모든 카탈로그 조회에 적용되게 한다.
func (h *harvester) configureReadOnly() {
	if h.dialect == "postgres" {
		h.db.SetMaxOpenConns(1)
		// 서버가 반환하는 뷰 정의를 스키마 한정 이름으로 고정한다.
		if _, err := h.db.Exec("SET search_path = pg_catalog"); err != nil {
			h.catalogIncomplete = true
			h.limitations = append(h.limitations, "PostgreSQL definition namespace could not be pinned; object resolution may be incomplete")
		}
	}
	var statements []string
	switch h.dialect {
	case "sqlite":
		h.db.SetMaxOpenConns(1)
		return
	case "postgres":
		statements = []string{"SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"}
	case "mysql":
		statements = []string{"SET SESSION transaction_read_only=1", "SET SESSION tx_read_only=1"}
	default:
		return
	}
	h.db.SetMaxOpenConns(1)
	for _, statement := range statements {
		if _, err := h.db.Exec(statement); err == nil {
			return
		}
	}
	h.limitations = append(h.limitations, "read-only session mode was unavailable; only catalog queries were issued")
}
