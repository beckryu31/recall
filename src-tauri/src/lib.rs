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

    Ok(conn)
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
) -> Result<Vec<Prompt>, String> {
    let conn = open_conn()?;

    let mut sql = String::from(SESSION_FIRST_CTE);
    sql.push_str(
        " SELECT p.id, p.session_id, p.cwd, p.prompt, p.created_at, b.prompt_id,
                 (SELECT GROUP_CONCAT(t.name, char(1))
                  FROM prompt_tags pt
                  JOIN tags t ON t.id = pt.tag_id
                  WHERE pt.prompt_id = p.id) AS tags,
                 (CASE WHEN p.response IS NOT NULL AND p.response <> '' THEN 1 ELSE 0 END) AS has_response
          FROM prompts p
          LEFT JOIN session_first sf ON sf.session_id = p.session_id
          LEFT JOIN prompt_bookmarks b ON b.prompt_id = p.id
          WHERE 1=1",
    );
    let mut bind: Vec<Value> = Vec::new();

    if let Some(q) = search.as_deref().filter(|s| !s.is_empty()) {
        sql.push_str(" AND p.prompt LIKE ?");
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

/// 응답이 비어 있는 프롬프트를 훑어 JSONL 에서 응답을 되살려 보고,
/// 그래도 응답을 찾지 못한 것들만 삭제 후보로 돌려준다.
/// 북마크나 태그가 달린 프롬프트는 사용자가 아끼는 것으로 보고 건드리지 않는다.
#[tauri::command]
fn scan_unanswered_prompts() -> Result<PurgeScan, String> {
    let conn = open_conn()?;

    let unprotected = "(p.response IS NULL OR p.response = '')
         AND NOT EXISTS (SELECT 1 FROM prompt_bookmarks b WHERE b.prompt_id = p.id)
         AND NOT EXISTS (SELECT 1 FROM prompt_tags t WHERE t.prompt_id = p.id)";

    let protected: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompts p
             WHERE (p.response IS NULL OR p.response = '')
               AND (EXISTS (SELECT 1 FROM prompt_bookmarks b WHERE b.prompt_id = p.id)
                 OR EXISTS (SELECT 1 FROM prompt_tags t WHERE t.prompt_id = p.id))",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;

    let ids: Vec<i64> = {
        let sql = format!(
            "SELECT p.id FROM prompts p WHERE {} ORDER BY p.id DESC",
            unprotected
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
    let mut candidates = Vec::new();
    for id in &ids {
        // 응답을 되살릴 수 있으면 살린다. 살아난 프롬프트는 삭제 후보가 아니다.
        if let Ok(Some(_)) = fetch_response_from_jsonl_and_save(&conn, *id) {
            recovered += 1;
            continue;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            list_prompts,
            list_cwds,
            set_cwd_alias,
            get_response,
            fetch_recent_responses,
            save_response,
            update_prompt,
            delete_prompt,
            scan_unanswered_prompts,
            delete_prompts,
            toggle_bookmark,
            list_tags,
            add_prompt_tag,
            remove_prompt_tag
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
