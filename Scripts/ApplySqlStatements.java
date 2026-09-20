import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.DriverManager;
import java.util.ArrayList;
import java.util.List;

/** 폐기용 IBM fixture를 JDBC로 적용한다. DB2/Informix의 루틴 내부 세미콜론을
 * 보존하기 위해 fixture가 지정한 `--#SET TERMINATOR @` 구분자를 해석한다. */
public final class ApplySqlStatements {

    public static void main(String[] args) throws Exception {
        if (args.length != 4) {
            System.err.println("usage: java ApplySqlStatements.java <jdbc-url> <user> <pass> <sql-file>");
            System.exit(2);
        }
        List<String> statements = split(Files.readString(Path.of(args[3])));
        try (var conn = DriverManager.getConnection(args[0], args[1], args[2])) {
            int failed = 0;
            for (String statement : statements) {
                try (var st = conn.createStatement()) {
                    st.execute(statement);
                } catch (Exception error) {
                    failed++;
                    String preview = statement.replaceAll("\\s+", " ");
                    System.err.println("FAIL: " + error.getMessage() + " :: "
                            + preview.substring(0, Math.min(100, preview.length())));
                }
            }
            if (failed > 0) {
                System.err.println("apply-sql-statements: " + failed + " statement(s) failed");
                System.exit(1);
            }
        }
    }

    private static List<String> split(String input) {
        char delimiter = detectDelimiter(input);
        StringBuilder current = new StringBuilder();
        List<String> statements = new ArrayList<>();
        boolean single = false;
        boolean doubleQuote = false;
        boolean lineComment = false;
        boolean blockComment = false;
        for (int i = 0; i < input.length(); i++) {
            char c = input.charAt(i);
            char next = i + 1 < input.length() ? input.charAt(i + 1) : 0;
            if (lineComment) {
                current.append(c);
                if (c == '\n') lineComment = false;
                continue;
            }
            if (blockComment) {
                current.append(c);
                if (c == '*' && next == '/') {
                    current.append(next);
                    i++;
                    blockComment = false;
                }
                continue;
            }
            if (!single && !doubleQuote && c == '-' && next == '-') {
                current.append(c).append(next);
                i++;
                lineComment = true;
                continue;
            }
            if (!single && !doubleQuote && c == '/' && next == '*') {
                current.append(c).append(next);
                i++;
                blockComment = true;
                continue;
            }
            if (c == '\'' && !doubleQuote) {
                current.append(c);
                if (single && next == '\'') {
                    current.append(next);
                    i++;
                } else {
                    single = !single;
                }
                continue;
            }
            if (c == '"' && !single) {
                current.append(c);
                if (doubleQuote && next == '"') {
                    current.append(next);
                    i++;
                } else {
                    doubleQuote = !doubleQuote;
                }
                continue;
            }
            if (c == delimiter && !single && !doubleQuote) {
                String statement = stripDirective(current.toString()).trim();
                if (!statement.isEmpty()) statements.add(statement);
                current.setLength(0);
                continue;
            }
            current.append(c);
        }
        String finalStatement = stripDirective(current.toString()).trim();
        if (!finalStatement.isEmpty()) statements.add(finalStatement);
        return statements;
    }

    private static char detectDelimiter(String input) {
        for (String line : input.split("\\R")) {
            String trimmed = line.trim();
            String prefix = "--#SET TERMINATOR ";
            if (trimmed.startsWith(prefix) && trimmed.length() > prefix.length()) {
                return trimmed.charAt(prefix.length());
            }
        }
        return ';';
    }

    private static String stripDirective(String text) {
        String trimmed = text.stripLeading();
        if (!trimmed.startsWith("--#SET TERMINATOR ")) return text;
        int newline = trimmed.indexOf('\n');
        return newline < 0 ? "" : trimmed.substring(newline + 1);
    }
}
