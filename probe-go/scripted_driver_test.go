package main

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"fmt"
	"io"
	"sync"
)

// scriptedServer는 서버 없이 방언 경로의 질의 모양·바인드 인자·실패 처리를 검사하는
// database/sql 가짜 서버다. Kotlin 테스트의 ScriptedConnection처럼 질의는 기록만 하고
// 응답은 테스트가 정한다. 연결 번호도 남겨 어느 물리 세션에서 실행됐는지 확인한다.
type scriptedServer struct {
	mu     sync.Mutex
	reply  func(query string, args []driver.NamedValue) scriptedReply
	calls  []scriptedCall
	opened int
}

// scriptedReply는 질의 하나의 응답이다. err는 질의 자체의 실패이고, rowsErr는 행을
// 다 준 뒤 커서가 내는 실패(rows.Err)다.
type scriptedReply struct {
	columns []string
	rows    [][]driver.Value
	err     error
	rowsErr error
}

// scriptedCall은 서버에 도달한 질의 하나다.
type scriptedCall struct {
	conn  int
	query string
	args  []driver.NamedValue
}

// newScriptedDB는 reply가 응답을 정하는 풀을 연다. reply가 nil이면 모든 질의가 빈 결과다.
// 전역 sql.Register 없이 connector로 열어 테스트마다 독립된 서버를 쓴다.
func newScriptedDB(reply func(string, []driver.NamedValue) scriptedReply) (*sql.DB, *scriptedServer) {
	server := &scriptedServer{reply: reply}
	return sql.OpenDB(scriptedConnector{server: server}), server
}

// scriptedRows는 행 값만으로 응답을 만든다. database/sql은 열 이름이 아니라 열 수만 본다.
func scriptedRows(rows ...[]driver.Value) scriptedReply {
	reply := scriptedReply{rows: rows}
	if len(rows) > 0 {
		for i := range rows[0] {
			reply.columns = append(reply.columns, fmt.Sprintf("c%d", i+1))
		}
	}
	return reply
}

// recorded는 지금까지 서버에 도달한 질의의 사본이다.
func (s *scriptedServer) recorded() []scriptedCall {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]scriptedCall(nil), s.calls...)
}

func (s *scriptedServer) run(conn int, query string, args []driver.NamedValue) scriptedReply {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.calls = append(s.calls, scriptedCall{conn: conn, query: query, args: append([]driver.NamedValue(nil), args...)})
	if s.reply == nil {
		return scriptedReply{}
	}
	return s.reply(query, args)
}

// namedArg는 바인드된 이름 인자의 값을 찾는다. Oracle·SQL Server 경로는 sql.Named로 바인딩한다.
func namedArg(args []driver.NamedValue, name string) (any, bool) {
	for _, arg := range args {
		if arg.Name == name {
			return arg.Value, true
		}
	}
	return nil, false
}

type scriptedConnector struct{ server *scriptedServer }

func (c scriptedConnector) Connect(context.Context) (driver.Conn, error) {
	c.server.mu.Lock()
	defer c.server.mu.Unlock()
	c.server.opened++
	return &scriptedConn{server: c.server, id: c.server.opened}, nil
}

func (scriptedConnector) Driver() driver.Driver { return scriptedDriver{} }

// scriptedDriver는 Connector 계약을 채우기 위한 것이다. 연결은 항상 connector가 연다.
type scriptedDriver struct{}

func (scriptedDriver) Open(string) (driver.Conn, error) {
	return nil, errors.New("scripted driver opens connections only through its connector")
}

type scriptedConn struct {
	server *scriptedServer
	id     int
}

func (c *scriptedConn) Prepare(string) (driver.Stmt, error) {
	return nil, errors.New("scripted connection runs queries directly; prepared statements are unsupported")
}

func (c *scriptedConn) Close() error { return nil }

func (c *scriptedConn) Begin() (driver.Tx, error) {
	return nil, errors.New("scripted connection does not support transactions")
}

func (c *scriptedConn) QueryContext(_ context.Context, query string, args []driver.NamedValue) (driver.Rows, error) {
	reply := c.server.run(c.id, query, args)
	if reply.err != nil {
		return nil, reply.err
	}
	return &scriptedCursor{reply: reply}, nil
}

func (c *scriptedConn) ExecContext(_ context.Context, query string, args []driver.NamedValue) (driver.Result, error) {
	if reply := c.server.run(c.id, query, args); reply.err != nil {
		return nil, reply.err
	}
	return driver.RowsAffected(0), nil
}

// scriptedCursor는 응답 행을 차례로 넘기고, 끝에서 rowsErr가 있으면 그것을, 없으면 io.EOF를 낸다.
type scriptedCursor struct {
	reply scriptedReply
	next  int
}

func (r *scriptedCursor) Columns() []string { return r.reply.columns }

func (r *scriptedCursor) Close() error { return nil }

func (r *scriptedCursor) Next(dest []driver.Value) error {
	if r.next < len(r.reply.rows) {
		copy(dest, r.reply.rows[r.next])
		r.next++
		return nil
	}
	if r.reply.rowsErr != nil {
		return r.reply.rowsErr
	}
	return io.EOF
}
