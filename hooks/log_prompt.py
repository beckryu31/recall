#!/usr/bin/env python3
"""Claude Code UserPromptSubmit hook: 제출된 프롬프트를 ~/.claude/prompts.db 에 기록한다.

~/.claude/settings.json 에 등록되어, 프롬프트 제출 시마다 Claude Code 가 stdin 으로
JSON payload 를 넘겨준다.

## 왜 스풀을 먼저 쓰는가

이 훅이 실패하면 **프롬프트 행이 유실되고, 디스크 어디에도 복구 소스가 없다.**
응답은 세션 기록(transcript)에서 다시 뽑을 수 있지만, 프롬프트 자체는 이 훅이
유일한 기록자다. 그래서 순서가 전부다:

  1. payload 원본을 스풀 파일에 **먼저** 쓴다 (배타 생성 + 단일 쓰기 — 경합으로 실패할 수 없다)
  2. **그 다음** DB INSERT 를 시도한다
  3. INSERT 가 **성공했을 때만** 스풀 파일을 지운다

DB 가 잠겨 있든 깨져 있든 프롬프트는 스풀에 남는다. 나중에 주워 담을 수 있다.

## 왜 무슨 일이 있어도 exit 0 인가

이 훅이 0 이 아닌 코드로 죽으면 사용자 프롬프트 밑에 파이썬 트레이스백이 뜬다.
프롬프트 기록기가 프롬프트 입력을 방해하는 건 어떤 이유로도 정당화되지 않는다.
(이전 버전은 이걸 docstring 으로 약속만 하고 json.JSONDecodeError 만 잡았다.)
"""
import json
import os
import sqlite3
import sys
import time
import uuid
from datetime import datetime
from pathlib import Path

HOME = Path.home()
DB_PATH = HOME / ".claude" / "prompts.db"
SPOOL_DIR = HOME / ".claude" / "recall-spool" / "prompts"

# DB 가 잠겨 있으면 기다린다. 앱 쪽(3초)보다 길게 잡은 것은 의도적이다 —
# 앱 쿼리가 실패하면 사용자가 다시 누르면 그만이지만, 훅이 실패하면 행이 사라진다.
BUSY_TIMEOUT_SEC = 5.0


def write_spool(raw: str):
    """DB 를 건드리기 전에 payload 원본을 남긴다. 실패해도 조용히 None."""
    try:
        SPOOL_DIR.mkdir(parents=True, exist_ok=True)
        path = SPOOL_DIR / f"{time.time_ns()}-{uuid.uuid4().hex[:8]}.json"
        with open(path, "x") as f:  # 배타 생성 — 덮어쓸 수 없다
            f.write(raw)
            f.flush()
            os.fsync(f.fileno())
        return path
    except Exception:
        return None


def init_db(conn):
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS prompts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT,
            cwd          TEXT,
            prompt       TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            response     TEXT
        )
        """
    )
    conn.execute("CREATE INDEX IF NOT EXISTS idx_prompts_session ON prompts(session_id)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_prompts_created ON prompts(created_at)")

    # cc_turn_id: Claude Code 훅 payload 의 prompt_id.
    #   transcript user 이벤트의 promptId 와 **같은 값**이다 (둘 다 내부적으로 같은 변수를 읽는다).
    #   이걸 저장해 두면 "생성 시각 ±30초 + 텍스트 앞 24자 비교" 라는 지금의 휴리스틱 매칭을
    #   결정론적 키 조인으로 대체할 수 있다.
    #   이름이 prompt_id 가 아닌 이유: prompt_id 는 이미 prompt_bookmarks/prompt_tags/
    #   prompt_responses 에서 INTEGER row id 를 뜻한다. 섞으면 조용한 오조인이 된다.
    #
    #   ⚠️ promptId 는 **턴 ID 이지 프롬프트 ID 가 아니다.** 전역 유일하지 않다
    #   (서브에이전트 transcript 가 부모의 promptId 를 갖고, queued 프롬프트가 공유할 수 있다).
    #   절대 UNIQUE 인덱스를 걸지 말 것 — 진짜 프롬프트가 조용히 사라진다.
    #
    # transcript_path: payload 가 알려주는 정확한 세션 파일 경로.
    #   지금은 cwd 를 폴더명으로 인코딩해 추측하는데, 세션 중 cd 하면 어긋난다.
    for ddl in (
        "ALTER TABLE prompts ADD COLUMN cc_turn_id TEXT",
        "ALTER TABLE prompts ADD COLUMN transcript_path TEXT",
    ):
        try:
            conn.execute(ddl)
        except sqlite3.OperationalError:
            pass  # 이미 있음


def main():
    raw = sys.stdin.read()

    # ① 무엇보다 먼저 원본을 남긴다.
    spool_path = write_spool(raw)

    # ② 그 다음 DB.
    data = json.loads(raw)
    conn = sqlite3.connect(DB_PATH, timeout=BUSY_TIMEOUT_SEC)
    try:
        init_db(conn)
        conn.execute(
            """INSERT INTO prompts (session_id, cwd, prompt, created_at, cc_turn_id, transcript_path)
               VALUES (?, ?, ?, ?, ?, ?)""",
            (
                data.get("session_id"),
                data.get("cwd"),
                data.get("prompt", ""),
                datetime.now().isoformat(timespec="seconds"),
                data.get("prompt_id"),
                data.get("transcript_path"),
            ),
        )
        conn.commit()
    finally:
        conn.close()

    # ③ DB 에 안전하게 들어갔을 때만 스풀을 지운다.
    if spool_path is not None:
        try:
            spool_path.unlink()
        except OSError:
            pass


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # 프롬프트 기록기가 프롬프트 입력을 막아서는 안 된다. 어떤 예외도 삼킨다.
        # 잃은 것은 없다 — payload 는 이미 스풀에 있다.
        pass
    sys.exit(0)
