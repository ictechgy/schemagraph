import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.DriverManager;

/**
 * GO 배치 구분자가 들어간 SQL fixture를 JDBC로 적용하는 검증 도구.
 * sqlcmd가 없는 컨테이너 이미지(Azure SQL Edge)에서 fixture를 싣기 위해 쓴다.
 * 단일 파일 소스 실행: `java Scripts/ApplySql.java <jdbc-url> <user> <pass> <sql-file>`
 *
 * 검증 전용이라 트랜잭션·롤백은 다루지 않는다 — 대상은 폐기용 DB라는 계약이다.
 */
public final class ApplySql {

    public static void main(String[] args) throws Exception {
        if (args.length != 4) {
            System.err.println("usage: java ApplySql.java <jdbc-url> <user> <pass> <sql-file>");
            System.exit(2);
        }
        String sql = Files.readString(Path.of(args[3]));
        try (var conn = DriverManager.getConnection(args[0], args[1], args[2]);
             var st = conn.createStatement()) {
            int failed = 0;
            // 행 단독 GO(T-SQL)나 /(Oracle sqlplus 관례)만 구분자로 본다 —
            // 문자열 리터럴 안의 동명 토큰은 건드리지 않는다.
            for (String batch : sql.split("(?im)^(?:GO|/)\\s*$")) {
                String b = batch.trim();
                if (b.isEmpty()) continue;
                try {
                    st.execute(b);
                } catch (Exception e) {
                    failed++;
                    System.err.println("FAIL: " + e.getMessage() + " :: "
                            + b.substring(0, Math.min(80, b.length())).replace('\n', ' '));
                }
            }
            if (failed > 0) {
                System.err.println("apply-sql: " + failed + "개 배치 실패");
                System.exit(1);
            }
        }
    }
}
