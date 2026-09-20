package schemagraph.probe

private const val PACKAGE_WORD = 0
private const val PACKAGE_QUOTED_IDENTIFIER = 1
private const val PACKAGE_SYMBOL = 2

private data class PackageToken(
    val kind: Int,
    val text: String,
    val start: Int,
    val end: Int,
)

/**
 * PACKAGE BODY를 catalog 멤버의 원문 범위로 나눈다.
 *
 * 정규식으로는 주석·문자열·q-quote의 키워드와 로컬 서브프로그램을
 * 구별할 수 없으므로 SQL 의미를 해석하지 않는 lexical scanner를 사용한다.
 * 경계를 확인하지 못한 멤버는 결과에서 빼 호출부가 limitation을 보고한다.
 */
internal fun slicePackageBodyLexical(impl: String, members: Set<String>): Map<String, String> {
    val tokens = lexPackageBody(impl)
    if (tokens.isEmpty() || members.isEmpty()) return emptyMap()
    val canonical = members.filter { it.isNotEmpty() }.sorted()
    data class Head(val name: String, val start: Int, val end: Int)
    val heads = mutableListOf<Head>()

    var index = 0
    while (index < tokens.size) {
        // 패키지 초기화 블록 안의 로컬 루틴은 public member가 아니다.
        if (packageLexTokenIs(tokens[index], "BEGIN")) break
        if (!isPackageRoutineHeader(tokens, index)) {
            index++
            continue
        }
        val endToken = packageRoutineEnd(tokens, index) ?: break
        val end = packageRoutineTerminator(tokens, endToken) ?: break
        packageCanonicalName(tokens[index + 1].text, canonical)?.let { name ->
            heads += Head(name, tokens[index].start, end)
        }
        index = endToken + 1
    }
    if (heads.isEmpty()) return emptyMap()

    val occurrences = mutableMapOf<String, Int>()
    val out = linkedMapOf<String, String>()
    for (head in heads) {
        val occurrence = occurrences.getOrDefault(head.name, 0)
        occurrences[head.name] = occurrence + 1
        val end = head.end
        if (end <= head.start || end > impl.length) continue
        out["${head.name}#$occurrence"] = impl.substring(head.start, end).trim()
    }
    return out
}

private fun packageRoutineTerminator(tokens: List<PackageToken>, endToken: Int): Int? =
    tokens.drop(endToken + 1)
        .firstOrNull { it.kind == PACKAGE_SYMBOL && it.text == ";" }
        ?.end

private fun packageCanonicalName(got: String, names: List<String>): String? =
    names.firstOrNull { it == got }
        ?: names.firstOrNull { it.equals(got, ignoreCase = true) }

private fun isPackageRoutineHeader(tokens: List<PackageToken>, index: Int): Boolean {
    if (index + 1 >= tokens.size || !isPackageRoutineKeyword(tokens[index].text)) return false
    return tokens[index + 1].kind == PACKAGE_WORD || tokens[index + 1].kind == PACKAGE_QUOTED_IDENTIFIER
}

private fun isPackageRoutineKeyword(text: String): Boolean =
    text.equals("PROCEDURE", ignoreCase = true) || text.equals("FUNCTION", ignoreCase = true)

private fun packageLexTokenIs(token: PackageToken, word: String): Boolean =
    token.kind == PACKAGE_WORD && token.text.equals(word, ignoreCase = true)

private fun packageRoutineEnd(tokens: List<PackageToken>, header: Int): Int? {
    var blocks = 0
    var sawBegin = false
    var index = header + 2
    while (index < tokens.size) {
        if (packageLexTokenIs(tokens[index], "END")) {
            if (blocks == 0) {
                index++
                continue
            }
            blocks--
            if (sawBegin && blocks == 0) return index
            index++
            continue
        }
        if (isPackageRoutineHeader(tokens, index)) {
            val nestedEnd = packageRoutineEnd(tokens, index) ?: return null
            index = nestedEnd + 1
            continue
        }
        when {
            packageLexTokenIs(tokens[index], "BEGIN") -> {
                blocks++
                sawBegin = true
            }
            packageLexTokenIs(tokens[index], "IF") ||
                packageLexTokenIs(tokens[index], "LOOP") ||
                packageLexTokenIs(tokens[index], "CASE") -> blocks++
        }
        index++
    }
    return null
}

private fun lexPackageBody(input: String): List<PackageToken> {
    val tokens = mutableListOf<PackageToken>()
    var index = 0
    while (index < input.length) {
        if (input[index].isWhitespace()) {
            index++
            continue
        }
        when {
            input.startsWith("--", index) -> index = skipPackageLineComment(input, index + 2)
            input.startsWith("/*", index) -> index = skipPackageBlockComment(input, index + 2)
            input[index] == '\'' -> index = skipPackageString(input, index + 1)
            (input[index] == 'q' || input[index] == 'Q') &&
                index + 1 < input.length && input[index + 1] == '\'' ->
                index = skipPackageQuote(input, index)
            input[index] == '"' -> {
                val (text, end) = readPackageQuotedIdentifier(input, index)
                tokens += PackageToken(PACKAGE_QUOTED_IDENTIFIER, text, index, end)
                index = end
            }
            isPackageIdentifierStart(input[index]) -> {
                val start = index++
                while (index < input.length && isPackageIdentifierPart(input[index])) index++
                tokens += PackageToken(PACKAGE_WORD, input.substring(start, index), start, index)
            }
            else -> {
                tokens += PackageToken(PACKAGE_SYMBOL, input[index].toString(), index, index + 1)
                index++
            }
        }
    }
    return tokens
}

private fun isPackageIdentifierStart(value: Char): Boolean =
    value == '_' || value == '$' || value == '#' || value.isLetter()

private fun isPackageIdentifierPart(value: Char): Boolean =
    isPackageIdentifierStart(value) || value.isDigit()

private fun skipPackageLineComment(input: String, start: Int): Int =
    input.indexOf('\n', start).let { if (it < 0) input.length else it }

private fun skipPackageBlockComment(input: String, start: Int): Int {
    var depth = 1
    var index = start
    while (index < input.length) {
        when {
            input.startsWith("/*", index) -> {
                depth++
                index += 2
            }
            input.startsWith("*/", index) -> {
                depth--
                index += 2
                if (depth == 0) return index
            }
            else -> index++
        }
    }
    return input.length
}

private fun skipPackageString(input: String, start: Int): Int {
    var index = start
    while (index < input.length) {
        if (input[index] != '\'') {
            index++
            continue
        }
        if (index + 1 < input.length && input[index + 1] == '\'') {
            index += 2
            continue
        }
        return index + 1
    }
    return input.length
}

private fun skipPackageQuote(input: String, start: Int): Int {
    val openIndex = start + 2
    if (openIndex >= input.length) return input.length
    val close = packageQuoteClose(input[openIndex])
    var index = openIndex + 1
    while (index < input.length) {
        if (input[index] == close && index + 1 < input.length && input[index + 1] == '\'') {
            return index + 2
        }
        index++
    }
    return input.length
}

private fun packageQuoteClose(open: Char): Char = when (open) {
    '[' -> ']'
    '{' -> '}'
    '(' -> ')'
    '<' -> '>'
    '«' -> '»'
    '‹' -> '›'
    '“' -> '”'
    '‘' -> '’'
    else -> open
}

private fun readPackageQuotedIdentifier(input: String, start: Int): Pair<String, Int> {
    val result = StringBuilder()
    var index = start + 1
    while (index < input.length) {
        if (input[index] == '"') {
            if (index + 1 < input.length && input[index + 1] == '"') {
                result.append('"')
                index += 2
                continue
            }
            return result.toString() to index + 1
        }
        result.append(input[index++])
    }
    return result.toString() to input.length
}
