package schemagraph.probe

import java.io.BufferedWriter
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.nio.channels.Channels
import java.nio.channels.FileChannel
import java.nio.file.FileAlreadyExistsException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import java.nio.file.attribute.PosixFilePermission
import java.util.UUID

// 프로브 출력 파일 보호. 엔진 #35의 산출물 계약(DESIGN.md "파일 산출물은 준비 후 교체한다")은
// 엔진 명령에 한정돼 있어, 프로브는 데이터 손실을 막는 두 가지만 따른다: SQLite 원본을 가리키는
// -o는 연결 전에 거절하고, 파일은 끝까지 쓴 뒤 교체한다. 새 파일 권한(0666&~umask)과 symlink를
// 따라 대상 파일을 갱신하는 동작은 바꾸지 않는다. probe-go/output.go와 같은 규칙이다.

/** 원본을 덮으면 JSON은 수집 뒤 DB를 문서로 바꾸고, NDJSON은 수집 전에 DB를 비운 뒤 빈 카탈로그를 냈다. */
internal const val SQLITE_SOURCE_ALIAS_ERROR =
    "output must differ from the SQLite database and its -wal/-shm files; choose another -o path"

/** 장치와 디스크립터 별칭이 사는 곳 — macOS는 파일로 리디렉션된 /dev/stdout을 일반 파일로 보고한다. */
private val DEVICE_DIRECTORY: Path = Path.of("/dev")

/**
 * 연결 전에 정한 출력 방식. 이때는 아무것도 쓰지 않는다.
 *
 * @property path 쓸 경로. 교체할 때는 symlink를 푼 실제 경로라 링크 자체는 그대로 남는다.
 * @property inPlace 지금처럼 제자리에 연다. 장치·FIFO·디렉터리·/dev 아래·대상 없는 symlink는
 *   rename으로 바꿀 대상이 아니다(root가 -o /dev/null을 주면 장치를 일반 파일로 바꾸게 된다).
 * @property permissions 교체할 기존 파일의 POSIX 권한. 새 파일이거나 POSIX가 아니면 null.
 */
internal class OutputTarget(
    val path: Path,
    val inPlace: Boolean,
    val permissions: Set<PosixFilePermission>? = null,
)

/**
 * 출력 경로의 쓰기 방식을 정한다. 기존 파일이 쓰기 불가면 제자리 쓰기가 실패하던 것처럼 예외를
 * 던져, rename이 보호된 파일을 몰래 바꾸지 않게 한다.
 */
internal fun prepareOutput(output: String): OutputTarget {
    if (output.isEmpty()) throw IOException("output path is empty; pass a file path or '-' for stdout")
    val path = Path.of(output)
    // 끝 구분자는 디렉터리를 뜻한다(Path.of가 지우므로 원문으로 본다).
    if (output.endsWith('/') || output.endsWith(File.separatorChar) ||
        path.toAbsolutePath().normalize().startsWith(DEVICE_DIRECTORY)
    ) return OutputTarget(path, inPlace = true)
    if (Files.exists(path)) return existingOutput(path)
    // 대상이 없는 symlink는 지금처럼 링크를 따라 대상 파일을 만든다.
    if (Files.isSymbolicLink(path)) return OutputTarget(path, inPlace = true)
    return OutputTarget(resolvedName(path), inPlace = false)
}

/** 일반 파일만 교체한다. 쓰기로 열어 보고 바로 닫는 것은 옛 쓰기와 같은 권한 검사이며 내용을 건드리지 않는다. */
private fun existingOutput(path: Path): OutputTarget {
    if (!Files.isRegularFile(path)) return OutputTarget(path, inPlace = true)
    val real = path.toRealPath()
    FileChannel.open(real, StandardOpenOption.WRITE).close()
    val permissions = if (supportsPosix(real)) Files.getPosixFilePermissions(real) else null
    return OutputTarget(real, inPlace = false, permissions = permissions)
}

private fun supportsPosix(path: Path): Boolean = "posix" in path.fileSystem.supportedFileAttributeViews()

/** 디렉터리만 실제 경로로 풀고 마지막 이름은 그대로 둔다. Path.parent는 ".."를 지우지 않아 OS처럼 풀린다. */
private fun resolvedName(path: Path): Path {
    val absolute = path.toAbsolutePath()
    val directory = absolute.parent ?: throw IOException("output path must name a file: $path")
    return directory.toRealPath().resolve(absolute.fileName)
}

/**
 * 출력이 SQLite 원본이나 그 -wal·-shm이면 사용자에게 보일 오류 문구를, 아니면 null을 돌려준다.
 * 같은 경로와 같은 파일(하드 링크, 대소문자를 무시하는 파일 시스템의 다른 표기)을 모두 본다.
 */
internal fun sqliteOutputAliasError(url: String, target: OutputTarget): String? {
    val database = sqliteDatabasePath(url) ?: return null
    val aliased = sqliteSourceFiles(database).any { it == target.path || sameExistingFile(it, target.path) }
    return if (aliased) SQLITE_SOURCE_ALIAS_ERROR else null
}

/**
 * sqlite-jdbc가 열 DB 파일 경로. 일반 경로는 '?' 앞까지(뒤는 sqlite-jdbc가 pragma로 뗀다)이고,
 * file: URI는 SQLite가 해석한다. 메모리·리소스 DB는 파일이 없어 null이다.
 */
internal fun sqliteDatabasePath(url: String): Path? {
    val target = sqliteAddress(url)
    val name = if (target.startsWith("file:")) sqliteUriPath(target.removePrefix("file:")) else target.substringBefore('?')
    if (name.isNullOrEmpty() || name.startsWith(":")) return null
    return Path.of(name)
}

/** SQLite URI 파일명 규칙(sqlite.org/uri.html): authority는 비거나 localhost, 경로는 %HH만 푼다. */
private fun sqliteUriPath(uri: String): String? {
    val withoutFragment = uri.substringBefore('#')
    if (withoutFragment.substringAfter('?', "").split('&').any { it == "mode=memory" }) return null
    var path = withoutFragment.substringBefore('?')
    if (path.startsWith("//")) {
        val authority = path.substring(2).substringBefore('/')
        if (authority != "" && authority != "localhost") return null
        path = path.substring(2 + authority.length)
    }
    return decodeSqlitePercent(path)
}

/** sqlite3ParseUri처럼 뒤에 16진수 두 자리가 오는 %만 바이트로 바꾸고 나머지 %는 글자 그대로 둔다. */
private fun decodeSqlitePercent(value: String): String {
    val raw = value.toByteArray(Charsets.UTF_8)
    val decoded = ByteArrayOutputStream(raw.size)
    var index = 0
    while (index < raw.size) {
        val high = raw.getOrNull(index + 1)?.let { Character.digit(it.toInt(), 16) } ?: -1
        val low = raw.getOrNull(index + 2)?.let { Character.digit(it.toInt(), 16) } ?: -1
        if (raw[index] == '%'.code.toByte() && high >= 0 && low >= 0) {
            decoded.write(high * 16 + low)
            index += 3
        } else {
            decoded.write(raw[index].toInt())
            index++
        }
    }
    return decoded.toString(Charsets.UTF_8)
}

/**
 * SQLite가 쓰는 원본 파일들. -wal·-shm은 SQLite 버전에 따라 링크 이름 옆이나 푼 이름 옆에 생기므로
 * 둘 다 넣는다. 없는 원본은 연결이 실패하므로 풀 수 있는 이름만 쓴다.
 */
private fun sqliteSourceFiles(database: Path): List<Path> {
    val absolute = database.toAbsolutePath()
    val bases = listOfNotNull(
        absolute.parent?.takeIf { Files.isDirectory(it) }?.let { resolvedName(absolute) },
        absolute.takeIf { Files.exists(it) }?.toRealPath(),
    )
    return bases.flatMap { base -> listOf(base, base.resolveSibling("${base.fileName}-wal"), base.resolveSibling("${base.fileName}-shm")) }
}

/** 두 경로가 모두 있고 같은 파일인지 본다. 없는 경로는 같은 파일이 아니다. */
private fun sameExistingFile(left: Path, right: Path): Boolean =
    Files.exists(left) && Files.exists(right) && Files.isSameFile(left, right)

/**
 * 문서를 target에 UTF-8로 쓴다. 교체할 출력은 같은 디렉터리의 임시 파일에 끝까지 쓰고 force한 뒤
 * rename한다. write가 실패하면 임시 파일을 지워 이전 파일을 그대로 두고 예외를 다시 던진다.
 * 제자리 쓰기는 지금처럼 File.bufferedWriter로 연다.
 */
internal fun writeOutput(target: OutputTarget, write: (BufferedWriter) -> Unit) {
    if (target.inPlace) {
        target.path.toFile().bufferedWriter().use(write)
        return
    }
    val (temporary, channel) = createStagingFile(target.path)
    try {
        Channels.newOutputStream(channel).bufferedWriter().use { writer ->
            write(writer)
            writer.flush()
            target.permissions?.let { Files.setPosixFilePermissions(temporary, it) }
            channel.force(true)
        }
        Files.move(temporary, target.path, StandardCopyOption.ATOMIC_MOVE)
    } catch (error: Throwable) {
        try {
            Files.deleteIfExists(temporary)
        } catch (cleanup: IOException) {
            error.addSuppressed(cleanup)
        }
        throw error
    }
}

/**
 * 대상 옆에 숨은 임시 파일을 만든다. 같은 디렉터리라 rename이 원자적이다. 권한을 지정하지 않아
 * 지금의 FileOutputStream처럼 0666&~umask로 만든다(Files.createTempFile은 0600이다).
 */
private fun createStagingFile(target: Path): Pair<Path, FileChannel> {
    var collision: FileAlreadyExistsException? = null
    repeat(100) {
        val candidate = target.resolveSibling(".${target.fileName}.tmp-${UUID.randomUUID()}")
        try {
            return candidate to FileChannel.open(candidate, StandardOpenOption.CREATE_NEW, StandardOpenOption.WRITE)
        } catch (error: FileAlreadyExistsException) {
            // 이름이 겹치면 다른 이름으로 다시 만들고, 끝내 실패하면 마지막 충돌을 원인으로 남긴다.
            collision = error
        }
    }
    throw IOException("cannot create a temporary file next to $target; remove stale .${target.fileName}.tmp-* files", collision)
}
