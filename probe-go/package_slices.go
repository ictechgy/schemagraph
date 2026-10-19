package main

import (
	"fmt"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"
)

const (
	packageTokenWord = iota
	packageTokenQuotedIdentifier
	packageTokenSymbol
)

type packageToken struct {
	kind       int
	text       string
	start, end int
}

// packageMember — 카탈로그(ALL_PROCEDURES의 SUBPROGRAM_ID 순)가 준 공개 멤버 하나다.
// signature는 ALL_ARGUMENTS.DATA_TYPE을 소문자로 ", " 이은 값이고, 매개변수가 없으면 비어 있다.
type packageMember struct {
	name      string
	function  bool
	signature string
}

// packageMemberSlice — 멤버 하나의 귀속 결과다. found가 false면 reason이 limitation 사유다.
type packageMemberSlice struct {
	body   string
	found  bool
	reason string
}

// packageBoundaryNotFound — 그 이름의 정의 헤더를 하나도 확정하지 못한 멤버의 limitation 사유다.
const packageBoundaryNotFound = "패키지 본문에서 멤버 경계를 못 찾음 — 해당 멤버의 몸체 간선 없음"

// packageOverloadMismatch — 오버로드 정의의 본문 순서를 카탈로그 순서와 확인하지 못한 사유다.
const packageOverloadMismatch = "package body member boundary ambiguous " +
	"(overload headers do not match catalog signatures); member body edges are omitted"

// packageOverloadSignatureShared — kind와 형 목록이 같은 오버로드가 있어 순서를 확인할 수 없는 사유다.
const packageOverloadSignatureShared = "package body member boundary ambiguous " +
	"(overloads share a catalog signature); member body edges are omitted"

// attributePackageMembers — 카탈로그 멤버마다 PACKAGE BODY의 원문 조각을 귀속한다.
// 같은 이름의 정의 수가 카탈로그 멤버 수와 다르면(스펙에 없는 비공개 오버로드 등) 어느
// 정의가 공개 멤버인지 원문만으로 정할 수 없어 그 이름의 멤버를 모두 limitation으로 남긴다.
// 오버로드는 본문 순서가 스펙 순서와 같다는 보장이 없어, 오버로드마다 kind와 형 목록이 다르고
// n번째 정의의 kind와 형 목록이 n번째 카탈로그 멤버와 모두 같을 때만 짝짓는다. 추측해 귀속하면
// 다른 서브프로그램의 간선이 limitation 없이 붙는다. 결과는 members와 같은 순서다.
// Kotlin attributePackageMembers와 같은 규칙이다.
func attributePackageMembers(impl string, members []packageMember) []packageMemberSlice {
	names := map[string]bool{}
	byName := map[string][]int{}
	for i, member := range members {
		names[member.name] = true
		byName[member.name] = append(byName[member.name], i)
	}
	heads := map[string][]packageRoutineHead{}
	for _, head := range packageRoutineHeads(impl, names) {
		heads[head.name] = append(heads[head.name], head)
	}
	out := make([]packageMemberSlice, len(members))
	for name, indexes := range byName {
		named := heads[name]
		problem := packageAttributionProblem(named, members, indexes)
		for n, index := range indexes {
			if problem != "" {
				out[index] = packageMemberSlice{reason: problem}
				continue
			}
			out[index] = packageMemberSlice{body: strings.TrimSpace(impl[named[n].start:named[n].end]), found: true}
		}
	}
	return out
}

// packageAttributionProblem — 한 이름의 정의 헤더를 카탈로그 멤버와 순서대로 짝지을 수 없는
// 사유를 돌려준다. 짝지을 수 있으면 빈 문자열이다.
func packageAttributionProblem(heads []packageRoutineHead, members []packageMember, indexes []int) string {
	switch {
	case len(heads) == 0:
		return packageBoundaryNotFound
	case len(heads) != len(indexes):
		return fmt.Sprintf("package body member boundary ambiguous (%d headers vs %d catalog members); "+
			"member body edges are omitted", len(heads), len(indexes))
	case len(indexes) == 1:
		return ""
	case packageOverloadsShareSignature(members, indexes):
		return packageOverloadSignatureShared
	}
	for n, index := range indexes {
		if heads[n].function != members[index].function || heads[n].signature != members[index].signature {
			return packageOverloadMismatch
		}
	}
	return ""
}

// packageOverloadsShareSignature — 같은 이름의 카탈로그 오버로드 가운데 kind와 형 목록이 같은 쌍이
// 있는지 본다. 매개변수 이름만 다른 오버로드처럼 형 목록이 같으면 순서 대조가 아무것도 확인하지
// 못해, 본문 순서가 스펙과 달라도 몸체가 뒤바뀐 채 귀속된다.
func packageOverloadsShareSignature(members []packageMember, indexes []int) bool {
	seen := make(map[packageMember]bool, len(indexes))
	for _, index := range indexes {
		if seen[members[index]] {
			return true
		}
		seen[members[index]] = true
	}
	return false
}

// packageRoutineHead — 패키지 수준에서 경계를 확정한 서브프로그램 정의 하나다. start·end는
// 헤더 키워드부터 종료 세미콜론까지의 원문 범위이고, signature는 packageHeaderSignature 값이다.
type packageRoutineHead struct {
	name       string
	function   bool
	signature  string
	start, end int
}

// packageRoutineHeads — 카탈로그 멤버와 이름이 같은 패키지 수준 정의를 본문 순서로 찾는다.
// 정규식은 주석·문자열 속 키워드와 로컬 서브프로그램을 구별할 수 없으므로, 여기서는 SQL
// 본문을 해석하지 않고 lexical 경계만 추적한다. 본문 없는 전방 선언은 정의가 아니다.
func packageRoutineHeads(impl string, members map[string]bool) []packageRoutineHead {
	tokens := lexPackageBody(impl)
	if len(tokens) == 0 || len(members) == 0 {
		return nil
	}
	canonical := packageMemberNames(members)
	var heads []packageRoutineHead
	for i := 0; i < len(tokens); {
		// PACKAGE BODY의 초기화 블록 뒤에는 멤버 선언이 없으므로, 그 안의
		// 로컬 루틴을 package-level member로 잘못 귀속하지 않는다.
		if packageTokenIs(tokens[i], "BEGIN") {
			break
		}
		if !isPackageRoutineHeader(tokens, i) {
			i++
			continue
		}
		declaration, forward, ok := packageRoutineDeclaration(tokens, i)
		if !ok {
			break
		}
		if forward {
			i = declaration + 1
			continue
		}
		endToken, ok := packageRoutineEnd(tokens, declaration)
		if !ok {
			// 이후 토큰은 이 루틴의 로컬 선언일 수도 있어 경계를 추측하지
			// 않는다. 이미 확인한 멤버만 반환해 호출부가 limitation을 낸다.
			break
		}
		end, ok := packageRoutineTerminator(tokens, endToken)
		if !ok {
			break
		}
		if name, ok := packageCanonicalName(tokens[i+1].text, canonical); ok {
			heads = append(heads, packageRoutineHead{
				name: name, function: strings.EqualFold(tokens[i].text, "FUNCTION"),
				signature: packageHeaderSignature(tokens, i), start: tokens[i].start, end: end,
			})
		}
		i = endToken + 1
	}
	return heads
}

// slicePackageBodyLexical — packageRoutineHeads의 조각을 "이름#본문 출현 순번"으로 모은다.
// 카탈로그 멤버와 짝짓는 규칙은 attributePackageMembers에 있다.
func slicePackageBodyLexical(impl string, members map[string]bool) map[string]string {
	out := map[string]string{}
	occurrence := map[string]int{}
	for _, head := range packageRoutineHeads(impl, members) {
		n := occurrence[head.name]
		occurrence[head.name]++
		out[head.name+"#"+itoa(n)] = strings.TrimSpace(impl[head.start:head.end])
	}
	return out
}

func packageRoutineTerminator(tokens []packageToken, endToken int) (int, bool) {
	for i := endToken + 1; i < len(tokens); i++ {
		if tokens[i].kind == packageTokenSymbol && tokens[i].text == ";" {
			return tokens[i].end, true
		}
	}
	return 0, false
}

func packageMemberNames(members map[string]bool) []string {
	names := make([]string, 0, len(members))
	for name, present := range members {
		if present {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	return names
}

func packageCanonicalName(got string, names []string) (string, bool) {
	for _, name := range names {
		if name == got {
			return name, true
		}
	}
	for _, name := range names {
		if strings.EqualFold(name, got) {
			return name, true
		}
	}
	return "", false
}

func isPackageRoutineHeader(tokens []packageToken, index int) bool {
	if index+1 >= len(tokens) || !isPackageRoutineKeyword(tokens[index].text) {
		return false
	}
	return tokens[index+1].kind == packageTokenWord || tokens[index+1].kind == packageTokenQuotedIdentifier
}

func isPackageRoutineKeyword(text string) bool {
	return strings.EqualFold(text, "PROCEDURE") || strings.EqualFold(text, "FUNCTION")
}

func packageTokenIs(token packageToken, word string) bool {
	return token.kind == packageTokenWord && strings.EqualFold(token.text, word)
}

func packageSymbolIs(token packageToken, symbol string) bool {
	return token.kind == packageTokenSymbol && token.text == symbol
}

// packageRoutineDeclaration — 헤더 뒤 괄호 밖에서 처음 나오는 ';' 또는 IS/AS를 찾는다.
// ';'가 먼저면 본문 없는 전방 선언(forward)이고 IS/AS가 먼저면 정의다. 전방 선언을 정의로
// 세면 그 뒤의 다른 멤버와 초기화 블록까지 한 조각으로 묶인다. 둘 다 없으면 ok가 false다.
func packageRoutineDeclaration(tokens []packageToken, header int) (index int, forward bool, ok bool) {
	depth := 0
	for i := header + 2; i < len(tokens); i++ {
		switch {
		case packageSymbolIs(tokens[i], "("):
			depth++
		case packageSymbolIs(tokens[i], ")"):
			if depth > 0 {
				depth--
			}
		case depth > 0:
			// 매개변수 목록·ACCESSIBLE BY 같은 괄호 안의 ';'와 IS/AS는 헤더 끝이 아니다.
		case packageSymbolIs(tokens[i], ";"):
			return i, true, true
		case packageTokenIs(tokens[i], "IS"), packageTokenIs(tokens[i], "AS"):
			return i, false, true
		}
	}
	return 0, false, false
}

// packageHeaderSignature — 헤더의 형식 매개변수 형을 소문자로 ", " 이어 돌려준다. 카탈로그
// ALL_ARGUMENTS.DATA_TYPE 목록과 글자 그대로 비교하려는 값이라 형을 해석하지 않는다.
// %TYPE이나 하위 형처럼 카탈로그 표기와 다른 형은 오버로드 짝을 확인하지 못한 것으로 남는다.
func packageHeaderSignature(tokens []packageToken, header int) string {
	open := header + 2
	if open >= len(tokens) || !packageSymbolIs(tokens[open], "(") {
		return ""
	}
	var types []string
	var parameter []packageToken
	depth := 0
	for i := open + 1; i < len(tokens); i++ {
		if depth == 0 && (packageSymbolIs(tokens[i], ",") || packageSymbolIs(tokens[i], ")")) {
			types = append(types, packageParameterType(parameter))
			parameter = parameter[:0]
			if tokens[i].text == ")" {
				break
			}
			continue
		}
		if packageSymbolIs(tokens[i], "(") {
			depth++
		} else if packageSymbolIs(tokens[i], ")") {
			depth--
		}
		parameter = append(parameter, tokens[i])
	}
	return strings.Join(types, ", ")
}

// packageParameterType — 매개변수 하나(이름, IN·OUT·NOCOPY, 형, DEFAULT나 := 식)에서 형만 잇는다.
func packageParameterType(parameter []packageToken) string {
	i := 1 // 첫 토큰은 매개변수 이름이다.
	for i < len(parameter) && (packageTokenIs(parameter[i], "IN") || packageTokenIs(parameter[i], "OUT") ||
		packageTokenIs(parameter[i], "NOCOPY")) {
		i++
	}
	var words []string
	for ; i < len(parameter); i++ {
		if packageTokenIs(parameter[i], "DEFAULT") || packageSymbolIs(parameter[i], ":") {
			break
		}
		words = append(words, strings.ToLower(parameter[i].text))
	}
	return strings.Join(words, " ")
}

// maxPackageRoutineDepth — 동시에 열린 서브프로그램 정의 수(패키지 수준 멤버 포함)의
// 상한이다. Kotlin MAX_PACKAGE_ROUTINE_DEPTH와 같다. 컴파일되는 PL/SQL은 블록 중첩
// 255단에서 멈추므로 정상 본문은 닿지 않는다. 컴파일 오류가 난 본문도 ALL_SOURCE에 원문이
// 남아, 끝없이 중첩된 헤더가 탐색을 키우지 못하게 넘으면 경계를 못 찾은 것으로 처리한다.
const maxPackageRoutineDepth = 256

// packageRoutineLevel — packageRoutineEnd가 추적하는 열린 routine 하나다. blocks는 그 안에서 열린
// 블록 머리(BEGIN·IF·LOOP·CASE)를 연 순서대로 담는다.
type packageRoutineLevel struct {
	blocks   []string
	sawBegin bool
}

// packageBlockKeywords — END로 닫히는 블록 머리다. IF·LOOP·CASE 문은 END 뒤에 같은 단어를 한 번 더
// 쓴다(END IF·END LOOP·END CASE). CASE 식은 END만 쓴다.
var packageBlockKeywords = []string{"BEGIN", "IF", "LOOP", "CASE"}

// packageBlockKeyword — 토큰이 블록 머리면 그 키워드를 돌려준다.
func packageBlockKeyword(token packageToken) (string, bool) {
	for _, keyword := range packageBlockKeywords {
		if packageTokenIs(token, keyword) {
			return keyword, true
		}
	}
	return "", false
}

// packageRoutineEnd는 정의의 IS/AS(declaration) 뒤부터 바깥 routine의 END를 찾는다. 중첩
// routine 정의는 명시적 스택으로 건너뛰어 로컬 멤버의 BEGIN/END가 바깥 깊이에 섞이지 않게
// 하고, 로컬 전방 선언은 ';'까지 건너뛴다. 재귀하지 않아 조작된 본문이 탐색을 키우지 못하고,
// maxPackageRoutineDepth를 넘는 중첩은 경계 없음이다.
func packageRoutineEnd(tokens []packageToken, declaration int) (int, bool) {
	levels := []packageRoutineLevel{{}}
	for i := declaration + 1; i < len(tokens); i++ {
		current := &levels[len(levels)-1]
		if packageTokenIs(tokens[i], "END") {
			if len(current.blocks) == 0 {
				continue
			}
			end := i
			closed := current.blocks[len(current.blocks)-1]
			current.blocks = current.blocks[:len(current.blocks)-1]
			// END IF·END LOOP·END CASE의 꼬리 단어는 방금 닫은 블록의 표지다. 새 블록으로 세면 그런
			// 문이 있는 멤버는 제 END에서 깊이가 0이 되지 않아 뒤 멤버와 패키지 END까지 삼킨다.
			// 꼬리가 닫은 블록과 다르면 새 블록 머리다(FOR i IN 1 .. CASE … END LOOP의 LOOP).
			if closed != "BEGIN" && i+1 < len(tokens) && packageTokenIs(tokens[i+1], closed) {
				i++
			}
			if current.sawBegin && len(current.blocks) == 0 {
				levels = levels[:len(levels)-1]
				if len(levels) == 0 {
					return end, true
				}
			}
			continue
		}
		if isPackageRoutineHeader(tokens, i) {
			nested, forward, ok := packageRoutineDeclaration(tokens, i)
			if !ok {
				return 0, false
			}
			if !forward {
				if len(levels) == maxPackageRoutineDepth {
					return 0, false
				}
				levels = append(levels, packageRoutineLevel{})
			}
			// 전방 선언은 ';'까지, 정의는 IS/AS까지 건너뛰고 그 다음 토큰부터 센다.
			i = nested
			continue
		}
		if keyword, ok := packageBlockKeyword(tokens[i]); ok {
			current.blocks = append(current.blocks, keyword)
			current.sawBegin = current.sawBegin || keyword == "BEGIN"
		}
	}
	return 0, false
}

func lexPackageBody(input string) []packageToken {
	var tokens []packageToken
	for i := 0; i < len(input); {
		if isPackageSpace(input[i]) {
			i++
			continue
		}
		if strings.HasPrefix(input[i:], "--") {
			i = skipPackageLineComment(input, i+2)
			continue
		}
		if strings.HasPrefix(input[i:], "/*") {
			i = skipPackageBlockComment(input, i+2)
			continue
		}
		if input[i] == '\'' {
			i = skipPackageString(input, i+1)
			continue
		}
		if (input[i] == 'q' || input[i] == 'Q') && i+1 < len(input) && input[i+1] == '\'' {
			i = skipPackageQuote(input, i)
			continue
		}
		if input[i] == '"' {
			text, end := readPackageQuotedIdentifier(input, i)
			tokens = append(tokens, packageToken{kind: packageTokenQuotedIdentifier, text: text, start: i, end: end})
			i = end
			continue
		}
		r, size := utf8.DecodeRuneInString(input[i:])
		if isPackageIdentifierStart(r) {
			start := i
			i += size
			for i < len(input) {
				next, width := utf8.DecodeRuneInString(input[i:])
				if !isPackageIdentifierPart(next) {
					break
				}
				i += width
			}
			tokens = append(tokens, packageToken{kind: packageTokenWord, text: input[start:i], start: start, end: i})
			continue
		}
		tokens = append(tokens, packageToken{kind: packageTokenSymbol, text: input[i : i+size], start: i, end: i + size})
		i += size
	}
	return tokens
}

func isPackageSpace(value byte) bool {
	return value == ' ' || value == '\t' || value == '\r' || value == '\n' || value == '\f'
}

func isPackageIdentifierStart(value rune) bool {
	return value == '_' || value == '$' || value == '#' || unicode.IsLetter(value)
}

func isPackageIdentifierPart(value rune) bool {
	return isPackageIdentifierStart(value) || unicode.IsDigit(value)
}

func skipPackageLineComment(input string, index int) int {
	for index < len(input) && input[index] != '\n' {
		index++
	}
	return index
}

func skipPackageBlockComment(input string, index int) int {
	depth := 1
	for index < len(input) {
		if strings.HasPrefix(input[index:], "/*") {
			depth++
			index += 2
		} else if strings.HasPrefix(input[index:], "*/") {
			depth--
			index += 2
			if depth == 0 {
				return index
			}
		} else {
			_, width := utf8.DecodeRuneInString(input[index:])
			index += width
		}
	}
	return len(input)
}

func skipPackageString(input string, index int) int {
	for index < len(input) {
		if input[index] != '\'' {
			_, width := utf8.DecodeRuneInString(input[index:])
			index += width
			continue
		}
		if index+1 < len(input) && input[index+1] == '\'' {
			index += 2
			continue
		}
		return index + 1
	}
	return len(input)
}

func skipPackageQuote(input string, index int) int {
	_, qWidth := utf8.DecodeRuneInString(input[index:])
	openIndex := index + qWidth + 1
	if openIndex >= len(input) {
		return len(input)
	}
	open, openWidth := utf8.DecodeRuneInString(input[openIndex:])
	close := packageQuoteClose(open)
	for i := openIndex + openWidth; i < len(input); {
		value, width := utf8.DecodeRuneInString(input[i:])
		if value == close && i+width < len(input) && input[i+width] == '\'' {
			return i + width + 1
		}
		i += width
	}
	return len(input)
}

func packageQuoteClose(open rune) rune {
	switch open {
	case '[':
		return ']'
	case '{':
		return '}'
	case '(':
		return ')'
	case '<':
		return '>'
	case '«':
		return '»'
	case '‹':
		return '›'
	case '“':
		return '”'
	case '‘':
		return '’'
	default:
		return open
	}
}

func readPackageQuotedIdentifier(input string, index int) (string, int) {
	var b strings.Builder
	for i := index + 1; i < len(input); {
		if input[i] == '"' {
			if i+1 < len(input) && input[i+1] == '"' {
				b.WriteByte('"')
				i += 2
				continue
			}
			return b.String(), i + 1
		}
		_, width := utf8.DecodeRuneInString(input[i:])
		b.WriteString(input[i : i+width])
		i += width
	}
	return b.String(), len(input)
}

func itoa(value int) string {
	if value == 0 {
		return "0"
	}
	var buf [20]byte
	i := len(buf)
	for value > 0 {
		i--
		buf[i] = byte('0' + value%10)
		value /= 10
	}
	return string(buf[i:])
}
