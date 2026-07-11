# Recall

[English](README.md) · **한국어**

**Claude Code 프롬프트 기록을 찾아보는 로컬 데스크톱 앱.**

Claude Code는 내가 무엇을 물어봤는지 검색할 수 있는 기록을 남기지 않습니다. 3주 전에 썼던 그 좋은 프롬프트 — 마침내 리팩터링을 제대로 굴러가게 만들었던 바로 그 프롬프트 — 는 다시는 찾을 수 없는 세션 트랜스크립트 어딘가에 묻혀 있습니다.

Recall은 그 문제를 해결합니다. hook이 제출하는 모든 프롬프트를 로컬 SQLite 데이터베이스에 기록하고, 앱은 그 데이터베이스를 검색·태그·북마크가 가능한 아카이브로 바꿔줍니다. 각 프롬프트에는 Claude가 실제로 준 응답이 함께 붙습니다.

모든 데이터는 내 컴퓨터 안에만 있습니다. 네트워크 통신도, 텔레메트리도, 계정도 없습니다.

![Recall — 프롬프트 기록 탐색 화면, 프롬프트와 Claude의 응답이 나란히 표시된다](docs/screenshot.png)

<p align="center">
  <em>왼쪽에 프로젝트와 태그, 가운데에 프롬프트 기록, 오른쪽에 프롬프트와 Claude의 응답.<br>
  화면에 열려 있는 프롬프트는 뒤쪽 목록에 보이는 "응답 없음" 뱃지를 만들어낸 바로 그 프롬프트입니다 — Recall이 Recall의 제작 과정을 되짚고 있는 셈이죠.</em>
</p>

---

## 무엇을 할 수 있나

- **검색** — 지금까지 제출한 모든 프롬프트를 대상으로 찾습니다.
- **프로젝트별 그룹핑** — 작업 디렉터리(cwd) 기준으로 묶이며, 각각에 읽기 좋은 별칭을 붙일 수 있습니다 (`/Users/you/dev/some-long-path` → `Payments API`).
- **북마크** — 두고두고 쓸 프롬프트를 표시해 둡니다.
- **태그** — 자유롭게 태그를 달고 태그로 필터링합니다.
- **날짜 범위 필터.**
- **응답 보기** — 해당 프롬프트의 세션 트랜스크립트를 찾아 Claude의 답변을 추출하고, Markdown으로 렌더링해 보여줍니다.
- **편집과 복사** — 재사용할 수 있게 프롬프트를 다듬은 뒤 클립보드로 복사합니다.

---

## 동작 원리

Recall은 Claude Code가 이미 디스크에 남기고 있는 두 개의 데이터 소스를 읽어서 이어 붙입니다.

```
                 ┌─────────────────────────────┐
    프롬프트 ──▶ │  UserPromptSubmit hook      │
      제출       │  hooks/log_prompt.py        │
                 └──────────────┬──────────────┘
                                │ INSERT
                                ▼
                 ~/.claude/prompts.db      ← 프롬프트 원문, session_id, cwd, 시각
                                │
                                │  프롬프트 원문으로 join
                                ▼
                 ~/.claude/projects/<cwd>/<session_id>.jsonl
                                           ← Claude Code가 남기는 세션 트랜스크립트.
                                             어시스턴트의 응답이 여기에 있다.
                                │
                                ▼
                         ┌─────────────┐
                         │   Recall    │
                         └─────────────┘
```

hook은 프롬프트를 작성하는 순간 그것을 붙잡아 DB에 넣습니다. 하지만 **응답은 데이터베이스에 없습니다** — 응답은 Claude Code의 세션 트랜스크립트에 있습니다. 그래서 프롬프트를 열면 Recall이 해당 `.jsonl` 파일을 찾아, 프롬프트 원문과 정확히 일치하는 `user` 라인을 스캔하고, 그 뒤에 이어지는 `assistant` 블록들을 모은 다음, 결과를 데이터베이스에 캐시합니다.

**스택:** Tauri 2 (Rust 백엔드) + React 19 + TypeScript. Rust를 쓴 이유는 속도 때문이 아니라, 브라우저 샌드박스로는 `~/.claude`를 읽을 수 없기 때문입니다.

---

## 설치

Recall은 사용자 자신의 기록을 보여주는 도구이므로, 기록이 쌓여야 비로소 쓸모가 있습니다. **hook을 먼저 설치**하고, Claude Code를 평소처럼 한동안 사용한 뒤, 앱을 실행하세요.

### 1. 프롬프트 기록 hook 설치

hook 스크립트를 Claude Code 설정 디렉터리로 복사합니다.

```bash
mkdir -p ~/.claude/hooks
cp hooks/log_prompt.py ~/.claude/hooks/log_prompt.py
```

`~/.claude/settings.json`에 `UserPromptSubmit` hook으로 등록합니다.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 $HOME/.claude/hooks/log_prompt.py"
          }
        ]
      }
    ]
  }
}
```

hook은 처음 실행될 때 `~/.claude/prompts.db`를 만들고, 이후 프롬프트마다 한 행씩 추가합니다. 어떤 오류가 나도 조용히 종료하므로, hook이 깨지더라도 프롬프트 전송 자체를 막는 일은 없습니다.

동작을 확인하려면 Claude Code에서 프롬프트를 하나 제출한 뒤 다음을 실행해 보세요.

```bash
sqlite3 ~/.claude/prompts.db "SELECT count(*) FROM prompts;"
```

### 2. 앱 빌드 및 실행

[Rust](https://rustup.rs/)와 Node.js가 필요합니다.

```bash
npm install
npm run tauri dev      # 개발 모드
npm run tauri build    # 프로덕션 번들
```

> **이전 버전 hook을 쓰고 있었다면?** Recall은 가져온 응답을 `response` 컬럼에 저장하는데, 구버전 `log_prompt.py`는 이 컬럼을 만들지 않았습니다. 앱 실행 시 오류가 난다면 컬럼을 추가하세요.
>
> ```bash
> sqlite3 ~/.claude/prompts.db "ALTER TABLE prompts ADD COLUMN response TEXT;"
> ```

---

## 데이터 취급

Recall은 완전히 오프라인으로 동작합니다. 접근하는 위치는 이미 사용자 컴퓨터에 존재하는 다음 두 곳뿐입니다.

| 경로 | 접근 |
|---|---|
| `~/.claude/prompts.db` | 읽기 **및 쓰기** |
| `~/.claude/projects/**/*.jsonl` | 읽기 전용 |

`prompts` 테이블은 hook이 만듭니다. 앱은 첫 실행 시 자신이 관리하는 메타데이터(북마크, 태그, 디렉터리 별칭, 응답 캐시)를 위한 테이블 다섯 개를 추가로 생성합니다.

```sql
prompts(id, session_id, cwd, prompt, created_at, response)  -- hook이 기록
cwd_aliases(cwd, alias, updated_at)
prompt_bookmarks(prompt_id, created_at)
tags(id, name, created_at)
prompt_tags(prompt_id, tag_id)
prompt_responses(prompt_id, response, fetched_at)           -- 레거시 캐시
```

**Recall은 프롬프트 기록을 수정하고 삭제할 수 있습니다.** 프롬프트를 편집하면 `prompts` 행에 `UPDATE`가 실행되고, 삭제는 실제 `DELETE`입니다. 되돌리기도, 휴지통도 없습니다. 기록이 소중하다면 `~/.claude/prompts.db`를 백업해 두세요.

---

## 알려진 한계

- **응답 조회는 프롬프트 원문의 정확한 일치에 의존합니다.** 한 세션에 동일한 프롬프트가 두 번 등장하면 Recall은 먼저 찾은 응답을 붙입니다. 프롬프트가 편집되었거나 트랜스크립트와 다른 형태로 저장되었다면 아예 찾지 못할 수도 있습니다. 이 경우에도 프롬프트 자체는 그대로 보이며, 응답만 비어 있습니다.
- **응답은 요약된 형태이지 재생이 아닙니다.** 텍스트 블록은 원문 그대로 보존되지만, 도구 호출은 `[tool: Read]` 같은 마커로 축약됩니다. Recall이 보여주는 것은 Claude가 *말한 것*이지, Claude가 *한 모든 일*은 아닙니다.
- **hook 설치 이후의 프롬프트만 나타납니다.** 기존 트랜스크립트를 소급해서 채우는 기능은 없습니다.
- **세션은 첫 작업 디렉터리를 기준으로 묶입니다.** 세션 도중에 `cd`를 하더라도, 그 세션의 모든 프롬프트는 시작한 위치에 묶인 채로 남습니다.
- macOS에서만 사용해 본 앱입니다. Tauri 특성상 Linux와 Windows에서도 빌드될 것으로 보이지만, 어느 쪽도 테스트하지 않았습니다.

---

## 라이선스

MIT
