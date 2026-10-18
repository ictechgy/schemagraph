package main

import (
	"database/sql"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	_ "modernc.org/sqlite"
)

// createSQLiteFixture — 별칭 검사의 원본 DB를 만들고 그 바이트를 돌려준다.
func createSQLiteFixture(t *testing.T, path string) []byte {
	t.Helper()
	db, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`CREATE TABLE accounts (id INTEGER PRIMARY KEY)`); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	return readOutput(t, path)
}

func readOutput(t *testing.T, path string) []byte {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return contents
}

// sqliteMainFile — SQLite가 spec.dsn으로 실제로 연 main DB 파일이다.
func sqliteMainFile(t *testing.T, spec connectionSpec) string {
	t.Helper()
	db, err := sql.Open(spec.driver, spec.dsn)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	var sequence int
	var name, file string
	if err := db.QueryRow("PRAGMA database_list").Scan(&sequence, &name, &file); err != nil {
		t.Fatal(err)
	}
	return file
}

// commitOutput — main의 JSON 경로처럼 연결 전에 출력을 정하고 문서를 쓴 뒤 교체한다.
func commitOutput(t *testing.T, path, contents string) {
	t.Helper()
	target, err := prepareOutput(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := writeDocument(target, []byte(contents)); err != nil {
		t.Fatal(err)
	}
}

func assertEntries(t *testing.T, directory string, want ...string) {
	t.Helper()
	entries, err := os.ReadDir(directory)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, entry := range entries {
		got = append(got, entry.Name())
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("directory entries = %q, want %q (a temporary file was left behind)", got, want)
	}
}

// 원본 SQLite를 -o로 주면 JSON은 수집 뒤 DB를 문서로 덮었고, NDJSON은 수집 전에 DB를 비운 뒤
// 빈 DB를 catalog_complete=true인 빈 카탈로그로 냈다. 경로 표기·링크·-wal/-shm 모두 연결 전에
// 거절해야 한다. SQLite가 실제로 연 파일과 spec.file이 같은지도 함께 본다.
func TestOutputNamingTheSQLiteSourceIsRejected(t *testing.T) {
	directory := t.TempDir()
	t.Chdir(directory)
	database := filepath.Join(directory, "app.db")
	original := createSQLiteFixture(t, database)
	createSQLiteFixture(t, filepath.Join(directory, "100%.db"))
	if err := os.Symlink("app.db", "link.db"); err != nil {
		t.Fatal(err)
	}
	if err := os.Link("app.db", "hard.db"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir("sub", 0o755); err != nil {
		t.Fatal(err)
	}
	cases := []struct{ url, output string }{
		{"sqlite:app.db", "app.db"},
		{"sqlite:app.db", "sub/../app.db"},
		{"sqlite:app.db", database},
		{"sqlite:" + database, "link.db"},
		{"sqlite://" + database, "hard.db"},
		{"file:" + database + "?cache=shared", "app.db"},
		{"sqlite:file://localhost" + database, "app.db"},
		{"sqlite:%61pp.db", "app.db"},
		{"sqlite:100%.db", "100%.db"},
		{"sqlite:link.db", "app.db-wal"},
		{"sqlite:app.db", "app.db-shm"},
		{"sqlite:link.db", "link.db-wal"},
		// 작업 디렉터리가 symlink 아래여도(macOS 임시 디렉터리) 없는 -wal을 같은 이름으로 본다.
		{"sqlite:" + database, "app.db-wal"},
		{"sqlite:app.db", database + "-shm"},
	}
	if _, err := os.Stat("APP.DB"); err == nil {
		// 대소문자를 무시하는 파일 시스템(macOS 기본값)에서는 다른 표기도 같은 파일이다.
		cases = append(cases, struct{ url, output string }{"sqlite:app.db", "APP.DB"})
	}
	for _, test := range cases {
		spec, err := parseConnection(test.url)
		if err != nil {
			t.Fatalf("%s: %v", test.url, err)
		}
		if opened := sqliteMainFile(t, spec); !sameFileForTest(t, opened, spec.file) {
			t.Fatalf("%s: SQLite opened %q but the probe compares %q", test.url, opened, spec.file)
		}
		if _, err := prepareFileOutput(spec, test.output); !errors.Is(err, sqliteSourceAliasError) {
			t.Errorf("-o %s with --url %s was accepted: %v", test.output, test.url, err)
		}
	}
	spec, err := parseConnection("sqlite:app.db")
	if err != nil {
		t.Fatal(err)
	}
	for _, output := range []string{"catalog.json", "app.db.json", "sub/app.db"} {
		if _, err := prepareFileOutput(spec, output); err != nil {
			t.Errorf("-o %s is not the SQLite source but was rejected: %v", output, err)
		}
	}
	if !reflect.DeepEqual(readOutput(t, database), original) {
		t.Fatal("the SQLite source changed")
	}
}

func sameFileForTest(t *testing.T, left, right string) bool {
	t.Helper()
	same, err := sameExistingFile(left, right)
	if err != nil {
		t.Fatal(err)
	}
	return same
}

// failingWriter — 정해진 바이트 뒤에 쓰기를 실패시켜 끊긴 수집(연결 끊김·디스크 가득 참)을 흉내 낸다.
type failingWriter struct {
	output    io.Writer
	remaining int
}

func (w *failingWriter) Write(data []byte) (int, error) {
	if len(data) > w.remaining {
		written, err := w.output.Write(data[:w.remaining])
		w.remaining = 0
		if err != nil {
			return written, err
		}
		return written, errStreamWriter
	}
	w.remaining -= len(data)
	return w.output.Write(data)
}

// NDJSON은 수집 전에 -o를 잘라 열어, 트레일러 전에 끊긴 수집이 이전 스냅샷을 잃게 했다. main처럼
// 스트림이 실패하면 버리고, 이전 파일과 디렉터리(임시 파일 없음)가 그대로여야 한다.
func TestInterruptedNDJSONStreamKeepsThePreviousFile(t *testing.T) {
	db, err := sql.Open("sqlite", "file:interrupted-output?mode=memory&cache=shared")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`CREATE TABLE interrupted_items (id INTEGER PRIMARY KEY, name TEXT)`); err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	path := filepath.Join(directory, "catalog.ndjson")
	if err := os.WriteFile(path, []byte("previous snapshot\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	target, err := prepareOutput(path)
	if err != nil {
		t.Fatal(err)
	}
	output, err := openOutput(target)
	if err != nil {
		t.Fatal(err)
	}
	h := &harvester{db: db, dialect: "sqlite", schemaFilter: map[string]bool{}}
	if err := h.streamNDJSON(&failingWriter{output: output, remaining: 64}, 1); !errors.Is(err, errStreamWriter) {
		t.Fatalf("stream error = %v", err)
	}
	if err := output.discard(); err != nil {
		t.Fatal(err)
	}
	if got := string(readOutput(t, path)); got != "previous snapshot\n" {
		t.Fatalf("previous snapshot was replaced by %q", got)
	}
	assertEntries(t, directory, "catalog.ndjson")
}

// 교체는 기존 권한을 지키고, 새 파일은 지금처럼 0644&^umask로 만든다(0600 전환은 계약 결정이라
// 하지 않는다).
func TestCommittedOutputReplacesTheFileAndKeepsPermissions(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "catalog.json")
	if err := os.WriteFile(path, []byte("previous\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o640); err != nil {
		t.Fatal(err)
	}
	commitOutput(t, path, "next\n")
	if got := string(readOutput(t, path)); got != "next\n" {
		t.Fatalf("contents = %q", got)
	}
	if mode := fileMode(t, path); mode != 0o640 {
		t.Fatalf("existing permissions changed to %v", mode)
	}
	fresh := filepath.Join(directory, "fresh.json")
	commitOutput(t, fresh, "next\n")
	control := filepath.Join(directory, "control.json")
	file, err := os.OpenFile(control, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	if fileMode(t, fresh) != fileMode(t, control) {
		t.Fatalf("new file mode %v differs from the previous default %v", fileMode(t, fresh), fileMode(t, control))
	}
	assertEntries(t, directory, "catalog.json", "control.json", "fresh.json")
}

func fileMode(t *testing.T, path string) fs.FileMode {
	t.Helper()
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode()
}

// symlink 출력은 지금처럼 가리키는 파일을 갱신하고 링크는 남긴다. 대상이 없는 링크는 지금처럼
// 링크를 따라 대상 파일을 만든다(symlink 거절은 계약 결정이라 하지 않는다).
func TestSymlinkOutputUpdatesTheLinkedFile(t *testing.T) {
	directory := t.TempDir()
	real := filepath.Join(directory, "real.json")
	if err := os.WriteFile(real, []byte("previous\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(directory, "link.json")
	if err := os.Symlink("real.json", link); err != nil {
		t.Fatal(err)
	}
	commitOutput(t, link, "next\n")
	if fileMode(t, link)&fs.ModeSymlink == 0 {
		t.Fatal("the symlink was replaced by a regular file")
	}
	if got := string(readOutput(t, real)); got != "next\n" {
		t.Fatalf("linked file contents = %q", got)
	}
	dangling := filepath.Join(directory, "dangling.json")
	if err := os.Symlink("created.json", dangling); err != nil {
		t.Fatal(err)
	}
	commitOutput(t, dangling, "next\n")
	if got := string(readOutput(t, filepath.Join(directory, "created.json"))); got != "next\n" || fileMode(t, dangling)&fs.ModeSymlink == 0 {
		t.Fatalf("dangling symlink target = %q", got)
	}
	assertEntries(t, directory, "created.json", "dangling.json", "link.json", "real.json")
}

// 장치·디렉터리는 rename으로 바꿀 대상이 아니다. /dev/null은 지금처럼 제자리에 쓰고, 디렉터리는
// 지금처럼 쓰기 오류가 난다.
func TestNonRegularOutputsAreWrittenInPlace(t *testing.T) {
	if _, err := os.Stat("/dev/null"); err != nil {
		t.Skip("no /dev/null on this platform")
	}
	commitOutput(t, "/dev/null", "next\n")
	directory := t.TempDir()
	target, err := prepareOutput(directory)
	if err != nil || !target.inPlace {
		t.Fatalf("directory target = %+v, %v", target, err)
	}
	if err := writeDocument(target, []byte("next\n")); err == nil {
		t.Fatal("writing a document over a directory succeeded")
	}
	assertEntries(t, directory)
}

// 제자리 쓰기는 읽기 전용 파일에서 실패했다. rename은 파일 권한 없이도 바꿀 수 있으므로 같은
// 검사를 먼저 해서 보호된 스냅샷을 바꾸지 않는다.
func TestReadOnlyOutputIsNotReplaced(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root can open read-only files for writing")
	}
	path := filepath.Join(t.TempDir(), "catalog.json")
	if err := os.WriteFile(path, []byte("protected\n"), 0o444); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareOutput(path); !errors.Is(err, fs.ErrPermission) {
		t.Fatalf("read-only output was accepted: %v", err)
	}
	if got := string(readOutput(t, path)); got != "protected\n" {
		t.Fatalf("contents = %q", got)
	}
}
