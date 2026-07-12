#!/usr/bin/env bash
#
# Recall 훅 + 아카이버 설치.
#
# ## 왜 복사가 아니라 심볼릭 링크인가
#
# 이전에는 `~/.claude/hooks/log_prompt.py` 가 이 레포의 사본이었고, 두 파일이
# 조용히 갈라졌다 (설치본이 레포보다 3개월 오래됐다). 훅을 고쳐도 실제로 도는 건
# 옛 코드였다는 뜻이다. 2026-04 에 프롬프트 438개가 사라진 사고의 원인도
# **"실행 중인 것이 최신 코드가 아니었다"** 였다.
#
# 심볼릭 링크는 진실의 원천을 하나로 만든다. 레포를 고치면 다음 프롬프트부터 적용된다.
# (Claude Code 는 settings.json 의 훅 *설정* 을 세션 시작 시 스냅샷하지만,
#  훅 *스크립트 파일* 은 매번 다시 읽는다.)
#
# 사용: bash hooks/install.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOKS_DIR="$HOME/.claude/hooks"
AGENT_DIR="$HOME/Library/LaunchAgents"
PLIST="$AGENT_DIR/com.recall.archive.plist"
LOG="$HOME/.claude/recall-archive/archive.log"
STAMP="$(date +%Y%m%d-%H%M%S)"

echo "레포: $REPO"
mkdir -p "$HOOKS_DIR" "$AGENT_DIR" "$(dirname "$LOG")"

# ── 1. log_prompt.py 를 심볼릭 링크로 ─────────────────────────────────────
TARGET="$HOOKS_DIR/log_prompt.py"
if [ -e "$TARGET" ] && [ ! -L "$TARGET" ]; then
  cp "$TARGET" "$TARGET.bak-$STAMP"
  echo "  기존 사본 백업 → $TARGET.bak-$STAMP"
fi
ln -sfn "$REPO/hooks/log_prompt.py" "$TARGET"
echo "  ✅ $TARGET → $(readlink "$TARGET")"

# ── 2. 스위퍼 launchd 등록 (매시간: 아카이브 + 수집) ────────────────────
# 이게 응답 수집의 **유일한** 자동 경로다. Stop 훅은 만들지 않는다 —
# 실측상 중단된 턴 9.9% 에서 발생하지 않아 어차피 스위퍼가 필요하고,
# 스위퍼가 있으면 훅이 사는 건 지연시간뿐인데 그 대가가 Claude Code 의 매 턴이다.
cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.recall.archive</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$REPO/hooks/sweep.sh</string>
  </array>
  <key>StartInterval</key><integer>3600</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>10</integer>
</dict>
</plist>
PLIST_EOF

launchctl bootout "gui/$(id -u)/com.recall.archive" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "  ✅ $PLIST (매시간, 로그: $LOG)"

echo
echo "설치 완료. 확인:"
echo "  launchctl list | grep recall"
echo "  /usr/bin/python3 $REPO/hooks/archive_transcripts.py --stats"
