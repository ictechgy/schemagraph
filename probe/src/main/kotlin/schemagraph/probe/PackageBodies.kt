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
 * 카탈로그(ALL_PROCEDURES의 SUBPROGRAM_ID 순)가 준 공개 멤버 하나.
 *
 * [signature]는 ALL_ARGUMENTS.DATA_TYPE을 소문자로 ", " 이은 값이고, 매개변수가 없으면 비어 있다.
 */
internal data class PackageMember(val name: String, val function: Boolean, val signature: String)

/** 멤버 하나의 귀속 결과 — [body]가 null이면 [reason]이 limitation 사유다. */
internal data class PackageMemberSlice(val body: String?, val reason: String?)

/** 그 이름의 정의 헤더를 하나도 확정하지 못한 멤버의 limitation 사유 — Go packageBoundaryNotFound와 같다. */
internal const val PACKAGE_BOUNDARY_NOT_FOUND = "패키지 본문에서 멤버 경계를 못 찾음 — 해당 멤버의 몸체 간선 없음"

/** 오버로드 정의의 본문 순서를 카탈로그 순서와 확인하지 못한 사유 — Go packageOverloadMismatch와 같다. */
private const val PACKAGE_OVERLOAD_MISMATCH = "package body member boundary ambiguous " +
    "(overload headers do not match catalog signatures); member body edges are omitted"

/** kind와 형 목록이 같은 오버로드가 있어 순서를 확인할 수 없는 사유 — Go packageOverloadSignatureShared와 같다. */
private const val PACKAGE_OVERLOAD_SIGNATURE_SHARED = "package body member boundary ambiguous " +
    "(overloads share a catalog signature); member body edges are omitted"

/**
 * 카탈로그 멤버마다 PACKAGE BODY의 원문 조각을 귀속한다. 결과는 [members]와 같은 순서다.
 *
 * 같은 이름의 정의 수가 카탈로그 멤버 수와 다르면(스펙에 없는 비공개 오버로드 등) 어느 정의가
 * 공개 멤버인지 원문만으로 정할 수 없어 그 이름의 멤버를 모두 limitation으로 남긴다. 오버로드는
 * 본문 순서가 스펙 순서와 같다는 보장이 없어, 오버로드마다 kind와 형 목록이 다르고 n번째 정의의
 * kind와 형 목록이 n번째 카탈로그 멤버와 모두 같을 때만 짝짓는다. 추측해 귀속하면 다른
 * 서브프로그램의 간선이 limitation 없이 붙는다. Go attributePackageMembers와 같은 규칙이다.
 */
internal fun attributePackageMembers(impl: String, members: List<PackageMember>): List<PackageMemberSlice> {
    val heads = packageRoutineHeads(impl, members.map { it.name }.toSet()).groupBy { it.name }
    val problems = members.groupBy { it.name }
        .mapValues { (name, named) -> packageAttributionProblem(heads[name].orEmpty(), named) }
    val seen = mutableMapOf<String, Int>()
    return members.map { member ->
        val n = seen.getOrDefault(member.name, 0)
        seen[member.name] = n + 1
        val problem = problems[member.name]
        // 사유가 없으면 그 이름의 정의 수가 카탈로그 멤버 수와 같으므로 n번째 정의가 있다.
        if (problem != null) PackageMemberSlice(null, problem)
        else heads.getValue(member.name)[n].let { PackageMemberSlice(impl.substring(it.start, it.end).trim(), null) }
    }
}

/**
 * 한 이름의 정의 헤더를 카탈로그 멤버와 순서대로 짝지을 수 없는 사유. 짝지을 수 있으면 null이다.
 *
 * 매개변수 이름만 다른 오버로드처럼 kind와 형 목록이 같은 멤버가 있으면 순서 대조가 아무것도
 * 확인하지 못해, 본문 순서가 스펙과 달라도 몸체가 뒤바뀐 채 귀속되므로 짝짓지 않는다.
 */
private fun packageAttributionProblem(heads: List<PackageRoutineHead>, members: List<PackageMember>): String? = when {
    heads.isEmpty() -> PACKAGE_BOUNDARY_NOT_FOUND
    heads.size != members.size -> "package body member boundary ambiguous " +
        "(${heads.size} headers vs ${members.size} catalog members); member body edges are omitted"
    members.size == 1 -> null
    members.distinct().size != members.size -> PACKAGE_OVERLOAD_SIGNATURE_SHARED
    heads.zip(members).any { (head, member) ->
        head.function != member.function || head.signature != member.signature
    } -> PACKAGE_OVERLOAD_MISMATCH
    else -> null
}

/**
 * 패키지 수준에서 경계를 확정한 서브프로그램 정의 하나. [start]·[end]는 헤더 키워드부터 종료
 * 세미콜론까지의 원문 범위이고, [signature]는 [packageHeaderSignature] 값이다.
 */
private class PackageRoutineHead(
    val name: String,
    val function: Boolean,
    val signature: String,
    val start: Int,
    val end: Int,
)

/**
 * 카탈로그 멤버와 이름이 같은 패키지 수준 정의를 본문 순서로 찾는다.
 *
 * 정규식으로는 주석·문자열·q-quote의 키워드와 로컬 서브프로그램을
 * 구별할 수 없으므로 SQL 의미를 해석하지 않는 lexical scanner를 사용한다.
 * 본문 없는 전방 선언은 정의가 아니다. 경계를 확인하지 못한 정의를 만나면
 * 그때까지 찾은 정의만 돌려주고, 빠진 멤버는 호출부가 limitation으로 보고한다.
 */
private fun packageRoutineHeads(impl: String, members: Set<String>): List<PackageRoutineHead> {
    val tokens = lexPackageBody(impl)
    if (tokens.isEmpty() || members.isEmpty()) return emptyList()
    val canonical = members.filter { it.isNotEmpty() }.sorted()
    val heads = mutableListOf<PackageRoutineHead>()

    var index = 0
    while (index < tokens.size) {
        // 패키지 초기화 블록 안의 로컬 루틴은 public member가 아니다.
        if (packageLexTokenIs(tokens[index], "BEGIN")) break
        if (!isPackageRoutineHeader(tokens, index)) {
            index++
            continue
        }
        val declaration = packageRoutineDeclaration(tokens, index) ?: break
        if (declaration.forward) {
            index = declaration.index + 1
            continue
        }
        val endToken = packageRoutineEnd(tokens, declaration.index) ?: break
        val end = packageRoutineTerminator(tokens, endToken) ?: break
        packageCanonicalName(tokens[index + 1].text, canonical)?.let { name ->
            heads += PackageRoutineHead(
                name = name,
                function = tokens[index].text.equals("FUNCTION", ignoreCase = true),
                signature = packageHeaderSignature(tokens, index),
                start = tokens[index].start,
                end = end,
            )
        }
        index = endToken + 1
    }
    return heads
}

/** [packageRoutineHeads]의 조각을 "이름#본문 출현 순번"으로 모은다. 카탈로그 짝짓기는 [attributePackageMembers]에 있다. */
internal fun slicePackageBodyLexical(impl: String, members: Set<String>): Map<String, String> {
    val occurrences = mutableMapOf<String, Int>()
    val out = linkedMapOf<String, String>()
    for (head in packageRoutineHeads(impl, members)) {
        val occurrence = occurrences.getOrDefault(head.name, 0)
        occurrences[head.name] = occurrence + 1
        out["${head.name}#$occurrence"] = impl.substring(head.start, head.end).trim()
    }
    return out
}

/**
 * [endToken] 뒤 첫 ';'의 끝 위치 — 멤버의 종료 세미콜론이다. 멤버마다 불리므로 남은 토큰을
 * 복사하지 않고 제자리에서 훑는다(복사하면 패키지 하나가 멤버 수 × 토큰 수만큼 할당한다).
 */
private fun packageRoutineTerminator(tokens: List<PackageToken>, endToken: Int): Int? {
    for (index in endToken + 1 until tokens.size) {
        if (packageSymbolIs(tokens[index], ";")) return tokens[index].end
    }
    return null
}

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

private fun packageSymbolIs(token: PackageToken, symbol: String): Boolean =
    token.kind == PACKAGE_SYMBOL && token.text == symbol

/** 헤더가 끝나는 토큰 — [forward]면 본문 없는 전방 선언의 ';', 아니면 정의의 IS/AS다. */
private class PackageDeclaration(val index: Int, val forward: Boolean)

/**
 * 헤더 뒤 괄호 밖에서 처음 나오는 ';' 또는 IS/AS를 찾는다. 전방 선언을 정의로 세면 그 뒤의
 * 다른 멤버와 초기화 블록까지 한 조각으로 묶인다. 둘 다 없으면 null이다(Go packageRoutineDeclaration).
 */
private fun packageRoutineDeclaration(tokens: List<PackageToken>, header: Int): PackageDeclaration? {
    var depth = 0
    for (index in header + 2 until tokens.size) {
        val token = tokens[index]
        when {
            packageSymbolIs(token, "(") -> depth++
            packageSymbolIs(token, ")") -> if (depth > 0) depth--
            // 매개변수 목록·ACCESSIBLE BY 같은 괄호 안의 ';'와 IS/AS는 헤더 끝이 아니다.
            depth > 0 -> Unit
            packageSymbolIs(token, ";") -> return PackageDeclaration(index, forward = true)
            packageLexTokenIs(token, "IS") || packageLexTokenIs(token, "AS") ->
                return PackageDeclaration(index, forward = false)
        }
    }
    return null
}

/**
 * 헤더의 형식 매개변수 형을 소문자로 ", " 이어 돌려준다. 카탈로그 ALL_ARGUMENTS.DATA_TYPE 목록과
 * 글자 그대로 비교하려는 값이라 형을 해석하지 않는다. %TYPE이나 하위 형처럼 카탈로그 표기와 다른
 * 형은 오버로드 짝을 확인하지 못한 것으로 남는다(Go packageHeaderSignature).
 */
private fun packageHeaderSignature(tokens: List<PackageToken>, header: Int): String {
    val open = header + 2
    if (open >= tokens.size || !packageSymbolIs(tokens[open], "(")) return ""
    val types = mutableListOf<String>()
    val parameter = mutableListOf<PackageToken>()
    var depth = 0
    for (index in open + 1 until tokens.size) {
        val token = tokens[index]
        if (depth == 0 && (packageSymbolIs(token, ",") || packageSymbolIs(token, ")"))) {
            types += packageParameterType(parameter)
            parameter.clear()
            if (token.text == ")") break
            continue
        }
        if (packageSymbolIs(token, "(")) depth++ else if (packageSymbolIs(token, ")")) depth--
        parameter += token
    }
    return types.joinToString(", ")
}

/** 매개변수 하나(이름, IN·OUT·NOCOPY, 형, DEFAULT나 := 식)에서 형만 잇는다. */
private fun packageParameterType(parameter: List<PackageToken>): String {
    // 첫 토큰은 매개변수 이름이다.
    var index = 1
    while (index < parameter.size && PARAMETER_MODES.any { packageLexTokenIs(parameter[index], it) }) index++
    val words = mutableListOf<String>()
    while (index < parameter.size && !packageLexTokenIs(parameter[index], "DEFAULT") &&
        !packageSymbolIs(parameter[index], ":")
    ) {
        words += parameter[index].text.lowercase()
        index++
    }
    return words.joinToString(" ")
}

/** 형 앞에 올 수 있는 매개변수 모드 키워드. */
private val PARAMETER_MODES = listOf("IN", "OUT", "NOCOPY")

/**
 * 동시에 열린 서브프로그램 정의 수(패키지 수준 멤버 포함)의 상한 — Go maxPackageRoutineDepth와 같다.
 *
 * 컴파일되는 PL/SQL은 블록 중첩 255단에서 멈추므로 정상 본문은 닿지 않는다. 컴파일 오류가
 * 난 본문도 ALL_SOURCE에 원문이 남아, 끝없이 중첩된 헤더가 탐색을 키우지 못하게 넘으면
 * 경계를 못 찾은 것으로 처리한다.
 */
private const val MAX_PACKAGE_ROUTINE_DEPTH = 256

/** [packageRoutineEnd]가 추적하는 열린 routine 하나 — 그 안에서 열린 블록 수와 BEGIN을 지났는지. */
private class PackageRoutineLevel {
    var blocks = 0
    var sawBegin = false
}

/**
 * 정의의 IS/AS([declaration]) 뒤부터 바깥 routine의 END를 찾는다. 중첩 routine 정의는 명시적
 * 스택으로 건너뛰어 로컬 멤버의 BEGIN/END가 바깥 깊이에 섞이지 않게 하고, 로컬 전방 선언은
 * ';'까지 건너뛴다. 재귀하지 않아 조작된 본문이 호출 스택을 소진하지 못하고,
 * [MAX_PACKAGE_ROUTINE_DEPTH]를 넘는 중첩은 경계 없음(null)이다.
 */
private fun packageRoutineEnd(tokens: List<PackageToken>, declaration: Int): Int? {
    val levels = mutableListOf(PackageRoutineLevel())
    var index = declaration + 1
    while (index < tokens.size) {
        val current = levels.last()
        val token = tokens[index]
        when {
            packageLexTokenIs(token, "END") -> if (current.blocks > 0) {
                current.blocks--
                if (current.sawBegin && current.blocks == 0) {
                    levels.removeAt(levels.lastIndex)
                    if (levels.isEmpty()) return index
                }
            }
            isPackageRoutineHeader(tokens, index) -> {
                val nested = packageRoutineDeclaration(tokens, index) ?: return null
                if (!nested.forward) {
                    if (levels.size == MAX_PACKAGE_ROUTINE_DEPTH) return null
                    levels += PackageRoutineLevel()
                }
                // 전방 선언은 ';'까지, 정의는 IS/AS까지 건너뛰고 그 다음 토큰부터 센다.
                index = nested.index
            }
            packageLexTokenIs(token, "BEGIN") -> {
                current.blocks++
                current.sawBegin = true
            }
            packageLexTokenIs(token, "IF") ||
                packageLexTokenIs(token, "LOOP") ||
                packageLexTokenIs(token, "CASE") -> current.blocks++
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
