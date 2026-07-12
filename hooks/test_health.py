#!/usr/bin/env python3
"""건강 검사 알람의 회귀 테스트. 의존성 없음 — `python3 hooks/test_health.py`.

## 왜 있는가

알람은 **짖지 않는 것으로는 검증되지 않는다.** 붙여 놓고 한 번도 짖지 않는 알람과,
고장 나서 영원히 침묵하는 알람은 겉보기에 똑같다. 그래서 여기서는 세 가지를 모두 본다:

  · 건강할 때 **침묵**하는가 (오탐이 없는가 — 무시당하는 알람은 없는 알람보다 나쁘다)
  · 고장났을 때 **짖는가** (이빨이 있는가)
  · "아직 안 돌았을 뿐" 일 때 **침묵**하는가 (접속사가 살아 있는가)

## fixture 규칙

**절대 시각을 박지 않는다.** 창("최근 10개 중 ≥3시간 된 것")은 쿼리 시각에 계산되므로,
하드코딩한 날짜는 시간이 흐르면 의미가 바뀐다 — 작성 시점엔 "3시간 전" 이던 값이
내일은 "27시간 전" 이 된다. 전부 지금 기준 상대 시간으로 만든다.

`created_at` 은 훅이 `datetime.isoformat()` 으로 쓰므로 **로컬 시간 + 'T' 구분자**다.
`ingest_state.ingested_at` 은 SQL `datetime('now')` = **UTC + 공백 구분자**다.
이 비대칭이 이 술어의 유일한 함정이므로, fixture 도 **양쪽을 실제와 똑같이** 만든다.
"""
import importlib.util
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


CASES = [
    # (이름, fixture, 짖어야 하나)
    (
        "건강함 — 수집기가 돌았고 kind 도 채웠다",
        dict(kind="human"),
        False,
    ),
    (
        "파서가 조용히 죽었다 — 돌았는데(도장 최신) kind 를 못 채웠다",
        dict(kind=None),
        True,
    ),
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


def main():
    ok = True
    for name, fixture, want in CASES:
        got = barks(**fixture)
        hit = got == want
        ok &= hit
        print(f"{'✅' if hit else '❌'} 짖음={str(got):5s} (기대 {str(want):5s})  {name}")
    print()
    if ok:
        print("PASS — 알람에 이빨이 있고, 오탐이 없다.")
        return 0
    print("FAIL")
    return 1


if __name__ == "__main__":
    sys.exit(main())
