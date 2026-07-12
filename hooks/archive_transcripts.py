#!/usr/bin/env python3
"""Claude Code 세션 기록(transcript)의 원본 바이트를 Recall 이 소유하는 저장소로 복사한다.

## 왜 필요한가

Claude Code 는 `cleanupPeriodDays`(기본 30일) 가 지난 transcript 를 **삭제한다.**
`~/.claude/prompts.db` 에 이미 4,900개가 넘는 프롬프트가 있지만, 그중 **61%는
transcript 가 이미 사라졌다.** 그 프롬프트들에게는 DB 의 응답 텍스트가
**영원히 유일한 사본**이다. 되살릴 방법은 없다.

이 스크립트는 원본을 보존 시계 밖으로 빼낸다. 그 효과는 단순한 백업이 아니다:

    **파서가 틀릴 권리를 산다.**

원본이 안전하면 이후의 모든 추출 버그가 **다시 돌리면 되는 실수**가 된다.
2026-04 에 프롬프트 438개가 사라진 건 파서가 틀려서가 아니라, 틀렸을 때
되돌릴 원본이 없어서였다.

## 안전 규칙

- **줄어들지 않는다.** transcript 는 append-only 다. 원본이 아카이브보다 작다면
  뭔가 잘못된 것이므로 **덮어쓰지 않고 경고한다.**
- **원자적으로 쓴다.** `.tmp` 로 쓰고 rename. 중간에 죽어도 반쪽짜리 아카이브는 없다.
- **읽기만 한다.** 원본에는 손대지 않는다. 아무것도 지우지 않는다.
- gzip 으로 저장한다 — 표준 포맷이라 Recall 없이 `zcat` 만으로도 읽힌다.

사용:
    python3 archive_transcripts.py          # 증분 복사
    python3 archive_transcripts.py --stats  # 현황만 출력
"""
import gzip
import json
import os
import shutil
import sys
from pathlib import Path

HOME = Path.home()
SRC_ROOT = HOME / ".claude" / "projects"
DST_ROOT = HOME / ".claude" / "recall-archive"
STATE_PATH = DST_ROOT / "manifest.json"


def load_manifest() -> dict:
    try:
        with open(STATE_PATH) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save_manifest(m: dict):
    DST_ROOT.mkdir(parents=True, exist_ok=True)
    tmp = STATE_PATH.with_suffix(".json.tmp")
    with open(tmp, "w") as f:
        json.dump(m, f, indent=1, sort_keys=True)
    os.replace(tmp, STATE_PATH)


def archive_one(src: Path, rel: str, manifest: dict) -> str:
    """한 파일을 아카이브한다. 상태 문자열을 돌려준다."""
    try:
        size = src.stat().st_size
    except OSError:
        return "unreadable"

    prev = manifest.get(rel)
    if prev is not None:
        if size == prev["src_size"]:
            return "unchanged"
        if size < prev["src_size"]:
            # append-only 여야 하는 파일이 줄었다. 정상이 아니다.
            # 이미 보관한 더 큰 사본을 덮어쓰지 않는다.
            return "shrank"

    dst = DST_ROOT / (rel + ".gz")
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(".gz.tmp")
    try:
        with open(src, "rb") as fin, gzip.open(tmp, "wb", compresslevel=6) as fout:
            shutil.copyfileobj(fin, fout, length=1 << 20)
        os.replace(tmp, dst)
    except OSError as e:
        try:
            tmp.unlink()
        except OSError:
            pass
        print(f"  ! {rel}: {e}", file=sys.stderr)
        return "failed"

    manifest[rel] = {"src_size": size, "gz_size": dst.stat().st_size}
    return "new" if prev is None else "grown"


def human(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.0f}{unit}"
        n /= 1024
    return f"{n:.1f}TB"


def main():
    manifest = load_manifest()

    if "--stats" in sys.argv:
        src = sum(v["src_size"] for v in manifest.values())
        gz = sum(v["gz_size"] for v in manifest.values())
        print(f"아카이브: {len(manifest):,}개 파일")
        print(f"  원본 {human(src)} → 압축 {human(gz)}"
              f"  ({gz / src * 100:.0f}%)" if src else "")
        print(f"  위치: {DST_ROOT}")
        return

    if not SRC_ROOT.is_dir():
        print(f"세션 기록 폴더가 없습니다: {SRC_ROOT}", file=sys.stderr)
        return

    counts = {}
    for src in sorted(SRC_ROOT.rglob("*.jsonl")):
        rel = str(src.relative_to(SRC_ROOT))
        state = archive_one(src, rel, manifest)
        counts[state] = counts.get(state, 0) + 1
        if state == "shrank":
            print(
                f"  ⚠ {rel}: 원본이 아카이브보다 작습니다 "
                f"({src.stat().st_size} < {manifest[rel]['src_size']}). "
                f"덮어쓰지 않았습니다.",
                file=sys.stderr,
            )

    save_manifest(manifest)

    total_src = sum(v["src_size"] for v in manifest.values())
    total_gz = sum(v["gz_size"] for v in manifest.values())
    parts = [f"{k} {v}" for k, v in sorted(counts.items())]
    print(f"아카이브 {len(manifest):,}개 파일 · " + " · ".join(parts))
    print(f"  원본 {human(total_src)} → 압축 {human(total_gz)}"
          + (f" ({total_gz / total_src * 100:.0f}%)" if total_src else ""))
    print(f"  위치: {DST_ROOT}")


if __name__ == "__main__":
    main()
