import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.util.Locale;

/** 원본 SQL의 유효성과 DB 카탈로그 근거를 분석기 없이 확인하기 위한 JDBC 도구다. */
public final class AccuracySql {
    /** 접속 비밀번호는 인자·보고서에 넣지 않고 이 자식 프로세스의 환경에서만 받는다. */
    public static void main(String[] args) {
        if (args.length != 4) {
            System.err.println("usage: AccuracySql JDBC_URL USER exec|query SQL_FILE");
            System.exit(2);
        }
        String password = System.getenv("SG_ACCURACY_JDBC_PASSWORD");
        try (var connection = DriverManager.getConnection(args[0], args[1], password);
             var statement = connection.createStatement()) {
            statement.setQueryTimeout(30);
            String sql = Files.readString(Path.of(args[3]));
            if (args[2].equals("query")) {
                try (var result = statement.executeQuery(sql)) {
                    printRows(result);
                }
            } else if (args[2].equals("exec")) {
                // Python이 만든 명시적 경계만 사용하며 PL/SQL 내부 세미콜론은 보존한다.
                for (String batch : sql.split("(?m)^-- SG-ACCURACY-BATCH\\s*$")) {
                    if (!batch.isBlank()) statement.execute(batch.trim());
                }
            } else {
                throw new IllegalArgumentException("mode must be exec or query");
            }
        } catch (Exception error) {
            String message = error.getMessage() == null ? error.getClass().getSimpleName() : error.getMessage();
            if (password != null && !password.isEmpty()) message = message.replace(password, "<redacted>");
            String code = error instanceof SQLException ? ((SQLException) error).getSQLState() : "tool";
            System.err.println("accuracy-sql [" + code + "]: " + message);
            System.exit(1);
        }
    }

    /** DB별 JSON 집계 함수 차이를 피하고 동일한 행 표현으로 비교한다. */
    private static void printRows(ResultSet rows) throws SQLException {
        var metadata = rows.getMetaData();
        System.out.print("[");
        boolean first = true;
        while (rows.next()) {
            if (!first) System.out.print(",");
            first = false;
            System.out.print("{");
            for (int column = 1; column <= metadata.getColumnCount(); column++) {
                if (column > 1) System.out.print(",");
                System.out.print(quote(metadata.getColumnLabel(column).toLowerCase(Locale.ROOT)));
                System.out.print(":");
                System.out.print(quote(rows.getString(column)));
            }
            System.out.print("}");
        }
        System.out.println("]");
    }

    /** 카탈로그 문자열의 제어 문자도 JSON 경계 밖으로 나오지 않게 한다. */
    private static String quote(String value) {
        if (value == null) return "null";
        StringBuilder output = new StringBuilder("\"");
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            switch (character) {
                case '"': output.append("\\\""); break;
                case '\\': output.append("\\\\"); break;
                case '\n': output.append("\\n"); break;
                case '\r': output.append("\\r"); break;
                case '\t': output.append("\\t"); break;
                default:
                    if (character < 0x20) output.append(String.format("\\u%04x", (int) character));
                    else output.append(character);
            }
        }
        return output.append('"').toString();
    }
}
