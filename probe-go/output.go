package main

// 프로브 출력 파일 보호. 엔진 #35의 산출물 계약(DESIGN.md "파일 산출물은 준비 후 교체한다")은
// 엔진 명령에 한정돼 있어, 프로브는 데이터 손실을 막는 두 가지만 따른다: SQLite 원본을 가리키는
// -o는 연결 전에 거절하고, 파일은 끝까지 쓴 뒤 교체한다. 새 파일 권한(0644&^umask)과 symlink를
// 따라 대상 파일을 갱신하는 동작은 바꾸지 않는다. probe/OutputFile.kt와 같은 규칙이다.

import (
	"errors"
	"fmt"
	"io/fs"
	"math/rand/v2"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"unicode/utf8"
)

// outputTarget — 연결 전에 정한 출력 방식이다. 이때는 아무것도 쓰지 않는다.
type outputTarget struct {
	// path는 쓸 경로다. 교체할 때는 symlink를 푼 실제 경로라 링크 자체는 그대로 남는다.
	path string
	// inPlace면 지금처럼 제자리에 연다. 장치·FIFO·디렉터리·/dev 아래 경로·대상이 없는 symlink는
	// rename으로 바꿀 대상이 아니다. root가 -o /dev/null을 주면 장치를 일반 파일로 바꾸게 된다.
	inPlace bool
	// existing이면 교체할 기존 파일의 권한 perm을 임시 파일에 옮긴다.
	existing bool
	perm     fs.FileMode
}

// sqliteSourceAliasError — 원본을 덮으면 JSON은 수집 뒤 DB를 문서로 바꾸고, NDJSON은 수집 전에
// DB를 비운 뒤 빈 DB를 catalog_complete=true인 빈 카탈로그로 냈다.
var sqliteSourceAliasError = errors.New("output must differ from the SQLite database and its -wal/-shm files; choose another -o path")

// prepareFileOutput — '-'가 아닌 -o를 연결 전에 해석하고 SQLite 원본 별칭을 거절한 뒤, 교체에 쓸
// 임시 파일을 만들 수 있는지 확인한다.
func prepareFileOutput(spec connectionSpec, output string) (outputTarget, error) {
	target, err := prepareOutput(output)
	if err != nil {
		return outputTarget{}, err
	}
	if err := checkOutputSource(spec, target); err != nil {
		return outputTarget{}, err
	}
	return target, checkStaging(target)
}

// checkStaging — 교체할 출력이면 연결 전에 같은 디렉터리에 임시 파일을 만들었다 지운다. 디렉터리
// 쓰기 권한이나 읽기 전용 파일 시스템 때문에 임시 파일을 만들 수 없으면 수집을 다 마친 뒤가 아니라
// 연결 전에 알린다. 제자리 쓰기 대상은 임시 파일을 쓰지 않는다.
func checkStaging(target outputTarget) error {
	if target.inPlace {
		return nil
	}
	file, err := createStagingFile(target.path)
	if err != nil {
		return fmt.Errorf("cannot create a temporary file next to the output (%w); "+
			"the output directory must be writable because the document is renamed into place", err)
	}
	return errors.Join(file.Close(), os.Remove(file.Name()))
}

// prepareOutput — 출력 경로의 쓰기 방식을 정한다. 기존 파일이 쓰기 불가면 제자리 쓰기가
// 실패하던 것처럼 오류로 돌려, rename이 보호된 파일을 몰래 바꾸지 않게 한다.
func prepareOutput(path string) (outputTarget, error) {
	if path == "" {
		return outputTarget{}, errors.New("output path is empty; pass a file path or '-' for stdout")
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return outputTarget{}, err
	}
	// 끝 구분자는 디렉터리를 뜻하고 /dev 아래는 장치와 디스크립터 별칭이다. macOS는 파일로
	// 리디렉션된 /dev/stdout(/dev/fd/1)을 일반 파일로 보고하지만 그 옆에 임시 파일을 만들 수 없다.
	if os.IsPathSeparator(path[len(path)-1]) || strings.HasPrefix(absolute, "/dev/") {
		return outputTarget{path: path, inPlace: true}, nil
	}
	info, err := os.Stat(path)
	if err == nil {
		return existingOutput(path, info)
	}
	if !errors.Is(err, fs.ErrNotExist) {
		return outputTarget{}, err
	}
	if _, err := os.Lstat(path); err == nil {
		// 대상이 없는 symlink다. 지금처럼 링크를 따라 대상 파일을 만든다.
		return outputTarget{path: path, inPlace: true}, nil
	}
	named, err := resolvedName(path)
	return outputTarget{path: named}, err
}

// existingOutput — 일반 파일만 교체한다. 쓰기로 열어 보고 바로 닫는 것은 옛 O_TRUNC 쓰기와 같은
// 권한 검사이며 내용을 건드리지 않는다.
func existingOutput(path string, info fs.FileInfo) (outputTarget, error) {
	if !info.Mode().IsRegular() {
		return outputTarget{path: path, inPlace: true}, nil
	}
	real, err := resolvedPath(path)
	if err != nil {
		return outputTarget{}, err
	}
	probe, err := os.OpenFile(real, os.O_WRONLY, 0)
	if err != nil {
		return outputTarget{}, err
	}
	if err := probe.Close(); err != nil {
		return outputTarget{}, err
	}
	return outputTarget{path: real, existing: true, perm: info.Mode().Perm()}, nil
}

// resolvedPath — 작업 디렉터리까지 포함해 symlink를 모두 푼 절대 경로다. 경로가 있어야 한다.
// 상대 경로를 먼저 풀고 filepath.Abs를 붙이면 작업 디렉터리의 symlink(macOS /var → /private/var)가
// 남아, 같은 파일이 원본 쪽과 출력 쪽에서 서로 다른 문자열이 된다.
func resolvedPath(path string) (string, error) {
	if !filepath.IsAbs(path) {
		// filepath.Abs는 ".."를 글자로 지워 symlink를 지난 ".."를 OS와 다르게 풀므로 그대로 잇는다.
		directory, err := os.Getwd()
		if err != nil {
			return "", err
		}
		path = directory + string(filepath.Separator) + path
	}
	return filepath.EvalSymlinks(path)
}

// resolvedName — 디렉터리만 실제 경로로 풀고 마지막 이름은 그대로 둔다. filepath.Dir은 ".."를
// 글자로 지우므로 마지막 이름만 떼어 OS처럼 symlink를 먼저 푼 뒤 ".."를 적용한다.
func resolvedName(path string) (string, error) {
	name := filepath.Base(path)
	directory := strings.TrimSuffix(path, name)
	if directory == "" {
		directory = "."
	}
	real, err := resolvedPath(directory)
	if err != nil {
		return "", err
	}
	return filepath.Join(real, name), nil
}

// checkOutputSource — 출력이 SQLite 원본이나 그 -wal·-shm이면 거절한다. 같은 경로 문자열과
// 같은 파일(하드 링크, 대소문자를 무시하는 파일 시스템의 다른 표기)을 모두 본다.
func checkOutputSource(spec connectionSpec, target outputTarget) error {
	if spec.file == "" {
		return nil
	}
	sources, err := sqliteSourceFiles(spec.file)
	if err != nil {
		return fmt.Errorf("cannot resolve the SQLite database path to compare with -o: %w", err)
	}
	for _, source := range sources {
		same, err := sameExistingFile(target.path, source)
		if err != nil {
			return err
		}
		if same || target.path == source {
			return sqliteSourceAliasError
		}
	}
	return nil
}

// sqliteSourceFiles — SQLite가 쓰는 원본 파일들이다. -wal·-shm은 SQLite 버전에 따라 링크 이름
// 옆이나 푼 이름 옆에 생기므로 둘 다 넣는다. 없는 원본은 연결이 실패하므로 풀 수 있는 이름만 쓴다.
func sqliteSourceFiles(file string) ([]string, error) {
	var bases []string
	for _, resolve := range []func(string) (string, error){resolvedName, resolvedPath} {
		base, err := resolve(file)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, err
		}
		bases = append(bases, base, base+"-wal", base+"-shm")
	}
	return bases, nil
}

// sameExistingFile — 두 경로가 모두 있고 같은 파일인지 본다. 없는 경로는 같은 파일이 아니다.
func sameExistingFile(left, right string) (bool, error) {
	leftInfo, err := os.Stat(left)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	rightInfo, err := os.Stat(right)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	return os.SameFile(leftInfo, rightInfo), nil
}

// stagedOutput — 문서 하나를 쓰는 동안의 파일이다. temporary가 있으면 commit이 그 임시 파일을
// target으로 rename하고, discard는 임시 파일을 지워 이전 파일을 그대로 둔다.
type stagedOutput struct {
	file      *os.File
	temporary string
	target    string
}

// openOutput — 교체할 출력은 같은 디렉터리에 임시 파일을 만든다. os.CreateTemp는 0600이라 새
// 파일 권한이 지금(0644&^umask)과 달라지므로 O_EXCL로 직접 만든다.
func openOutput(target outputTarget) (*stagedOutput, error) {
	if target.inPlace {
		file, err := os.OpenFile(target.path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
		if err != nil {
			return nil, err
		}
		return &stagedOutput{file: file, target: target.path}, nil
	}
	file, err := createStagingFile(target.path)
	if err != nil {
		return nil, err
	}
	output := &stagedOutput{file: file, temporary: file.Name(), target: target.path}
	if target.existing {
		if err := file.Chmod(target.perm); err != nil {
			return nil, errors.Join(err, output.discard())
		}
	}
	return output, nil
}

// stagingNameLimit — 임시 파일 이름에 싣는 대상 이름의 최대 바이트 수다. 임시 이름은 대상 이름에
// "."와 ".tmp-<임의 문자 13자 이하>"를 더하므로, 대상 이름을 통째로 쓰면 이름 길이 상한(NAME_MAX,
// 보통 255바이트) 근처의 -o가 수집을 다 마친 뒤 임시 파일을 만들지 못한다. 상한 안이면 대상 이름이
// 아무리 길어도 임시 이름은 119바이트를 넘지 않는다. Kotlin STAGING_NAME_LIMIT과 같다.
const stagingNameLimit = 100

// stagingPrefix — 대상 옆 숨은 임시 파일의 경로 접두다. 대상 이름은 앞 stagingNameLimit바이트까지
// UTF-8 글자 경계에서 자른다. 어느 출력의 임시 파일인지 알아볼 수 있을 만큼만 남긴다.
func stagingPrefix(target string) string {
	name := filepath.Base(target)
	if len(name) > stagingNameLimit {
		cut := stagingNameLimit
		for cut > 0 && !utf8.RuneStart(name[cut]) {
			cut--
		}
		name = name[:cut]
	}
	return filepath.Join(filepath.Dir(target), "."+name+".tmp-")
}

// createStagingFile — 대상 옆에 숨은 임시 파일을 만든다. 같은 디렉터리라 rename이 원자적이다.
func createStagingFile(target string) (*os.File, error) {
	prefix := stagingPrefix(target)
	for range 100 {
		file, err := os.OpenFile(prefix+strconv.FormatUint(rand.Uint64(), 36), os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o644)
		if !errors.Is(err, fs.ErrExist) {
			return file, err
		}
	}
	return nil, fmt.Errorf("cannot create a temporary file next to %s; remove stale %s* files", target, prefix)
}

func (o *stagedOutput) Write(data []byte) (int, error) {
	return o.file.Write(data)
}

// commit — 끝까지 쓴 임시 파일을 동기화하고 닫은 뒤 target으로 rename한다. 실패하면 임시 파일을
// 지워 이전 파일을 그대로 둔다. 제자리 쓰기는 닫기만 한다.
func (o *stagedOutput) commit() error {
	if o.temporary == "" {
		return o.file.Close()
	}
	if err := o.file.Sync(); err != nil {
		return errors.Join(err, o.discard())
	}
	if err := o.file.Close(); err != nil {
		return errors.Join(err, os.Remove(o.temporary))
	}
	if err := os.Rename(o.temporary, o.target); err != nil {
		return errors.Join(err, os.Remove(o.temporary))
	}
	return nil
}

// discard — 실패한 쓰기를 버린다. 임시 파일은 지운다. 제자리 쓰기는 지금처럼 쓴 만큼 남는다.
func (o *stagedOutput) discard() error {
	closeErr := o.file.Close()
	if o.temporary == "" {
		return closeErr
	}
	return errors.Join(closeErr, os.Remove(o.temporary))
}

// writeDocument — 직렬화가 끝난 JSON 문서를 쓰고 교체한다.
func writeDocument(target outputTarget, encoded []byte) error {
	output, err := openOutput(target)
	if err != nil {
		return err
	}
	if _, err := output.Write(encoded); err != nil {
		return errors.Join(err, output.discard())
	}
	return output.commit()
}
