#!/usr/bin/env bash
#
# 매시간 launchd 가 돌리는 스위퍼. 두 가지를 한다:
#
#   1. 세션 기록 원본을 아카이브로 복사 (보존 시계 밖으로 빼낸다)
#   2. 응답을 세그먼트로 수집
#
# 이 순서가 중요하다. 먼저 원본을 안전하게 만들면, 수집기가 틀려도 다시 돌리면 된다.
#
# Claude Code 에는 아무 지연도 추가하지 않는다 — Stop 훅이 없다.
# (실측: Stop 은 중단된 턴 9.9%에서 발생하지 않아 어차피 스위퍼가 필요하고,
#  스위퍼가 있으면 훅이 사는 건 지연시간뿐이며, 391MB 를 몇 초에 훑는다.)
# ⚠️ 이 스크립트는 레포가 아니라 ~/.claude/recall-bin/ 에서 돈다.
#
# macOS TCC 는 launchd 에이전트에게 ~/Documents · ~/Desktop · ~/Downloads · iCloud Drive
# 접근을 허용하지 않는다. 레포가 그 안에 있으면 에이전트는 이 스크립트를 열지도 못하고
# 매번 `Operation not permitted` 로 죽는다 — 그리고 `launchctl bootstrap` 은 성공한다.
# 그래서 install.sh 가 필요한 것들을 ~/.claude/recall-bin/ 으로 복사한다.
# 레포 코드를 고쳤으면 install.sh 를 다시 돌려야 반영된다.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INGEST="$HERE/recall-ingest"
ARCHIVER="$HERE/archive_transcripts.py"

echo "── $(date '+%Y-%m-%d %H:%M:%S') ──"

if [ -f "$ARCHIVER" ]; then
  /usr/bin/python3 "$ARCHIVER" || echo "  ! 아카이브 실패"
else
  echo "  ! 아카이버가 없습니다: $ARCHIVER  (bash hooks/install.sh 재실행)"
fi

if [ -x "$INGEST" ]; then
  "$INGEST" || echo "  ! 수집 실패"
else
  # 조용히 넘기지 않는다. 수집이 멈춘 것을 90일 뒤 데이터 부재로 알게 되면 이미 늦었다.
  echo "  ! 수집기가 없습니다: $INGEST  (bash hooks/install.sh 재실행)"
fi
