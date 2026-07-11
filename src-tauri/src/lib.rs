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

fn extract_response_from_jsonl(
    path: &Path,
    target_prompt: &str,
) -> Result<Option<String>, String> {
    let file = File::open(path).map_err(|e| format!("JSONL open 실패 ({}): {}", path.display(), e))?;
    let reader = BufReader::new(file);

    let mut collecting = false;
    let mut out = String::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let j: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = j.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if t == "user" {
            let content = j.pointer("/message/content");
            let user_text = content.and_then(|v| v.as_str());
            if let Some(text) = user_text {
                if collecting {
                    break;
                }
                if text == target_prompt {
                    collecting = true;
                }
            }
        } else if t == "assistant" && collecting {
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
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        if !out.is_empty() {
                            out.push_str("\n");
                        }
                        out.push_str(&format!("[tool: {}]", name));
                    }
                }
            }
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
    let (session_id, cwd, prompt_text): (Option<String>, Option<String>, String) = conn
        .query_row(
            "SELECT session_id, cwd, prompt FROM prompts WHERE id = ?1",
            params![prompt_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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

    let resp = extract_response_from_jsonl(&path, &prompt_text)?;
    let Some(response) = resp else {
        return Ok(None);
    };

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

#[tauri::command]
fn delete_prompt(id: i64) -> Result<(), String> {
    let conn = open_conn()?;
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
        "DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM prompt_tags WHERE tag_id = tags.id)",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
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
            toggle_bookmark,
            list_tags,
            add_prompt_tag,
            remove_prompt_tag
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
