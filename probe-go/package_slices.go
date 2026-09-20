package main

import (
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

// slicePackageBodyLexical — 카탈로그에 알려진 PACKAGE BODY 멤버만 원문
// 범위로 나눈다. 정규식은 주석·문자열 속 키워드와 로컬 서브프로그램을
// 구별할 수 없으므로, 여기서는 SQL 본문을 해석하지 않고 lexical 경계만
// 추적한다.
func slicePackageBodyLexical(impl string, members map[string]bool) map[string]string {
	tokens := lexPackageBody(impl)
	if len(tokens) == 0 || len(members) == 0 {
		return map[string]string{}
	}
	canonical := packageMemberNames(members)
	type head struct {
		name  string
		start int
		end   int
	}
	var heads []head

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
		endToken, ok := packageRoutineEnd(tokens, i)
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
			heads = append(heads, head{name: name, start: tokens[i].start, end: end})
		}
		i = endToken + 1
	}
	if len(heads) == 0 {
		return map[string]string{}
	}

	// 확인된 routine의 종료 세미콜론까지 원문을 직접 잘라낸다.
	out := make(map[string]string, len(heads))
	occurrence := map[string]int{}
	for _, item := range heads {
		n := occurrence[item.name]
		occurrence[item.name]++
		end := item.end
		if end <= item.start || end > len(impl) {
			continue
		}
		out[item.name+"#"+itoa(n)] = strings.TrimSpace(impl[item.start:end])
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

// packageRoutineEnd는 바깥 routine의 END를 찾는다. 중첩 routine 선언을
// 재귀로 건너뛰어 로컬 멤버의 BEGIN/END가 바깥 깊이에 섞이지 않게 한다.
func packageRoutineEnd(tokens []packageToken, header int) (int, bool) {
	blocks := 0
	sawBegin := false
	for i := header + 2; i < len(tokens); i++ {
		if packageTokenIs(tokens[i], "END") {
			if blocks == 0 {
				continue
			}
			blocks--
			if sawBegin && blocks == 0 {
				return i, true
			}
			continue
		}
		if isPackageRoutineHeader(tokens, i) {
			end, ok := packageRoutineEnd(tokens, i)
			if !ok {
				return 0, false
			}
			i = end
			continue
		}
		switch {
		case packageTokenIs(tokens[i], "BEGIN"):
			blocks++
			sawBegin = true
		case packageTokenIs(tokens[i], "IF"), packageTokenIs(tokens[i], "LOOP"), packageTokenIs(tokens[i], "CASE"):
			blocks++
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
