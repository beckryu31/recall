pub mod ingest;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Serialize, Debug)]
struct Prompt {
    id: i64,
    session_id: Option<String>,
    cwd: Option<String>,
    prompt: String,
    created_at: String,
    bookmarked: bool,
    tags: Vec<String>,
    has_response: bool,
    /// 'human' | 'system'(주입 이벤트) | null(아직 수집 안 됨)
    kind: Option<String>,
}

#[derive(Serialize, Debug)]
struct TagGroup {
    name: String,
    count: i64,
}

#[derive(Serialize, Debug)]
struct CwdGroup {
    cwd: Option<String>,
    alias: Option<String>,
    count: i64,
}

#[derive(Serialize, Debug)]
struct PromptResponse {
    response: String,
    fetched_at: String,
    source: String,
}

#[derive(Serialize, Debug)]
struct BatchFetchResult {
    total: i64,
    fetched: i64,
    not_found: i64,
    failed: i64,
}

#[derive(Serialize, Debug)]
struct PurgeCandidate {
    id: i64,
    prompt: String,
    created_at: String,
    cwd: Option<String>,
}

#[derive(Serialize, Debug)]
struct PurgeScan {
    scanned: i64,
    recovered: i64,
    protected: i64,
    /// 세션 기록이 없거나 읽지 못해 판단이 불가능했던 프롬프트.
    /// 절대 삭제 후보가 아니다. 증거의 부재는 부재의 증거가 아니다.
    unknown: i64,
    candidates: Vec<PurgeCandidate>,
}

#[derive(Serialize, Debug)]
struct DeleteResult {
    deleted: i64,
    backup_path: String,
}

fn db_path() -> PathBuf {
    let mut p = dirs::home_dir().expect("home dir not found");
    p.push(".claude");
    p.push("prompts.db");
    p
}

fn open_conn() -> Result<Connection, String> {
    let conn = Connection::open_with_flags(
        db_path(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("DB open failed ({}): {}", db_path().display(), e))?;

    // 이 DB 에는 쓰는 프로세스가 둘이다: Claude Code 의 UserPromptSubmit 훅과 이 앱.
    // 기본 busy_timeout 은 0 이라 경합하면 곧바로 "database is locked" 로 실패한다.
    // 비대칭이 의도적이다 — 앱 쿼리가 실패하면 사용자가 다시 누르면 그만이지만,
    // 훅이 실패하면 프롬프트 행이 유실되고 그건 디스크 어디에도 복구 소스가 없다.
    // 그래서 훅 쪽은 더 길게(5초) 기다리고, 앱은 짧게(3초) 양보한다.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(3000));

    ensure_aux_tables(&conn)?;
    Ok(conn)
}

/// Recall 이 소유하는 부수 테이블들. `prompts` 는 log_prompt.py 가 만드는 외부 테이블이라
/// 여기서 만들지 않는다.
fn ensure_aux_tables(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS cwd_aliases (
             cwd        TEXT PRIMARY KEY,
             alias      TEXT NOT NULL,
             updated_at TEXT NOT NULL DEFAULT (datetime('now'))
         )",
        [],
    )
    .map_err(|e| e.to_string())?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS prompt_responses (
             prompt_id  INTEGER PRIMARY KEY,
             response   TEXT NOT NULL,
             fetched_at TEXT NOT NULL DEFAULT (datetime('now'))
         )",
        [],
    )
    .map_err(|e| e.to_string())?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS prompt_bookmarks (
             prompt_id  INTEGER PRIMARY KEY,
             created_at TEXT NOT NULL DEFAULT (datetime('now'))
         )",
        [],
    )
    .map_err(|e| e.to_string())?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tags (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             name       TEXT NOT NULL UNIQUE,
             created_at TEXT NOT NULL DEFAULT (datetime('now'))
         )",
        [],
    )
    .map_err(|e| e.to_string())?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS prompt_tags (
             prompt_id INTEGER NOT NULL,
             tag_id    INTEGER NOT NULL,
             PRIMARY KEY (prompt_id, tag_id),
             FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
         )",
        [],
    )
    .map_err(|e| e.to_string())?;

    // prompts 테이블에 transcript user 이벤트의 uuid 캐시 컬럼.
    // (log_prompt.py 가 만든 외부 테이블이라 여기서 방어적으로 추가; 이미 있으면 무시)
    let _ = conn.execute("ALTER TABLE prompts ADD COLUMN msg_uuid TEXT", []);

    // 세그먼트 스키마도 여기서 보장한다. 테이블이 항상 존재하면 "있으면 이 SQL, 없으면 저 SQL"
    // 같은 분기가 사라지고, 분기가 없으면 두 갈래가 어긋날 일도 없다.
    ingest::ensure_schema(conn)?;

    Ok(())
}

fn encode_cwd_to_folder(cwd: &str) -> String {
    cwd.replace('/', "-")
}

fn jsonl_path(cwd: &str, session_id: &str) -> PathBuf {
    let mut p = dirs::home_dir().expect("home dir not found");
    p.push(".claude");
    p.push("projects");
    p.push(encode_cwd_to_folder(cwd));
    p.push(format!("{}.jsonl", session_id));
    p
}

fn find_jsonl(cwd: &str, session_id: &str) -> Option<PathBuf> {
    let direct = jsonl_path(cwd, session_id);
    if direct.exists() {
        return Some(direct);
    }

    let mut projects_dir = dirs::home_dir()?;
    projects_dir.push(".claude");
    projects_dir.push("projects");

    let target = format!("{}.jsonl", session_id);
    let entries = std::fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(&target);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// transcript user 이벤트에서 "실제 타이핑된 프롬프트" 텍스트를 뽑는다.
/// - isMeta/isSidechain(주입·서브에이전트) 이벤트는 프롬프트가 아니므로 제외
/// - content 가 문자열이면 그대로, 배열이면 text/image 블록을 합쳐 반환
/// - tool_result 만 있는 user 이벤트(도구 응답)는 프롬프트가 아니므로 None
fn user_typed_text(j: &serde_json::Value) -> Option<String> {
    if j.get("type").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    if j.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    if j.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let c = j.pointer("/message/content")?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = c.as_array() {
        let mut parts: Vec<String> = Vec::new();
        let mut has_tool_result = false;
        for b in arr {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                Some("image") => parts.push("[Image]".to_string()),
                Some("tool_result") => has_tool_result = true,
                _ => {}
            }
        }
        if has_tool_result && parts.is_empty() {
            return None;
        }
        return Some(parts.join("\n"));
    }
    None
}

/// 텍스트 비교용 정규화(연속 공백/개행 → 단일 공백). 커맨드 래퍼 대조에도 사용.
fn normalize_for_match(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 'YYYY-MM-DDTHH:MM:SS[.fff][Z]' → epoch 초(tz 표기는 무시하고 자릿수만 해석).
/// created_at(로컬)엔 호출측에서 오프셋을 빼 UTC 로 맞춘다.
fn parse_iso_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let se: i64 = s.get(17..19)?.parse().ok()?;
    // days_from_civil (Howard Hinnant): 1970-01-01 = day 0
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

/// 로컬 시간대 오프셋(초). SQLite localtime vs utc 차이로 런타임에 구한다(tz 하드코딩 회피).
fn local_offset_seconds(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT CAST(ROUND((julianday(datetime('now','localtime')) - julianday(datetime('now'))) * 86400) AS INTEGER)",
        [],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

struct UserTurn {
    uuid: String,
    epoch: i64,
    text: String,
}

/// transcript 를 한 번 훑어 실제 user 프롬프트 이벤트 목록(uuid/시각/텍스트)을 만든다.
fn build_user_turns(path: &Path) -> Vec<UserTurn> {
    let mut v = Vec::new();
    let Ok(file) = File::open(path) else {
        return v;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(text) = user_typed_text(&j) {
            let uuid = j
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if uuid.is_empty() {
                continue;
            }
            let epoch = j
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_iso_to_epoch)
                .unwrap_or(0);
            v.push(UserTurn { uuid, epoch, text });
        }
    }
    v
}

/// transcript 1패스로 user 턴 목록과 uuid→응답 맵을 동시에 만든다.
///
/// 프롬프트 하나씩 처리하면 같은 세션 파일을 프롬프트 수만큼 다시 읽게 된다.
/// 전체 재추출처럼 한 세션의 프롬프트를 몰아서 처리할 때는 이 인덱스를 한 번만 만들어 쓴다.
struct SessionIndex {
    turns: Vec<UserTurn>,
    responses: std::collections::HashMap<String, String>,
}

fn build_session_index(path: &Path) -> SessionIndex {
    let mut turns = Vec::new();
    let mut responses = std::collections::HashMap::new();
    let Ok(file) = File::open(path) else {
        return SessionIndex { turns, responses };
    };

    // 현재 수집 중인 프롬프트의 uuid 와, 거기에 쌓고 있는 응답.
    let mut current: Option<String> = None;
    let mut out = String::new();

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let t = j.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if t == "user" {
            // 실제 프롬프트를 만나면 직전 프롬프트의 수집을 마감하고 새로 시작한다.
            // tool_result·주입(isMeta) 이벤트는 user 타입이어도 프롬프트가 아니므로 통과.
            if let Some(text) = user_typed_text(&j) {
                if let Some(u) = current.take() {
                    responses.insert(u, std::mem::take(&mut out));
                }
                let uuid = j
                    .get("uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if uuid.is_empty() {
                    continue;
                }
                let epoch = j
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(parse_iso_to_epoch)
                    .unwrap_or(0);
                turns.push(UserTurn {
                    uuid: uuid.clone(),
                    epoch,
                    text,
                });
                current = Some(uuid);
                out.clear();
            }
        } else if t == "assistant" && current.is_some() {
            append_assistant_blocks(&j, &mut out);
        }
    }
    if let Some(u) = current.take() {
        responses.insert(u, out);
    }

    SessionIndex { turns, responses }
}

/// created_at(로컬)·prompt_text 로 transcript user 이벤트의 uuid 를 찾는다.
/// 1차: 시간창 ±30s, 2차: 텍스트 접두 일치로 동률 보정, 최근접 시각 선택.
fn resolve_uuid(
    turns: &[UserTurn],
    created_at: &str,
    prompt_text: &str,
    offset: i64,
) -> Option<String> {
    let target = parse_iso_to_epoch(created_at)? - offset; // 로컬 → UTC
    let norm_p = normalize_for_match(prompt_text);
    let prefix: String = norm_p.chars().take(24).collect();

    let in_window: Vec<&UserTurn> = turns
        .iter()
        .filter(|t| (t.epoch - target).abs() <= 30)
        .collect();
    if in_window.is_empty() {
        return None;
    }
    let prefer: Vec<&UserTurn> = if prefix.is_empty() {
        Vec::new()
    } else {
        in_window
            .iter()
            .copied()
            .filter(|t| normalize_for_match(&t.text).starts_with(&prefix))
            .collect()
    };
    let pool = if prefer.is_empty() { &in_window } else { &prefer };
    pool.iter()
        .copied()
        .min_by_key(|t| (t.epoch - target).abs())
        .map(|t| t.uuid.clone())
}

/// assistant 이벤트의 text/tool_use 블록을 out 에 이어붙인다.
fn append_assistant_blocks(j: &serde_json::Value, out: &mut String) {
    if let Some(arr) = j.pointer("/message/content").and_then(|v| v.as_array()) {
        for block in arr {
            let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if btype == "text" {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(text);
                }
            } else if btype == "tool_use" {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[tool: {}]", name));
            }
        }
    }
}

/// uuid 로 직접 응답을 추출한다(콘텐츠 형태와 무관). 해당 이벤트부터
/// 다음 "실제 user 프롬프트" 전까지의 assistant 블록을 모은다(tool_result 는 통과).
fn extract_response_by_uuid(path: &Path, target_uuid: &str) -> Result<Option<String>, String> {
    let file = File::open(path).map_err(|e| format!("JSONL open 실패 ({}): {}", path.display(), e))?;
    let reader = BufReader::new(file);
    let mut collecting = false;
    let mut out = String::new();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let j: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let t = j.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "user" {
            // 수집 중 다음 실제 프롬프트를 만나면 종료(도구 응답·주입 이벤트는 통과)
            if collecting && user_typed_text(&j).is_some() {
                break;
            }
            if j.get("uuid").and_then(|v| v.as_str()) == Some(target_uuid) {
                collecting = true;
            }
        } else if t == "assistant" && collecting {
            append_assistant_blocks(&j, &mut out);
        }
    }

    if !collecting {
        return Ok(None);
    }
    Ok(Some(out))
}

/// 텍스트 기반 폴백 매처(배열 content·공백 차이에 견디도록 개선).
/// uuid 해석이 실패한 경우에만 쓰인다.
fn extract_response_from_jsonl(
    path: &Path,
    target_prompt: &str,
) -> Result<Option<String>, String> {
    let file = File::open(path).map_err(|e| format!("JSONL open 실패 ({}): {}", path.display(), e))?;
    let reader = BufReader::new(file);

    let mut collecting = false;
    let mut out = String::new();
    let norm_target = normalize_for_match(target_prompt);

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let j: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = j.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if t == "user" {
            if let Some(text) = user_typed_text(&j) {
                if collecting {
                    break;
                }
                let nt = normalize_for_match(&text);
                let hit = !norm_target.is_empty()
                    && (nt == norm_target
                        || nt.starts_with(&norm_target)
                        || norm_target.starts_with(&nt));
                if hit {
                    collecting = true;
                }
            }
        } else if t == "assistant" && collecting {
            append_assistant_blocks(&j, &mut out);
        }
    }

    if !collecting {
        return Ok(None);
    }
    Ok(Some(out))
}

fn row_to_prompt(row: &rusqlite::Row) -> rusqlite::Result<Prompt> {
    let bookmarked: Option<i64> = row.get(5)?;
    let tags_str: Option<String> = row.get(6)?;
    let tags: Vec<String> = match tags_str {
        Some(s) if !s.is_empty() => s.split('\x01').map(String::from).collect(),
        _ => Vec::new(),
    };
    let has_response: i64 = row.get(7)?;
    Ok(Prompt {
        id: row.get(0)?,
        session_id: row.get(1)?,
        cwd: row.get(2)?,
        prompt: row.get(3)?,
        created_at: row.get(4)?,
        bookmarked: bookmarked.is_some(),
        tags,
        has_response: has_response != 0,
        kind: row.get(8)?,
    })
}

const SESSION_FIRST_CTE: &str = "WITH session_first AS (
    SELECT p.session_id, p.cwd
    FROM prompts p
    INNER JOIN (
        SELECT session_id, MIN(id) as min_id
        FROM prompts
        WHERE session_id IS NOT NULL
        GROUP BY session_id
    ) m ON m.session_id = p.session_id AND m.min_id = p.id
)";

#[tauri::command]
fn list_prompts(
    limit: i64,
    offset: i64,
    search: Option<String>,
    cwd: Option<String>,
    only_bookmarked: Option<bool>,
    tag: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    include_system: Option<bool>,
) -> Result<Vec<Prompt>, String> {
    let conn = open_conn()?;

    let mut sql = String::from(SESSION_FIRST_CTE);
    // has_response: 응답이 있다는 것은 이제 두 가지를 뜻한다 —
    // 레거시 `response` 텍스트가 있거나, **세그먼트로 수집됐거나.**
    // 이 술어를 안 고치면 완전본으로 수집된 프롬프트가 전부 "응답 없음" 으로 보이고,
    // 삭제 후보로도 잡힌다(그건 Stage 0 에서 이미 막았다).
    sql.push_str(
        " SELECT p.id, p.session_id, p.cwd, p.prompt, p.created_at, b.prompt_id,
                 (SELECT GROUP_CONCAT(t.name, char(1))
                  FROM prompt_tags pt
                  JOIN tags t ON t.id = pt.tag_id
                  WHERE pt.prompt_id = p.id) AS tags,
                 (CASE WHEN (p.response IS NOT NULL AND p.response <> '')
                         OR EXISTS (SELECT 1 FROM turn_segments s WHERE s.prompt_id = p.id)
                       THEN 1 ELSE 0 END) AS has_response,
                 p.kind
          FROM prompts p
          LEFT JOIN session_first sf ON sf.session_id = p.session_id
          LEFT JOIN prompt_bookmarks b ON b.prompt_id = p.id
          WHERE 1=1",
    );
    let mut bind: Vec<Value> = Vec::new();

    // task-notification 같은 주입 이벤트도 훅을 발생시켜 행을 만든다(382개).
    // 그 내용은 원래 프롬프트의 응답 안에 접혀 있으므로 목록에서는 기본으로 숨긴다.
    // 지우지는 않는다 — 토글로 볼 수 있다.
    //
    // 단, **북마크나 태그가 달린 것은 숨기지 않는다.** 사용자가 명시적으로 아낀다고 표시한
    // 것을 앱이 임의로 감추면 안 된다. 실제로 그런 행이 있다(id 4668 은 북마크된
    // <task-notification> 이다) — 이 예외가 없으면 그 북마크가 목록에서 사라진다.
    if !include_system.unwrap_or(false) {
        sql.push_str(
            " AND (p.kind IS NULL OR p.kind <> 'system'
                   OR b.prompt_id IS NOT NULL
                   OR EXISTS (SELECT 1 FROM prompt_tags t WHERE t.prompt_id = p.id))",
        );
    }

    if let Some(q) = search.as_deref().filter(|s| !s.is_empty()) {
        // 산문(narrative)까지 검색한다. 도구 덤프는 제외되므로 노이즈가 없다.
        sql.push_str(" AND (p.prompt LIKE ? OR p.narrative LIKE ?)");
        bind.push(Value::Text(format!("%{}%", q)));
        bind.push(Value::Text(format!("%{}%", q)));
    }
    if let Some(c) = cwd.as_deref() {
        if c == "__NULL__" {
            sql.push_str(" AND COALESCE(sf.cwd, p.cwd) IS NULL");
        } else if !c.is_empty() {
            sql.push_str(" AND COALESCE(sf.cwd, p.cwd) = ?");
            bind.push(Value::Text(c.to_string()));
        }
    }
    if only_bookmarked.unwrap_or(false) {
        sql.push_str(" AND b.prompt_id IS NOT NULL");
    }
    if let Some(t) = tag.as_deref().filter(|s| !s.is_empty()) {
        sql.push_str(
            " AND p.id IN (SELECT pt.prompt_id FROM prompt_tags pt
                           JOIN tags tg ON tg.id = pt.tag_id
                           WHERE tg.name = ?)",
        );
        bind.push(Value::Text(t.to_string()));
    }
    if let Some(df) = date_from.as_deref().filter(|s| !s.is_empty()) {
        sql.push_str(" AND date(p.created_at) >= date(?)");
        bind.push(Value::Text(df.to_string()));
    }
    if let Some(dt) = date_to.as_deref().filter(|s| !s.is_empty()) {
        sql.push_str(" AND date(p.created_at) <= date(?)");
        bind.push(Value::Text(dt.to_string()));
    }
    sql.push_str(" ORDER BY p.id DESC LIMIT ? OFFSET ?");
    bind.push(Value::Integer(limit));
    bind.push(Value::Integer(offset));

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mapped = stmt
        .query_map(params_from_iter(bind.iter()), row_to_prompt)
        .map_err(|e| e.to_string())?;
    let rows: Vec<Prompt> = mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

#[tauri::command]
fn list_cwds() -> Result<Vec<CwdGroup>, String> {
    let conn = open_conn()?;
    let sql = format!(
        "{cte}
         SELECT
             COALESCE(sf.cwd, p.cwd) as canonical_cwd,
             (SELECT alias FROM cwd_aliases a WHERE a.cwd = COALESCE(sf.cwd, p.cwd)) as alias,
             COUNT(*) as cnt
         FROM prompts p
         LEFT JOIN session_first sf ON sf.session_id = p.session_id
         GROUP BY COALESCE(sf.cwd, p.cwd)
         ORDER BY cnt DESC, canonical_cwd ASC",
        cte = SESSION_FIRST_CTE
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mapped = stmt
        .query_map([], |row| {
            Ok(CwdGroup {
                cwd: row.get(0)?,
                alias: row.get(1)?,
                count: row.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_cwd_alias(cwd: String, alias: String) -> Result<(), String> {
    let conn = open_conn()?;
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        conn.execute("DELETE FROM cwd_aliases WHERE cwd = ?1", params![cwd])
            .map_err(|e| e.to_string())?;
    } else {
        conn.execute(
            "INSERT INTO cwd_aliases (cwd, alias, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(cwd) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at",
            params![cwd, trimmed],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn save_to_prompts_response(
    conn: &Connection,
    prompt_id: i64,
    response: &str,
) -> Result<String, String> {
    conn.execute(
        "UPDATE prompts SET response = ?1 WHERE id = ?2",
        params![response, prompt_id],
    )
    .map_err(|e| e.to_string())?;
    conn.query_row("SELECT datetime('now')", [], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())
}

/// JSONL에서 해당 프롬프트의 응답을 추출해 prompts.response 에 저장한다.
/// 성공 시 (응답 본문, fetched_at) 을 반환하고, 응답을 찾지 못하면 Ok(None) 을 반환한다.
fn fetch_response_from_jsonl_and_save(
    conn: &Connection,
    prompt_id: i64,
) -> Result<Option<(String, String)>, String> {
    let (session_id, cwd, prompt_text, created_at, msg_uuid): (
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT session_id, cwd, prompt, created_at, msg_uuid FROM prompts WHERE id = ?1",
            params![prompt_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(|e| format!("프롬프트를 찾을 수 없음: {}", e))?;

    let session_id = session_id.ok_or("session_id가 없어 JSONL을 찾을 수 없음")?;
    let cwd = cwd.ok_or("cwd가 없어 JSONL을 찾을 수 없음")?;

    let path = find_jsonl(&cwd, &session_id).ok_or_else(|| {
        format!(
            "JSONL 파일이 없음 (session_id={}, cwd={})",
            session_id, cwd
        )
    })?;

    let nonempty = |s: String| -> Option<String> {
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    };

    // 1) 캐시된 uuid 우선
    let mut response: Option<String> = None;
    let mut resolved_uuid: Option<String> = msg_uuid.clone();
    if let Some(u) = msg_uuid.as_deref() {
        response = extract_response_by_uuid(&path, u)?.and_then(nonempty);
    }
    // 2) uuid 없거나 실패 → 시간 근접 + 텍스트로 uuid 해석 후 추출
    if response.is_none() {
        let offset = local_offset_seconds(conn);
        let turns = build_user_turns(&path);
        if let Some(u) = resolve_uuid(&turns, &created_at, &prompt_text, offset) {
            if let Some(r) = extract_response_by_uuid(&path, &u)?.and_then(nonempty) {
                resolved_uuid = Some(u);
                response = Some(r);
            }
        }
    }
    // 3) 최후 폴백 → 개선된 텍스트 매칭
    if response.is_none() {
        response = extract_response_from_jsonl(&path, &prompt_text)?.and_then(nonempty);
    }

    let Some(response) = response else {
        return Ok(None);
    };

    // 해석된 uuid 를 비어 있을 때만 캐시(다음부턴 곧장 uuid 경로)
    if let Some(u) = resolved_uuid {
        let _ = conn.execute(
            "UPDATE prompts SET msg_uuid = ?1 WHERE id = ?2 AND (msg_uuid IS NULL OR msg_uuid = '')",
            params![u, prompt_id],
        );
    }

    let fetched_at = save_to_prompts_response(conn, prompt_id, &response)?;
    Ok(Some((response, fetched_at)))
}

#[tauri::command]
fn get_response(
    prompt_id: i64,
    refresh: Option<bool>,
) -> Result<Option<PromptResponse>, String> {
    let conn = open_conn()?;
    let force = refresh.unwrap_or(false);

    if !force {
        let saved: Option<(Option<String>, String)> = conn
            .query_row(
                "SELECT response, created_at FROM prompts WHERE id = ?1",
                params![prompt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((Some(response), created_at)) = saved {
            return Ok(Some(PromptResponse {
                response,
                fetched_at: created_at,
                source: "saved".into(),
            }));
        }

        let cached: Option<String> = conn
            .query_row(
                "SELECT response FROM prompt_responses WHERE prompt_id = ?1",
                params![prompt_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(response) = cached {
            let fetched_at = save_to_prompts_response(&conn, prompt_id, &response)?;
            return Ok(Some(PromptResponse {
                response,
                fetched_at,
                source: "saved".into(),
            }));
        }
    }

    match fetch_response_from_jsonl_and_save(&conn, prompt_id)? {
        Some((response, fetched_at)) => Ok(Some(PromptResponse {
            response,
            fetched_at,
            source: "saved".into(),
        })),
        None => Ok(None),
    }
}

/// 최근 24시간(로컬 시간 기준) 내에 생성된 모든 프롬프트의 응답을
/// JSONL 에서 다시 읽어와 저장(갱신)한다.
#[tauri::command]
fn fetch_recent_responses() -> Result<BatchFetchResult, String> {
    let conn = open_conn()?;

    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT id FROM prompts
                 WHERE datetime(created_at) >= datetime('now', '-24 hours', 'localtime')
                 ORDER BY id DESC",
            )
            .map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };

    let mut fetched = 0;
    let mut not_found = 0;
    let mut failed = 0;
    for id in &ids {
        match fetch_response_from_jsonl_and_save(&conn, *id) {
            Ok(Some(_)) => fetched += 1,
            Ok(None) => not_found += 1,
            Err(_) => failed += 1,
        }
    }

    Ok(BatchFetchResult {
        total: ids.len() as i64,
        fetched,
        not_found,
        failed,
    })
}

/// 저장된 응답을 전부 버리고 현재 매칭 로직으로 다시 추출한다.
///
/// 왜 필요한가: get_response 는 prompts.response 에 값이 있으면 그대로 돌려주고
/// transcript 를 다시 읽지 않는다. 추출 로직이 바뀐 버전으로 올라오면, 비어 있던 응답은
/// 자동으로 새로 채워지지만 예전 로직이 "잘못 채워 넣은" 응답은 영원히 그대로 남는다.
/// 업그레이드 후 한 번 돌려 과거 기록까지 새 로직으로 맞추는 용도.
///
/// 세션 파일을 프롬프트마다 다시 읽지 않고 세션당 한 번만 인덱싱한다.
///
/// 이 함수는 저장된 응답을 **대량으로 덮어쓴다.** 새 추출 결과가 저장된 바이트보다 낫다고
/// 자동으로 판단하는 것이므로, 삭제와 같은 등급의 파괴적 작업이다.
/// 그래서 delete_prompts 와 마찬가지로 **백업이 성공한 뒤에만** 진행한다.
#[tauri::command]
fn refetch_all_responses() -> Result<BatchFetchResult, String> {
    let conn = open_conn()?;
    backup_db(&conn)?;
    let offset = local_offset_seconds(&conn);

    // (cwd, session_id) 로 묶어, 같은 transcript 를 쓰는 프롬프트를 한 번에 처리한다.
    struct Row {
        id: i64,
        prompt: String,
        created_at: String,
        msg_uuid: Option<String>,
    }
    let mut by_session: std::collections::BTreeMap<(String, String), Vec<Row>> =
        std::collections::BTreeMap::new();
    let mut total: i64 = 0;

    {
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, cwd, prompt, created_at, msg_uuid
                 FROM prompts
                 WHERE session_id IS NOT NULL AND cwd IS NOT NULL
                 ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (id, session_id, cwd, prompt, created_at, msg_uuid) =
                row.map_err(|e| e.to_string())?;
            total += 1;
            by_session.entry((cwd, session_id)).or_default().push(Row {
                id,
                prompt,
                created_at,
                msg_uuid,
            });
        }
    }

    let mut fetched = 0;
    let mut not_found = 0;
    let mut failed = 0;

    for ((cwd, session_id), rows) in by_session {
        let Some(path) = find_jsonl(&cwd, &session_id) else {
            // transcript 가 사라진 세션. 저장된 응답은 손대지 않는다.
            failed += rows.len() as i64;
            continue;
        };
        let idx = build_session_index(&path);

        for row in rows {
            // 캐시된 uuid 가 있으면 그대로, 없으면 시각·텍스트로 해석한다.
            let uuid = row
                .msg_uuid
                .filter(|u| !u.is_empty())
                .or_else(|| resolve_uuid(&idx.turns, &row.created_at, &row.prompt, offset));

            let Some(uuid) = uuid else {
                not_found += 1;
                continue;
            };
            let Some(resp) = idx.responses.get(&uuid).filter(|s| !s.trim().is_empty()) else {
                not_found += 1;
                continue;
            };

            conn.execute(
                "UPDATE prompts SET response = ?1, msg_uuid = ?2 WHERE id = ?3",
                params![resp, uuid, row.id],
            )
            .map_err(|e| e.to_string())?;
            fetched += 1;
        }
    }

    Ok(BatchFetchResult {
        total,
        fetched,
        not_found,
        failed,
    })
}

#[tauri::command]
fn save_response(prompt_id: i64, response: String) -> Result<(), String> {
    let conn = open_conn()?;
    conn.execute(
        "UPDATE prompts SET response = ?1 WHERE id = ?2",
        params![response, prompt_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn update_prompt(id: i64, prompt: String) -> Result<(), String> {
    let conn = open_conn()?;
    conn.execute(
        "UPDATE prompts SET prompt = ?1 WHERE id = ?2",
        params![prompt, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn delete_prompt_row(conn: &Connection, id: i64) -> Result<(), String> {
    conn.execute("DELETE FROM prompts WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM prompt_bookmarks WHERE prompt_id = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM prompt_tags WHERE prompt_id = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM prompt_responses WHERE prompt_id = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn drop_orphan_tags(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM prompt_tags WHERE tag_id = tags.id)",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_prompt(id: i64) -> Result<(), String> {
    let conn = open_conn()?;
    delete_prompt_row(&conn, id)?;
    drop_orphan_tags(&conn)?;
    Ok(())
}

/// 자동 수집이 응답을 `turn_segments` 에 쓰기 시작하면 `prompts.response` 는 비어 있는 채로
/// 남는다. 그때 "응답 없음 = 지울 것"이라는 옛 의미를 그대로 두면 **완벽하게 수집된 프롬프트가
/// 전부 삭제 후보가 된다.** 테이블이 아직 없더라도 이 가드를 지금 넣어, 마이그레이션과 술어
/// 수정이 어긋나는 창을 아예 만들지 않는다.
fn has_segments_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='turn_segments'",
        [],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// 응답이 비어 있는 프롬프트를 훑어 JSONL 에서 응답을 되살려 보고,
/// **부재의 적극적 증거가 있는 것만** 삭제 후보로 돌려준다.
///
/// 삭제 후보가 되려면 세 가지가 모두 참이어야 한다:
///   1. 세션 기록을 **찾았고**
///   2. 그 기록 안에서 이 프롬프트의 턴에 응답이 **없음을 확인했고**
///   3. 48시간이 지나 진행 중인 턴이 아님이 확실하다
///
/// 세션 기록이 없거나 읽지 못한 것은 `unknown` 이며 **절대 삭제 후보가 아니다.**
/// 2026-04 에 438개가 삭제된 사고가 정확히 이 경로였다 — 복구 로직이 고장 나 있었고,
/// 복구 실패가 곧 "지워도 되는 것"으로 해석됐다. 증거의 부재는 부재의 증거가 아니다.
///
/// 북마크나 태그가 달린 프롬프트는 사용자가 아끼는 것으로 보고 건드리지 않는다.
#[tauri::command]
fn scan_unanswered_prompts() -> Result<PurgeScan, String> {
    let conn = open_conn()?;
    scan_unanswered(&conn)
}

fn scan_unanswered(conn: &Connection) -> Result<PurgeScan, String> {
    let no_response = if has_segments_table(conn) {
        "(p.response IS NULL OR p.response = '')
         AND NOT EXISTS (SELECT 1 FROM turn_segments s WHERE s.prompt_id = p.id)"
    } else {
        "(p.response IS NULL OR p.response = '')"
    };
    let loved = "(EXISTS (SELECT 1 FROM prompt_bookmarks b WHERE b.prompt_id = p.id)
                  OR EXISTS (SELECT 1 FROM prompt_tags t WHERE t.prompt_id = p.id))";
    // 진행 중인 턴을 삭제 후보로 집지 않는다. 응답은 턴이 끝나야 존재한다.
    let settled = "datetime(p.created_at) < datetime('now', '-48 hours', 'localtime')";

    let protected: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM prompts p WHERE {no_response} AND {loved}"),
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;

    let ids: Vec<i64> = {
        let sql = format!(
            "SELECT p.id FROM prompts p
             WHERE {no_response} AND NOT {loved} AND {settled}
             ORDER BY p.id DESC"
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };

    let mut recovered = 0;
    let mut unknown = 0;
    let mut candidates = Vec::new();
    for id in &ids {
        match fetch_response_from_jsonl_and_save(conn, *id) {
            // 세션 기록에서 응답을 되살렸다. 삭제 후보가 아니다.
            Ok(Some(_)) => {
                recovered += 1;
                continue;
            }
            // 세션 기록은 찾았는데 이 턴에 응답이 없었다 — 부재의 적극적 증거.
            Ok(None) => {}
            // 세션 기록 자체가 없거나 읽지 못했다 — 판단 불가. 손대지 않는다.
            Err(_) => {
                unknown += 1;
                continue;
            }
        }
        let row = conn
            .query_row(
                "SELECT id, prompt, created_at, cwd FROM prompts WHERE id = ?1",
                params![id],
                |row| {
                    Ok(PurgeCandidate {
                        id: row.get(0)?,
                        prompt: row.get(1)?,
                        created_at: row.get(2)?,
                        cwd: row.get(3)?,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        candidates.push(row);
    }

    Ok(PurgeScan {
        scanned: ids.len() as i64,
        recovered,
        protected,
        unknown,
        candidates,
    })
}

fn backups_dir() -> PathBuf {
    let mut p = dirs::home_dir().expect("home dir not found");
    p.push(".claude");
    p.push("recall-backups");
    p
}

/// 오래된 백업을 지우고 최근 KEEP 개만 남긴다.
fn prune_backups(keep: usize) {
    let Ok(entries) = std::fs::read_dir(backups_dir()) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("db"))
        .collect();
    // 파일명이 prompts-YYYYmmdd-HHMMSS.db 라 이름순 = 시간순.
    files.sort();
    if files.len() > keep {
        for old in &files[..files.len() - keep] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// 되돌릴 수 없는 삭제 직전에 DB 스냅샷을 남긴다.
/// 단순 파일 복사가 아니라 VACUUM INTO 를 쓰므로 쓰기가 진행 중이어도 일관된 백업이 나온다.
fn backup_db(conn: &Connection) -> Result<PathBuf, String> {
    let dir = backups_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("백업 폴더 생성 실패: {}", e))?;

    let stamp: String = conn
        .query_row(
            "SELECT strftime('%Y%m%d-%H%M%S', 'now', 'localtime')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let path = dir.join(format!("prompts-{}.db", stamp));

    // 같은 초에 두 번 눌러도 덮어쓰지 않도록. VACUUM INTO 는 기존 파일이 있으면 실패한다.
    if path.exists() {
        return Ok(path);
    }

    conn.execute(
        "VACUUM INTO ?1",
        params![path.to_str().ok_or("백업 경로가 유효하지 않음")?],
    )
    .map_err(|e| format!("백업 실패: {}", e))?;

    prune_backups(10);
    Ok(path)
}

/// 사용자가 확인한 id 만 지운다. 스캔 결과를 그대로 재사용하므로
/// 화면에서 본 목록과 실제로 지워지는 목록이 어긋나지 않는다.
/// 삭제는 되돌릴 수 없으므로 반드시 백업이 성공한 뒤에만 진행한다.
#[tauri::command]
fn delete_prompts(ids: Vec<i64>) -> Result<DeleteResult, String> {
    if ids.is_empty() {
        return Ok(DeleteResult {
            deleted: 0,
            backup_path: String::new(),
        });
    }

    let mut conn = open_conn()?;
    // 백업이 실패하면 아무것도 지우지 않는다.
    let backup = backup_db(&conn)?;

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for id in &ids {
        delete_prompt_row(&tx, *id)?;
    }
    drop_orphan_tags(&tx)?;
    tx.commit().map_err(|e| e.to_string())?;

    Ok(DeleteResult {
        deleted: ids.len() as i64,
        backup_path: backup.to_string_lossy().to_string(),
    })
}

#[tauri::command]
fn list_tags() -> Result<Vec<TagGroup>, String> {
    let conn = open_conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT t.name, COUNT(pt.prompt_id) as cnt
             FROM tags t
             LEFT JOIN prompt_tags pt ON pt.tag_id = t.id
             GROUP BY t.id
             ORDER BY cnt DESC, t.name ASC",
        )
        .map_err(|e| e.to_string())?;
    let mapped = stmt
        .query_map([], |row| {
            Ok(TagGroup {
                name: row.get(0)?,
                count: row.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_prompt_tag(prompt_id: i64, name: String) -> Result<(), String> {
    let conn = open_conn()?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("태그 이름이 비어있습니다".into());
    }
    conn.execute(
        "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
        params![trimmed],
    )
    .map_err(|e| e.to_string())?;
    let tag_id: i64 = conn
        .query_row("SELECT id FROM tags WHERE name = ?1", params![trimmed], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR IGNORE INTO prompt_tags (prompt_id, tag_id) VALUES (?1, ?2)",
        params![prompt_id, tag_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn remove_prompt_tag(prompt_id: i64, name: String) -> Result<(), String> {
    let conn = open_conn()?;
    conn.execute(
        "DELETE FROM prompt_tags
         WHERE prompt_id = ?1 AND tag_id = (SELECT id FROM tags WHERE name = ?2)",
        params![prompt_id, name],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM tags
         WHERE name = ?1 AND NOT EXISTS (SELECT 1 FROM prompt_tags WHERE tag_id = tags.id)",
        params![name],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn toggle_bookmark(prompt_id: i64, bookmarked: bool) -> Result<bool, String> {
    let conn = open_conn()?;
    if bookmarked {
        conn.execute(
            "INSERT OR IGNORE INTO prompt_bookmarks (prompt_id) VALUES (?1)",
            params![prompt_id],
        )
        .map_err(|e| e.to_string())?;
    } else {
        conn.execute(
            "DELETE FROM prompt_bookmarks WHERE prompt_id = ?1",
            params![prompt_id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(bookmarked)
}

// ───────────────────────── 세그먼트 수집 / 조회 ─────────────────────────

/// 응답 세그먼트의 **헤더만.** `body` 는 일부러 빼놨다.
///
/// 이게 렌더링 문제의 전부다. 실측: 프롬프트 5055 의 응답은 222,991 바이트 / 개행 62,941 개
/// (yarn.lock diff) 라, react-markdown 이 ~94,000 개 노드를 만들어 앱을 1.1초 얼린다.
/// SQLite 는 큰 BLOB 을 overflow page 에 두므로 **컬럼을 안 고르면 안 읽는다** —
/// 최악의 턴도 2.5MB 대신 헤더 몇십 KB 만 넘어온다. 본문은 사용자가 펼칠 때만 가져온다.
#[derive(Serialize, Debug)]
struct SegmentHeader {
    id: i64,
    seq: i64,
    kind: String,
    name: Option<String>,
    tool_id: Option<String>,
    preview: String,
    nbytes: i64,
    nlines: i64,
    is_error: bool,
    /// 작은 산문은 헤더에 실어 보낸다 — 왕복을 아끼고, 대부분의 응답은 이걸로 끝난다.
    body: Option<String>,
}

/// 이 크기까지는 헤더와 함께 보낸다. 넘으면 사용자가 펼칠 때 따로 가져온다.
const INLINE_BODY_MAX_BYTES: i64 = 24_000;
const INLINE_BODY_MAX_LINES: i64 = 400;

#[tauri::command]
fn list_segments(prompt_id: i64) -> Result<Vec<SegmentHeader>, String> {
    let conn = open_conn()?;
    if !has_segments_table(&conn) {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.seq, s.kind, s.name, s.tool_id, s.preview, s.nbytes, s.nlines, s.is_error,
                    CASE WHEN s.kind = 'text' AND s.nbytes <= ?2 AND s.nlines <= ?3
                         THEN COALESCE(e.body, s.body) ELSE NULL END AS inline_body
             FROM turn_segments s
             LEFT JOIN segment_edits e
                    ON e.prompt_id = s.prompt_id AND e.src_uuid = s.src_uuid
                   AND e.block_idx = s.block_idx
             WHERE s.prompt_id = ?1
             ORDER BY s.seq",
        )
        .map_err(|e| e.to_string())?;
    let mapped = stmt
        .query_map(
            params![prompt_id, INLINE_BODY_MAX_BYTES, INLINE_BODY_MAX_LINES],
            |r| {
                Ok(SegmentHeader {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    kind: r.get(2)?,
                    name: r.get(3)?,
                    tool_id: r.get(4)?,
                    preview: r.get(5)?,
                    nbytes: r.get(6)?,
                    nlines: r.get(7)?,
                    is_error: r.get::<_, i64>(8)? != 0,
                    body: r.get(9)?,
                })
            },
        )
        .map_err(|e| e.to_string())?;
    mapped.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// 접힌 세그먼트를 펼칠 때만 본문을 가져온다.
#[tauri::command]
fn get_segment_body(segment_id: i64) -> Result<Option<String>, String> {
    let conn = open_conn()?;
    conn.query_row(
        "SELECT COALESCE(e.body, s.body)
         FROM turn_segments s
         LEFT JOIN segment_edits e
                ON e.prompt_id = s.prompt_id AND e.src_uuid = s.src_uuid
               AND e.block_idx = s.block_idx
         WHERE s.id = ?1",
        params![segment_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// 모든 세션을 다시 수집한다.
///
/// 매번 전체를 다시 읽는다. 391MB 를 몇 초에 훑으므로 바이트 커서가 필요 없고,
/// 커서가 없으므로 "체크포인트를 EOF 로 잡아 이후 내용을 잃는" 실패 모드가 **존재하지 않는다.**
/// `prompts.response` 는 이 경로에서 절대 쓰이지 않는다.
pub fn run_ingest() -> Result<ingest::IngestStats, String> {
    let mut conn = open_conn()?;
    ingest::ensure_schema(&conn)?;
    let offset = local_offset_seconds(&conn);
    ingest::ingest_all(
        &mut conn,
        offset,
        &parse_iso_to_epoch,
        &|cwd: &str, sid: &str| find_jsonl(cwd, sid).or_else(|| Some(jsonl_path(cwd, sid))),
    )
}

#[tauri::command(async)]
fn ingest_all_sessions() -> Result<ingest::IngestStats, String> {
    run_ingest()
}

/// 아직 수집 안 된 최근 프롬프트가 있는 세션만 수집한다. 앱의 tick 이 부른다.
/// 보통 진행 중인 세션 하나뿐이라 파일 하나만 읽는다.
#[tauri::command(async)]
fn ingest_pending() -> Result<ingest::IngestStats, String> {
    let mut conn = open_conn()?;
    let offset = local_offset_seconds(&conn);
    ingest::ingest_pending(&mut conn, offset, &parse_iso_to_epoch, &|cwd: &str, sid: &str| {
        find_jsonl(cwd, sid).or_else(|| Some(jsonl_path(cwd, sid)))
    })
}

/// 목록이 살아 있게 만드는 값싼 쿼리. **쓰기가 없다.**
///
/// 4,900행짜리 테이블에 대한 인덱스 SELECT 세 개라 마이크로초 단위다.
/// 앱은 이 값만 3초마다 보고, 바뀌었을 때만 실제 목록을 다시 읽는다.
#[derive(Serialize, Debug)]
struct DbHead {
    /// 새 프롬프트가 들어왔는지
    max_prompt_id: i64,
    /// 응답이 수집됐는지
    max_segment_id: i64,
    /// 수집기가 아직 분류하지 못한 최근 프롬프트 수. 상태 표시용이다.
    ///
    /// "세그먼트가 없는 프롬프트" 로 세면 안 된다 — task-notification 행은 턴을 소유하지
    /// 않으므로 영원히 세그먼트가 없고, 그러면 이 값이 절대 0이 되지 않는다.
    /// `kind IS NULL` 은 "수집기가 이 행을 아직 못 봤다" 를 정확히 뜻한다.
    pending: i64,
}

#[tauri::command]
fn db_head() -> Result<DbHead, String> {
    let conn = open_conn()?;
    conn.query_row(
        "SELECT COALESCE((SELECT MAX(id) FROM prompts), 0),
                COALESCE((SELECT MAX(id) FROM turn_segments), 0),
                (SELECT COUNT(*) FROM prompts p
                  WHERE p.session_id IS NOT NULL
                    AND p.created_at >= datetime('now','-48 hours','localtime')
                    AND p.kind IS NULL)",
        [],
        |r| {
            Ok(DbHead {
                max_prompt_id: r.get(0)?,
                max_segment_id: r.get(1)?,
                pending: r.get(2)?,
            })
        },
    )
    .map_err(|e| e.to_string())
}

/// rollback journal 모드에서는 쓰기 하나가 DB 전체를 잠근다. 훅과 앱이 함께 쓰는 이 DB 에선
/// 그게 곧 프롬프트 행 유실이다. WAL 은 읽기와 쓰기가 서로를 막지 않으므로 이 경합을 없앤다.
///
/// journal_mode 는 DB 헤더에 영속되는 값이라 한 번만 바꾸면 훅도 자동으로 혜택을 받는다.
/// 되돌릴 수 없는 헤더 변경이므로 **스냅샷을 먼저 뜨고**, 전환 뒤 **반드시 읽어서 확인한다** —
/// `PRAGMA journal_mode=WAL` 은 다른 연결이 락을 쥐고 있으면 실패를 알리지 않고
/// 그냥 '현재 모드'를 돌려주기 때문이다.
fn migrate_journal_mode() -> Result<String, String> {
    let conn = open_conn()?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(mode);
    }

    backup_db(&conn)?;

    let got: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if !got.eq_ignore_ascii_case("wal") {
        return Err(format!("WAL 전환이 적용되지 않음 (현재 모드: {got})"));
    }
    Ok(got)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 실패해도 앱은 뜬다 — 예전과 똑같이 동작할 뿐이다. 조용히 넘기지는 않는다.
    match migrate_journal_mode() {
        Ok(mode) => eprintln!("[recall] journal_mode = {mode}"),
        Err(e) => eprintln!("[recall] WAL 전환 실패, rollback journal 로 계속합니다: {e}"),
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            list_prompts,
            list_cwds,
            set_cwd_alias,
            get_response,
            fetch_recent_responses,
            refetch_all_responses,
            save_response,
            update_prompt,
            delete_prompt,
            scan_unanswered_prompts,
            delete_prompts,
            toggle_bookmark,
            list_tags,
            add_prompt_tag,
            remove_prompt_tag,
            list_segments,
            get_segment_body,
            ingest_all_sessions,
            ingest_pending,
            db_head
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 삭제 후보 판정만 검증한다. 이 프로젝트에는 CI 가 없고, 여기서 한 번 틀려서
    /// 프롬프트 438개가 사라졌다. `cargo test` 로 못 박아 둔다.
    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE prompts (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT,
                 cwd        TEXT,
                 prompt     TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 response   TEXT,
                 msg_uuid   TEXT
             );",
        )
        .unwrap();
        ensure_aux_tables(&conn).unwrap();
        conn
    }

    /// created_at 을 리터럴로 박으면 시간이 흘러 '48시간 이전' 의 의미가 바뀐다.
    /// 항상 지금 기준 상대 시각으로 만든다.
    fn insert_prompt(conn: &Connection, session: Option<&str>, cwd: Option<&str>, age_hours: i64) -> i64 {
        conn.execute(
            "INSERT INTO prompts (session_id, cwd, prompt, created_at, response)
             VALUES (?1, ?2, 'p', datetime('now', 'localtime', ?3), NULL)",
            params![session, cwd, format!("-{} hours", age_hours)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// 세션 기록 파일이 없으면 '판단 불가' 다. 절대 삭제 후보가 될 수 없다.
    /// 438개가 사라진 경로가 바로 이것이었다: 복구가 실패했다는 사실이
    /// "지워도 되는 것" 의 근거로 쓰였다.
    #[test]
    fn missing_transcript_is_unknown_never_a_candidate() {
        let conn = test_db();
        insert_prompt(&conn, Some("no-such-session"), Some("/no/such/cwd"), 72);

        let scan = scan_unanswered(&conn).unwrap();

        assert_eq!(scan.unknown, 1, "세션 기록이 없으면 unknown 이어야 한다");
        assert!(
            scan.candidates.is_empty(),
            "세션 기록을 못 찾은 프롬프트가 삭제 후보가 됐다 — 438 사고의 재발"
        );
    }

    /// session_id / cwd 가 아예 없는 행도 마찬가지로 판단 불가다.
    #[test]
    fn prompt_without_session_is_unknown_never_a_candidate() {
        let conn = test_db();
        insert_prompt(&conn, None, None, 72);

        let scan = scan_unanswered(&conn).unwrap();

        assert_eq!(scan.unknown, 1);
        assert!(scan.candidates.is_empty());
    }

    /// 진행 중일 수 있는 최근 턴은 아예 검사 대상이 아니다.
    /// 응답은 턴이 끝나야 존재하므로, "지금 응답이 없다" 는 아무 의미가 없다.
    #[test]
    fn recent_prompt_is_not_scanned() {
        let conn = test_db();
        insert_prompt(&conn, Some("s"), Some("/c"), 1);

        let scan = scan_unanswered(&conn).unwrap();

        assert_eq!(scan.scanned, 0, "48시간 안쪽 프롬프트를 검사했다");
        assert!(scan.candidates.is_empty());
    }

    /// Stage 2 대비. 자동 수집이 turn_segments 에 쓰기 시작하면 prompts.response 는
    /// 비어 있는 채로 남는다. 그때 "응답 없음 = 지울 것" 이라는 옛 의미를 그대로 두면
    /// **완벽하게 수집된 프롬프트가 전부 삭제 후보가 된다.**
    #[test]
    fn prompt_with_segments_is_never_a_candidate() {
        let conn = test_db(); // ensure_aux_tables 가 turn_segments 를 만든다
        let id = insert_prompt(&conn, Some("no-such-session"), Some("/no/such/cwd"), 72);
        conn.execute(
            "INSERT INTO turn_segments (prompt_id, src_uuid, block_idx, seq, kind)
             VALUES (?1, 'u1', 0, 0, 'text')",
            params![id],
        )
        .unwrap();

        let scan = scan_unanswered(&conn).unwrap();

        assert_eq!(
            scan.scanned, 0,
            "세그먼트로 수집이 끝난 프롬프트를 '응답 없음' 으로 봤다"
        );
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.unknown, 0);
    }

    /// 북마크가 달린 프롬프트는 검사조차 하지 않는다.
    #[test]
    fn bookmarked_prompt_is_protected() {
        let conn = test_db();
        let id = insert_prompt(&conn, Some("no-such-session"), Some("/no/such/cwd"), 72);
        conn.execute(
            "INSERT INTO prompt_bookmarks (prompt_id) VALUES (?1)",
            params![id],
        )
        .unwrap();

        let scan = scan_unanswered(&conn).unwrap();

        assert_eq!(scan.protected, 1);
        assert_eq!(scan.scanned, 0);
        assert!(scan.candidates.is_empty());
    }
}
