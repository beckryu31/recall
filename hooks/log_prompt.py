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
import hashlib
import json
import os
import sqlite3
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()
DB_PATH = HOME / ".claude" / "prompts.db"
SPOOL_DIR = HOME / ".claude" / "recall-spool" / "prompts"
MANIFEST = HOME / ".claude" / "recall-archive" / "manifest.json"
STAMP = HOME / ".claude" / "recall-spool" / ".last-health-warn"
BIN_DIR = HOME / ".claude" / "recall-bin"

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


def drain_spool(conn, keep):
    """DB 에 못 들어간 채 스풀에 남아 있는 프롬프트를 주워 담는다.

    스풀에 쓰기만 하고 비우지 않으면 그건 write-only 무덤이다 — 프롬프트가 **보존**은 되지만
    **복구되지 않는다.** DB 쓰기가 한 번이라도 실패했다면(락·디스크 참) 그 프롬프트는
    다음 성공적인 훅 실행에서 여기로 돌아온다.

    중복 방지는 (cc_turn_id, prompt) 로 한다. INSERT 는 성공했는데 unlink 가 실패한 경우가
    있을 수 있기 때문이다. cc_turn_id 는 전역 유일하지 않으므로 prompt 텍스트와 함께 본다.
    """
    try:
        files = sorted(SPOOL_DIR.glob("*.json"))
    except OSError:
        return
    for path in files[:50]:  # 30초 타임아웃을 넘기지 않도록 한 번에 조금씩
        if path == keep:
            continue
        try:
            data = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            # 파싱 자체가 안 되는 파일은 지우지 않는다 — 사람이 볼 수 있게 남긴다.
            continue
        try:
            dup = conn.execute(
                "SELECT 1 FROM prompts WHERE cc_turn_id IS ?1 AND prompt = ?2 LIMIT 1",
                (data.get("prompt_id"), data.get("prompt", "")),
            ).fetchone()
            if dup is None:
                conn.execute(
                    """INSERT INTO prompts
                       (session_id, cwd, prompt, created_at, cc_turn_id, transcript_path)
                       VALUES (?, ?, ?, ?, ?, ?)""",
                    (
                        data.get("session_id"),
                        data.get("cwd"),
                        data.get("prompt", ""),
                        # 스풀 파일명이 time_ns 로 시작한다. 원래 시각을 최대한 살린다.
                        datetime.fromtimestamp(
                            int(path.name.split("-")[0]) / 1e9
                        ).isoformat(timespec="seconds"),
                        data.get("prompt_id"),
                        data.get("transcript_path"),
                    ),
                )
            conn.commit()
            path.unlink()
        except Exception:
            # 이번에도 실패하면 그대로 둔다. 다음 기회에 다시 시도한다.
            return


# ── ⑥ "스위퍼는 돌았는데 아무것도 안 만들었다" 를 재는 술어 ───────────────────────
#
# 기존 정체 검사(⑤)는 `MAX(ingest_state.ingested_at)` 만 본다. 그런데 수집기는
# **세그먼트를 0개 만들어도 도장을 찍는다** (`ingest.rs` 의 `ingest_sessions` 는 파일
# mtime 만 보고 기록한다). 즉 ⑤ 는 **프로세스 생존**을 재면서 문구는 "응답을 수집하지
# 못했습니다" 라고 **결과**를 주장한다. 파서가 조용히 죽으면 ⑤ 는 영원히 침묵한다 —
# 하트비트는 "살아있음" 이지 "일했음" 이 아니다.
#
# 그래서 결과를 본다. 최근 프롬프트 10개(≥3시간 된 것)가 **전부**
#   · kind 가 비어 있고 (수집이 손대면 'human' 또는 'system' 이 된다) — **그리고**
#   · 그 세션이 그 프롬프트보다 **나중에** 수집됐다
# 면, 수집기가 보고 지나갔는데 아무것도 못 만든 것이다.
#
# **접속사가 오탐을 죽인다.** "아직 안 돌았다"(도장이 프롬프트보다 이르다)로는 짖지
# 않는다 — 노트북을 며칠 닫았다 열고 바로 프롬프트를 치는 시나리오가 그것이다.
#
# 함정 둘, 둘 다 조용하다:
#   · `created_at` 은 로컬, `ingested_at` 은 UTC(`datetime('now')`) → 'localtime' 으로 맞춘다.
#   · `created_at` 은 'T' 구분자("2026-07-12T18:11:38")인데 `datetime()` 은 공백을 돌려준다.
#     감싸지 않고 문자열로 비교하면 'T'(0x54) > ' '(0x20) 이라 **같은 날짜가 통째로 미래로
#     취급**되어, "3시간 전" 창이 조용히 "어제 이전" 으로 밀린다 → 양쪽 다 `datetime()` 으로 감싼다.
BARREN_WINDOW = 10
BARREN_MIN_AGE_HOURS = 3
BARREN_SWEEP_SQL = f"""
    SELECT COUNT(*),
           SUM(CASE WHEN p.kind IS NULL
                     AND s.ingested_at IS NOT NULL
                     AND datetime(s.ingested_at, 'localtime') > datetime(p.created_at)
                    THEN 1 ELSE 0 END)
    FROM (SELECT id, session_id, created_at, kind
          FROM prompts
          WHERE datetime(created_at) <= datetime('now', 'localtime', '-{BARREN_MIN_AGE_HOURS} hours')
          ORDER BY id DESC
          LIMIT {BARREN_WINDOW}) p
    LEFT JOIN ingest_state s ON s.session_id = p.session_id
"""


def health_warning(check_db=True):
    """수집이 조용히 멈췄으면 경고 문자열을 돌려준다. 정상이면 None.

    ## 왜 훅에서 하는가

    수집기는 Claude Code 를 막지 않으려고 **무슨 일이 있어도 조용히 실패**하도록 만들어졌다.
    그런데 그건 **모든 실패가 구조적으로 조용해진다**는 뜻이고, 조용한 실패는 30~90일 뒤
    **데이터 부재**로만 드러난다 — 그때는 이미 세션 기록이 삭제돼 복구가 불가능하다.

    그래서 알람을 **사용자가 피할 수 없는 경로**에 둔다. UserPromptSubmit 훅의 stdout 은
    Claude 의 컨텍스트에 주입되므로(실증 확인함), **Claude 가 직접 사용자에게 말한다.**
    아무도 열 필요 없다고 설계한 GUI 뒤에 유일한 알람을 두지 않는다.

    ## 왜 DB 없이도 도는가

    DB 가 망가진 상황이야말로 경고가 가장 필요한 때다. 스풀 적체·디스크·아카이브 정지는
    **파일시스템만 봐도** 알 수 있고, 그게 DB 를 못 열 때 유일하게 남는 신호다.
    """
    problems = []

    # ① 스풀 적체 = 프롬프트가 DB 에 못 들어가고 있다. 가장 심각하다.
    #    (프롬프트는 응답과 달리 재생성이 불가능하다 — 이 훅이 유일한 기록자다.)
    try:
        n = len(list(SPOOL_DIR.glob("*.json")))
        if n > 0:
            problems.append(f"프롬프트 {n}개가 DB 에 기록되지 못하고 스풀에 남아 있습니다")
    except OSError:
        pass

    # ② 아카이브가 멈췄다 = launchd 스위퍼가 안 돈다.
    #    세션 기록은 cleanupPeriodDays 뒤 삭제되므로, 아카이브가 멈추면 영구 소실이 시작된다.
    try:
        age = (time.time() - MANIFEST.stat().st_mtime) / 86400
        if age >= 3:
            problems.append(
                f"세션 기록 아카이브가 {age:.0f}일째 갱신되지 않았습니다 "
                f"(launchctl list | grep recall 확인)"
            )
    except OSError:
        problems.append("세션 기록 아카이브가 아직 없습니다 (bash hooks/install.sh)")

    # ③ 디스크. 아카이브와 DB 가 자라므로 여유가 없으면 조용히 실패하기 시작한다.
    try:
        st = os.statvfs(str(HOME))
        free_gb = st.f_bavail * st.f_frsize / 1e9
        if free_gb < 5:
            problems.append(f"디스크 여유가 {free_gb:.1f}GB 뿐입니다")
    except OSError:
        pass

    # ④ 스위퍼 사본이 레포보다 오래됐다.
    #
    #   스위퍼는 launchd 가 돌리는데, macOS TCC 는 launchd 에이전트에게 ~/Documents 등의
    #   접근을 허용하지 않는다. 그래서 레포를 심링크할 수 없고 ~/.claude/recall-bin/ 으로
    #   **복사**해야 한다. 그런데 사본은 갈라진다 — 훅을 고쳐도 안 돌던 그 문제와 똑같다.
    #   여기서 감시해서, 갈라지면 Claude 가 말하게 한다.
    #   mtime 이 아니라 **내용 해시**로 본다. mtime 으로 보면 `git checkout` 이나 `git pull` 만
    #   해도 짖는데, 내용은 같다. 오탐이 잦은 알람은 곧 무시당하는 알람이다.
    try:
        repo = Path((BIN_DIR / ".repo").read_text().strip())
        for name in ("sweep.sh", "archive_transcripts.py"):
            src = hashlib.sha256((repo / "hooks" / name).read_bytes()).digest()
            dst = hashlib.sha256((BIN_DIR / name).read_bytes()).digest()
            if src != dst:
                problems.append(
                    f"스위퍼 사본이 레포와 다릅니다 ({name}) — bash hooks/install.sh 재실행"
                )
                break
    except (OSError, ValueError):
        pass

    # ⑤ 수집기가 아예 안 돈다 (**생존**을 잰다).
    # ⑥ 수집기는 도는데 아무것도 만들지 못한다 (**결과**를 잰다).
    #    DB 를 열 수 있을 때만 본다. 한 번의 연결로 둘 다 읽는다.
    if check_db:
        try:
            conn = sqlite3.connect(DB_PATH, timeout=2.0)
            try:
                row = conn.execute("SELECT MAX(ingested_at) FROM ingest_state").fetchone()
                barren = conn.execute(BARREN_SWEEP_SQL).fetchone()
            finally:
                conn.close()

            if row and row[0]:
                # ingested_at 은 datetime('now') = UTC. utcnow() 는 3.12+ 에서 deprecated 라
                # stderr 경고가 사용자 화면에 뜰 수 있다. timezone-aware 로 계산한다.
                last = datetime.fromisoformat(row[0]).replace(tzinfo=timezone.utc)
                days = (datetime.now(timezone.utc) - last).days
                if days >= 3:
                    problems.append(f"Recall 이 {days}일째 응답을 수집하지 못했습니다")

            # ⑤ 와 겹치지 않는다: 수집기가 아예 안 돌면 ⑥ 의 접속사("프롬프트보다 나중에
            # 수집됨")가 거짓이 되어 ⑥ 은 침묵한다. 두 알람이 동시에 짖는 일은 없다.
            # 창이 다 차지 않았으면(프롬프트가 10개 미만) 판단하지 않는다 — 증거 부족이다.
            if barren and barren[0] == BARREN_WINDOW and barren[1] == BARREN_WINDOW:
                problems.append(
                    f"Recall 수집기는 돌고 있지만 최근 프롬프트 {BARREN_WINDOW}개에서 "
                    "응답을 하나도 만들지 못했습니다 (파서가 고장났을 수 있습니다)"
                )
        except Exception:
            pass

    if not problems:
        return None
    return "[Recall 수집 상태 경고 — 사용자에게 알려주세요]\n" + "\n".join(
        f"- {p}" for p in problems
    )


def warn_if_unhealthy(check_db=True):
    """건강 상태를 계산해 (1) DB 에 기록하고 (2) 필요하면 stdout 으로 경고한다.

    앱도 이 상태를 보여주지만, **판정은 여기서만 한다.** 같은 로직을 Rust 에 또 짜면
    두 벌이 되고, 두 벌은 조용히 갈라진다 — 그게 프롬프트 438개를 죽인 사고의 형태다.
    훅이 쓰고 앱은 읽는다.

    stdout 경고는 한 시간에 한 번까지만. 매 프롬프트마다 짖으면 무시당한다.
    """
    msg = health_warning(check_db)

    # 앱이 읽을 수 있게 남긴다. DB 를 못 열면 그냥 넘어간다 — 그때는 stdout 이 유일한 채널이다.
    if check_db:
        try:
            conn = sqlite3.connect(DB_PATH, timeout=2.0)
            try:
                conn.execute(
                    """CREATE TABLE IF NOT EXISTS capture_health (
                           id         INTEGER PRIMARY KEY CHECK (id = 1),
                           problems   TEXT,
                           checked_at TEXT NOT NULL DEFAULT (datetime('now'))
                       )"""
                )
                conn.execute(
                    """INSERT INTO capture_health (id, problems, checked_at)
                       VALUES (1, ?1, datetime('now'))
                       ON CONFLICT(id) DO UPDATE SET
                           problems = excluded.problems, checked_at = excluded.checked_at""",
                    (msg or "",),
                )
                conn.commit()
            finally:
                conn.close()
        except Exception:
            pass

    if not msg:
        return
    try:
        if STAMP.exists() and time.time() - STAMP.stat().st_mtime < 3600:
            return
    except OSError:
        pass
    print(msg)
    try:
        STAMP.parent.mkdir(parents=True, exist_ok=True)
        STAMP.touch()
    except OSError:
        pass


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

    # ② 그 다음 DB. 실패해도 여기서 죽지 않는다 —
    #    DB 가 영구히 망가진 상황이야말로 ④의 경고가 가장 필요한 때인데,
    #    여기서 예외를 올려버리면 그 경고에 영영 도달하지 못한다.
    ok = False
    try:
        data = json.loads(raw)
        conn = sqlite3.connect(DB_PATH, timeout=BUSY_TIMEOUT_SEC)
        try:
            init_db(conn)
            conn.execute(
                """INSERT INTO prompts
                   (session_id, cwd, prompt, created_at, cc_turn_id, transcript_path)
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
            ok = True
            # ③ DB 에 안전하게 들어갔을 때만 스풀을 지운다.
            if spool_path is not None:
                try:
                    spool_path.unlink()
                except OSError:
                    pass
            # 예전에 못 들어간 프롬프트를 주워 담는다. 스풀은 무덤이 아니라 대기실이다.
            try:
                drain_spool(conn, spool_path)
            except Exception:
                pass
        finally:
            conn.close()
    except Exception:
        pass  # payload 는 스풀에 있다. 아무것도 잃지 않았다.

    # ④ 건강 경고. **DB 가 없어도 동작해야 한다** — 스풀 적체·디스크·아카이브 정지는
    #    파일시스템만 봐도 알 수 있고, 그게 DB 가 망가졌을 때 유일하게 남는 신호다.
    try:
        warn_if_unhealthy(check_db=ok)
    except Exception:
        pass


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # 프롬프트 기록기가 프롬프트 입력을 막아서는 안 된다. 어떤 예외도 삼킨다.
        # 잃은 것은 없다 — payload 는 이미 스풀에 있다.
        pass
    sys.exit(0)
