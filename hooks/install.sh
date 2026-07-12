#!/usr/bin/env bash
#
# Recall 훅 + 스위퍼 설치.
#
# ## 왜 훅은 심링크이고 스위퍼는 복사인가
#
# **훅**은 Claude Code 프로세스가 실행하므로 레포를 읽을 수 있다. 심볼릭 링크로 걸어
# 진실의 원천을 하나로 만든다 — 사본은 갈라진다. 실제로 설치본이 레포보다 3개월 오래됐던
# 적이 있고, "실행 중인 것이 최신 코드가 아니다" 는 프롬프트 438개를 죽인 사고의 원인이다.
#
# **스위퍼**는 launchd 가 실행하는데, **macOS TCC 는 launchd 에이전트에게
# ~/Documents · ~/Desktop · ~/Downloads · iCloud Drive 접근을 허용하지 않는다.**
# 레포가 그 안에 있으면 에이전트는 스크립트를 열지도 못하고 매번
# `Operation not permitted` 로 죽는다 — **조용히.** (launchctl bootstrap 은 성공한다.)
# 그래서 스위퍼가 쓰는 것들은 TCC 가 막지 않는 ~/.claude/recall-bin/ 으로 **복사**한다.
#
# 사본이 갈라지는 문제는 훅의 건강 검사가 잡는다: 레포가 사본보다 새로우면 경고한다.
#
# 사용: bash hooks/install.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOKS_DIR="$HOME/.claude/hooks"
BIN_DIR="$HOME/.claude/recall-bin"
AGENT_DIR="$HOME/Library/LaunchAgents"
PLIST="$AGENT_DIR/com.recall.archive.plist"
LOG="$HOME/.claude/recall-archive/archive.log"
STAMP="$(date +%Y%m%d-%H%M%S)"
INGEST_SRC="$REPO/src-tauri/target/release/recall-ingest"

echo "레포: $REPO"
mkdir -p "$HOOKS_DIR" "$BIN_DIR" "$AGENT_DIR" "$(dirname "$LOG")"

# ── 0. 수집기가 빌드돼 있는가 ────────────────────────────────────────────
# 스위퍼는 이 바이너리를 실행한다. 없으면 아카이브만 하고 응답은 하나도 수집하지 않는데,
# 그 사실이 아무도 읽지 않는 로그에만 남는다. 여기서 막는다.
if [ ! -x "$INGEST_SRC" ]; then
  echo
  echo "❌ 수집기가 없습니다: $INGEST_SRC"
  echo
  echo "   스위퍼는 이 바이너리로 응답을 수집합니다. 먼저 빌드하세요:"
  echo "     cargo build --release --bin recall-ingest --manifest-path src-tauri/Cargo.toml"
  echo
  exit 1
fi

# ── 1. log_prompt.py 를 심볼릭 링크로 ─────────────────────────────────────
TARGET="$HOOKS_DIR/log_prompt.py"
if [ -e "$TARGET" ] && [ ! -L "$TARGET" ]; then
  cp "$TARGET" "$TARGET.bak-$STAMP"
  echo "  기존 사본 백업 → $TARGET.bak-$STAMP"
fi
ln -sfn "$REPO/hooks/log_prompt.py" "$TARGET"
echo "  ✅ 훅(심링크): $TARGET"

# ── 2. 스위퍼 자산을 TCC 가 막지 않는 곳으로 복사 ────────────────────────
cp "$REPO/hooks/archive_transcripts.py" "$BIN_DIR/"
cp "$REPO/hooks/sweep.sh"               "$BIN_DIR/"
cp "$INGEST_SRC"                        "$BIN_DIR/recall-ingest"
chmod +x "$BIN_DIR/sweep.sh" "$BIN_DIR/recall-ingest"
# 훅의 건강 검사가 사본이 갈라졌는지 볼 수 있도록 레포 경로를 남긴다.
printf '%s\n' "$REPO" > "$BIN_DIR/.repo"
echo "  ✅ 스위퍼(사본): $BIN_DIR"

# ── 3. launchd 등록 (매시간) ─────────────────────────────────────────────
cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.recall.archive</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$BIN_DIR/sweep.sh</string>
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

# ── 4. 등록이 아니라 **실행**을 확인한다 ─────────────────────────────────
# bootstrap 은 스크립트를 열 수 없어도 성공한다. "등록됐다"는 "돌았다"가 아니다.
# 이 검사가 없어서, TCC 에 막혀 한 번도 실행되지 않은 에이전트를 두고 ✅ 를 찍고 있었다.
echo -n "  에이전트 실행 확인"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  sleep 1; echo -n "."
  STATUS="$(launchctl list | awk '$3=="com.recall.archive" {print $2}')"
  [ "${STATUS:-}" = "0" ] && break
done
echo
if [ "${STATUS:-}" != "0" ]; then
  echo
  echo "❌ launchd 에이전트가 실행되지 못했습니다 (종료코드: ${STATUS:-?})"
  echo "   로그: $LOG"
  tail -3 "$LOG" 2>/dev/null | sed 's/^/     /'
  echo
  echo "   126 이나 'Operation not permitted' 라면 macOS TCC 문제입니다."
  echo "   ~/.claude/recall-bin 에 접근 권한이 없다는 뜻이니 이슈로 알려주세요."
  exit 1
fi
echo "  ✅ launchd: com.recall.archive (매시간, 종료코드 0)"
echo "     로그: $LOG"

echo
echo "설치 완료. 다음 단계 — ~/.claude/settings.json 에 훅을 등록하세요:"
cat <<'JSON'
  { "hooks": { "UserPromptSubmit": [
      { "hooks": [ { "type": "command",
                     "command": "python3 $HOME/.claude/hooks/log_prompt.py" } ] } ] } }
JSON
echo
echo "  ⚠️ 기존 hooks 설정이 있다면 통째로 덮어쓰지 말고 UserPromptSubmit 항목만 추가하세요."
echo
echo "레포를 옮기거나 코드를 고친 뒤에는 이 스크립트를 다시 실행하세요 (스위퍼는 사본입니다)."
