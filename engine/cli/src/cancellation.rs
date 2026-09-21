//! 동기식 탐색이 async 런타임을 점유해도 Ctrl+C를 관측할 수 있게 한다.

use anyhow::{Context, Result};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// 한 번 실행되는 CLI 탐색에만 핸들러를 설치해 다른 명령의 기본 신호 처리를 보존한다.
pub(crate) fn install() -> Result<&'static AtomicBool> {
    // 백그라운드 셸이 물려준 SIG_IGN도 정상 질의를 막지 않도록 이 CLI가
    // 사용하는 신호 처리를 설치한다. 프로세스당 하나의 명령에서만 호출한다.
    ctrlc::set_handler(|| {
        if REQUESTED.swap(true, Ordering::Relaxed) {
            // 두 번째 요청은 로딩·출력처럼 협력적으로 멈출 수 없는 단계도 끝낸다.
            std::process::exit(130);
        }
        // 안내 출력 실패로 신호 처리 스레드가 panic하면 다음 Ctrl+C도 처리하지
        // 못한다. stderr가 끊긴 경우에는 명시적인 취소 종료 코드로 끝낸다.
        if writeln!(
            std::io::stderr(),
            "Cancellation requested; press Ctrl+C again to exit immediately."
        )
        .is_err()
        {
            std::process::exit(130);
        }
    })
    .context("could not install Ctrl+C handling; check process signal support and retry")?;
    Ok(&REQUESTED)
}

/// 출력·입력 오류와 취소가 경합해도 호출자가 중단을 정상 완료로 오독하지 않게 한다.
pub(crate) fn exit_code(normal: i32) -> i32 {
    if REQUESTED.load(Ordering::Relaxed) {
        130
    } else {
        normal
    }
}
