import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.DriverManager;

/** ApplySqlStatements의 @ 구분자가 문자열·주석 속 @를 보존하는지 H2로 확인한다. */
public final class TestApplySqlStatements {
    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.err.println("usage: java TestApplySqlStatements.java <h2-jdbc-url>");
            System.exit(2);
        }
        Path sql = Files.createTempFile("schemagraph-apply-sql-", ".sql");
        try {
            Files.writeString(sql, """
                    --#SET TERMINATOR @
                    CREATE TABLE apply_check(text_value VARCHAR(64))@
                    -- comment contains @ and must not split
                    INSERT INTO apply_check VALUES ('quoted @ marker')@
                    /* block comment contains @ too */
                    INSERT INTO apply_check VALUES ('second')@
                    """);
            ApplySqlStatements.main(new String[]{args[0], "sa", "", sql.toString()});
            try (var conn = DriverManager.getConnection(args[0], "sa", "");
                 var query = conn.prepareStatement("SELECT text_value FROM apply_check ORDER BY text_value");
                 var rows = query.executeQuery()) {
                if (!rows.next() || !"quoted @ marker".equals(rows.getString(1)) ||
                        !rows.next() || !"second".equals(rows.getString(1)) || rows.next()) {
                    throw new AssertionError("@ splitter changed quoted/commented SQL");
                }
            }
        } finally {
            Files.deleteIfExists(sql);
        }
    }
}
