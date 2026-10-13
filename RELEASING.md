# 릴리스 게시

버전·PR 검증을 끝낸 정확한 최종 병합 커밋에서 `CI`와 관련 fixture workflow의 성공을
확인한 뒤 새 `vX.Y.Z` 태그를 만든다. 자세한 사전 관문은 [HANDOFF.md](HANDOFF.md)를 따른다.
`Release binaries`는 기존 Cargo·Gradle·태그 버전 대조, Linux/macOS 바이너리와 JDBC JAR
빌드·검증, 패키징·체크섬·`RELEASE-METADATA.txt` 대조를 모두 통과한 뒤에만 게시한다.
PR·수동 실행은 계속 `publish=false`이며, crate·Maven·Pages 게시는 별도 절차다.

공개 자산은 네 개다: `schemagraph-VERSION-x86_64-unknown-linux-gnu.tar.gz`,
`schemagraph-VERSION-aarch64-apple-darwin.tar.gz`, `schemagraph-probe-VERSION-all.jar`,
`SHA256SUMS`. `RELEASE-METADATA.txt`는 검증 번들에만 남고 공개 자산에 추가하지 않는다.
게시 helper는 검증된 번들 디렉터리에서 실행하며 대상은 `ictechgy/schemagraph`로 고정한다.

GitHub UI에서 릴리스를 먼저 만드는 경우에는 검증한 커밋을 선택하고, 노트에 바이너리 게시가
아직 진행 중임을 적는다. 릴리스 페이지가 생겼다는 이유만으로 배포 완료로 보지 않는다.
helper는 태그를 커밋까지 풀어 빌드 SHA와 대조하며, 일반 공개 릴리스
(`draft=false`, `prerelease=false`)의 누락 자산만 채운다. 제목·본문·표시는 바꾸지 않는다.
기존 예상 자산은 `uploaded` 상태·크기·SHA-256이 검증된 파일과 모두 같아야 재사용한다.
네 자산의 전체 목록을 먼저 검사하므로 충돌이 있으면 다른 파일도 올리지 않는다.
다른 이름의 사용자 자산은 그대로 둔다. API 오류·누락 digest·알 수 없는 immutability는 실패다.

기존 파일의 `--clobber`·삭제, 태그 이동·생성, 릴리스 설정 변경은 하지 않는다.
실패하거나 결과가 불명확한 생성·업로드는 원격 상태를 다시 대조하고, 누락이 남으면 실패한다.
같은 검증 산출물로만 재시도한다. 다시 빌드한 압축 파일은 시간 정보 등으로 해시가 달라질 수 있어
이름만 같은 파일을 성공으로 처리하지 않는다. 태그 확인과 업로드는 원자적이지 않다.
업로드 뒤 태그 이동을 발견하면 실패하며 이미 추가한 자산을 지우지 않으므로, 재시도 전에 실행을 확인한다.

immutable 공개 릴리스가 불완전하면 helper는 설정을 바꾸지 않고 실패한다. 완전하고 같은 바이트이면
읽기 전용 재실행이 성공한다. 릴리스가 없으면 기존 `gh release create --verify-tag` 경로로
`schemagraph VERSION` 제목과 `--generate-notes`를 사용한다. gh가 새 draft에 자산을 붙인 뒤
공개하고, 업로드 실패 시 그 draft를 정리하는 기존 동작은 유지한다. immutable 정책에서는
이렇게 자산 첨부를 마친 뒤 공개해야 한다.

최종 성공은 태그가 빌드 SHA를 가리키고 네 자산이 모두 검증된 바이트일 때다.
공개 다운로드·체크섬·설치 소비자 확인을 마친 뒤 준비 중인 노트를 갱신한다.
오프라인 회귀 검사: `bash Scripts/test-publish-release-assets.sh`. 모든 요청은 가짜 `gh`가 처리하며,
자격 증명·네트워크 없이 Ubuntu/macOS CI에서 같은 36개 성공·거절·불확실 쓰기 시나리오를 확인한다.
