#!/usr/bin/env python3
"""훅의 회귀 테스트. 의존성 없음 — `python3 hooks/test_hook.py`.

두 가지를 못 박는다:

  A. **기록 규칙** — 무엇을 프롬프트로 적고 무엇을 적지 않는가.
     훅은 프롬프트의 **유일한 기록자**다. 잘못 적으면 지울 수 없는 껍데기가 남고,
     잘못 안 적으면 그 프롬프트는 지구상에서 사라진다. 양쪽 다 본다.

  B. **수집 알람** — 짖어야 할 때 짖고, 짖지 말아야 할 때 침묵하는가.
     알람은 **짖지 않는 것으로는 검증되지 않는다.** 붙여 놓고 한 번도 짖지 않은 알람과,
     고장 나서 영원히 침묵하는 알람은 겉보기에 똑같다.

## fixture 규칙

**절대 시각을 박지 않는다.** 알람의 창("최근 10개 중 ≥3시간 된 것")은 쿼리 시각에
계산되므로, 하드코딩한 날짜는 시간이 흐르면 의미가 바뀐다 — 작성 시점엔 "3시간 전" 이던
값이 내일은 "27시간 전" 이 된다. 전부 지금 기준 상대 시간으로 만든다.

`created_at` 은 훅이 `datetime.isoformat()` 으로 쓰므로 **로컬 시간 + 'T' 구분자**,
`ingest_state.ingested_at` 은 SQL `datetime('now')` = **UTC + 공백 구분자**다.
이 비대칭이 알람 술어의 유일한 함정이므로, fixture 도 **양쪽을 실제와 똑같이** 만든다.
"""
import contextlib
import importlib.util
import io
import json
import sqlite3
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
BARREN = "응답을 하나도 만들지 못했습니다"

spec = importlib.util.spec_from_file_location("log_prompt", HERE / "log_prompt.py")
lp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lp)


# ── A. 기록 규칙 ──────────────────────────────────────────────────────────────


def run_hook(raw, workdir):
    """훅을 payload 하나로 돌리고 (DB 행 수, 스풀 파일 수) 를 돌려준다."""
    db = workdir / "p.db"
    spool = workdir / "spool"
    lp.DB_PATH = str(db)
    lp.SPOOL_DIR = spool

    stdin, sys.stdin = sys.stdin, io.StringIO(raw)
    try:
        with contextlib.redirect_stdout(io.StringIO()):  # 건강 경고 출력은 삼킨다
            lp.main()
    finally:
        sys.stdin = stdin

    rows = 0
    if db.exists():
        conn = sqlite3.connect(db)
        try:
            rows = conn.execute("SELECT COUNT(*) FROM prompts").fetchone()[0]
        except sqlite3.OperationalError:
            rows = 0  # 테이블조차 안 생겼다 = 아무것도 안 적었다
        finally:
            conn.close()
    spooled = len(list(spool.glob("*.json"))) if spool.exists() else 0
    return rows, spooled


RECORD_CASES = [
    # (이름, payload(raw), 기대 DB 행, 기대 스풀 파일)
    (
        "정상 프롬프트는 적는다",
        json.dumps({"prompt": "안녕", "session_id": "s", "cwd": "/c"}),
        1,
        0,
    ),
    (
        "빈 문자열은 적지 않는다 — 껍데기 행이 목록에 영원히 남는다",
        json.dumps({"prompt": "", "session_id": "s", "cwd": "/c"}),
        0,
        0,
    ),
    (
        "prompt 키가 없으면 적지 않는다 (실제로 이렇게 껍데기 행이 생겼다)",
        json.dumps({}),
        0,
        0,
    ),
    (
        "공백뿐이어도 적지 않는다",
        json.dumps({"prompt": "   \n\t "}),
        0,
        0,
    ),
    (
        "**파싱 불가는 버리지 않는다** — 판단 불가는 삭제의 근거가 아니다. 스풀에 남긴다",
        "이건 JSON 이 아니다",
        0,
        1,
    ),
    (
        "모르는 모양(prompt 가 문자열이 아님)도 버리지 않는다 — 스풀에 남긴다",
        json.dumps({"prompt": {"blocks": ["x"]}}),
        0,
        1,
    ),
]


def check_recording():
    ok = True
    for name, raw, want_rows, want_spool in RECORD_CASES:
        with tempfile.TemporaryDirectory() as d:
            rows, spooled = run_hook(raw, Path(d))
        hit = (rows, spooled) == (want_rows, want_spool)
        ok &= hit
        print(
            f"{'✅' if hit else '❌'} DB={rows} 스풀={spooled} "
            f"(기대 DB={want_rows} 스풀={want_spool})  {name}"
        )
    return ok


def check_drain_skips_empty():
    """스풀에 이미 있던 빈 프롬프트는 삽입하지 않고 **지운다**.

    지우지 않으면 영원히 드레인되지 않는 파일이 되어, 검사 ①("스풀에 남아 있습니다")이
    오탐으로 짖는다 — 알람을 죽이는 가장 빠른 길이다. 버리는 것은 본문 없는 껍데기뿐이다.
    """
    with tempfile.TemporaryDirectory() as d:
        work = Path(d)
        spool = work / "spool"
        spool.mkdir()
        # 이 규칙 이전에 만들어진 스풀 파일 둘. 파일명은 time_ns- 로 시작해야 한다
        # (drain 이 파일명에서 원래 시각을 복원한다).
        stamp = int(datetime.now().timestamp() * 1e9)
        (spool / f"{stamp}-aaaaaaaa.json").write_text(json.dumps({"prompt": ""}))
        (spool / f"{stamp + 1}-bbbbbbbb.json").write_text(
            json.dumps({"prompt": "스풀에 남아 있던 진짜 프롬프트"})
        )

        rows, spooled = run_hook(json.dumps({"prompt": "새 프롬프트"}), work)

    # 새 프롬프트 1 + 스풀에 있던 진짜 프롬프트 1 = 2. 빈 것은 안 들어간다.
    # 스풀은 비어야 한다 — 빈 것은 지워지고, 진짜는 DB 로 들어가며 지워진다.
    hit = (rows, spooled) == (2, 0)
    print(
        f"{'✅' if hit else '❌'} DB={rows} 스풀={spooled} (기대 DB=2 스풀=0)  "
        "드레인: 빈 스풀 파일은 삽입하지 않고 지운다 (진짜 프롬프트는 주워 담는다)"
    )
    return hit


# ── B. 수집 알람 ──────────────────────────────────────────────────────────────


def build_db(path, n_prompts=10, kind="human", ingest_offset_hours=+1, prompt_age_hours=5):
    """프롬프트 n 개와 수집 도장 하나를 심는다.

    prompt_age_hours: 프롬프트가 얼마나 오래됐나 (창에 들려면 ≥3시간).
    ingest_offset_hours: 수집이 프롬프트보다 **얼마나 나중에** 돌았나. 음수면 이전에 돈 것.
    """
    conn = sqlite3.connect(path)
    conn.executescript(
        """
        CREATE TABLE prompts (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT,
            cwd        TEXT,
            prompt     TEXT NOT NULL,
            created_at TEXT NOT NULL,
            kind       TEXT
        );
        CREATE TABLE ingest_state (
            session_id  TEXT PRIMARY KEY,
            src_mtime   INTEGER NOT NULL,
            ingested_at TEXT NOT NULL
        );
        """
    )
    created_local = datetime.now() - timedelta(hours=prompt_age_hours)
    for i in range(n_prompts):
        conn.execute(
            "INSERT INTO prompts (session_id, cwd, prompt, created_at, kind)"
            " VALUES ('s', '/c', 'p', ?, ?)",
            # 훅과 똑같이: 로컬 시간 + 'T' 구분자
            ((created_local + timedelta(seconds=i)).isoformat(timespec="seconds"), kind),
        )
    # 수집기와 똑같이: UTC + 공백 구분자
    ingested_utc = (
        datetime.now(timezone.utc) - timedelta(hours=prompt_age_hours - ingest_offset_hours)
    ).strftime("%Y-%m-%d %H:%M:%S")
    conn.execute(
        "INSERT INTO ingest_state (session_id, src_mtime, ingested_at) VALUES ('s', 1, ?)",
        (ingested_utc,),
    )
    conn.commit()
    conn.close()


def barks(**kw):
    with tempfile.TemporaryDirectory() as d:
        db = Path(d) / "t.db"
        build_db(db, **kw)
        lp.DB_PATH = str(db)
        return BARREN in (lp.health_warning(check_db=True) or "")


ALARM_CASES = [
    ("건강함 — 수집기가 돌았고 kind 도 채웠다", dict(kind="human"), False),
    ("파서가 조용히 죽었다 — 돌았는데(도장 최신) kind 를 못 채웠다", dict(kind=None), True),
    (
        "아직 안 돌았을 뿐 — 도장이 프롬프트보다 이르다 (접속사가 오탐을 죽인다)",
        dict(kind=None, ingest_offset_hours=-2),
        False,
    ),
    (
        "창이 다 안 찼다 — 프롬프트가 9개뿐이면 판단하지 않는다",
        dict(kind=None, n_prompts=9),
        False,
    ),
    (
        "너무 최근이다 — 3시간이 안 된 프롬프트는 창에 들지 않는다",
        dict(kind=None, prompt_age_hours=1),
        False,
    ),
    (
        "하나라도 채워졌으면 짖지 않는다 (system 도 수집이 손댄 흔적이다)",
        dict(kind="system"),
        False,
    ),
]


def check_alarm():
    ok = True
    for name, fixture, want in ALARM_CASES:
        got = barks(**fixture)
        hit = got == want
        ok &= hit
        print(f"{'✅' if hit else '❌'} 짖음={str(got):5s} (기대 {str(want):5s})  {name}")
    return ok


def main():
    print("── A. 기록 규칙 — 무엇을 적고 무엇을 적지 않는가 ──")
    ok = check_recording()
    ok &= check_drain_skips_empty()
    print()
    print("── B. 수집 알람 — 짖어야 할 때 짖는가, 침묵해야 할 때 침묵하는가 ──")
    ok &= check_alarm()
    print()
    if ok:
        print("PASS — 껍데기는 안 적고 진짜는 안 버린다. 알람에 이빨이 있고 오탐이 없다.")
        return 0
    print("FAIL")
    return 1


if __name__ == "__main__":
    sys.exit(main())
