//! 세션 기록(transcript)을 읽어 프롬프트별 응답을 **세그먼트**로 수집한다.
//!
//! # 소유권 규칙 — 이 파일에서 가장 중요한 문장
//!
//! > transcript 의 user 이벤트가 턴을 **소유**하는 조건은
//! > **`UserPromptSubmit` 훅이 발생했다는 것** = **`prompts` 행이 존재한다는 것**이다.
//!
//! 훅이 발생했다는 사실 자체가 "사람이 입력했다"의 근거다. 텍스트나 `promptSource` 로
//! 추측하지 않는다. 실측으로 반증됐다: `promptSource ∈ {typed,queued,…}` 로 판정하면
//! 커스텀 슬래시 커맨드(`/memory-cleanup` 같은 skill)를 놓친다 — 그것들은 훅을 발생시켜
//! DB 행을 만들지만 transcript 에는 `<command-name>…</command-name>` 로 확장되어
//! `promptSource: None` 으로 기록되기 때문이다. 133개 세션 중 21개에서 프롬프트가 사라졌다.
//!
//! 소유자가 아닌 모든 user 이벤트 — task-notification, 내장 슬래시커맨드, bash 입력,
//! 인터럽트 — 는 **직전 소유 턴 안으로 접힌다.** 그래서 하나의 프롬프트가 하나의 완결된
//! 이야기가 된다.
//!
//! # 안전 불변식
//!
//! - 자동화는 `turn_segments` 와 `prompts.narrative` 에만 쓴다. **`prompts.response` 는 동결.**
//!   그 컬럼은 프롬프트 61%에게 유일하게 남은 사본이며 자동화가 손댈 이유가 없다.
//! - 이 파일에는 `DELETE` 문이 없다.
//! - 세그먼트의 병합 키는 `(prompt_id, src_uuid, block_idx)` 다. 표시 순서 `seq` 가 아니다 —
//!   파서를 고치면 seq 는 바뀌지만 src_uuid 는 안 바뀐다.
//!
//! # 왜 체크포인트가 없는가
//!
//! 391MB 전체를 2.2초에 훑는다(Python 기준, Rust 는 더 빠르다). 바이트 커서를 두면
//! "첫 Stop 에서 EOF 로 체크포인트 → 이후 4.87MB 유실" 같은 실패 모드가 생기는데,
//! 매번 전부 다시 읽으면 **그 실패 모드가 존재하지 않는다.**

use flate2::read::GzDecoder;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub const INGEST_VER: i64 = 1;

/// transcript 의 이벤트 한 줄.
struct Ev {
    etype: String,
    uuid: String,
    turn_id: Option<String>, // transcript 의 promptId
    origin_kind: Option<String>,
    is_meta: bool,
    ts: Option<String>,
    content: serde_json::Value, // message.content
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub src_uuid: String,
    pub block_idx: i64,
    pub kind: String,
    pub name: Option<String>,
    pub tool_id: Option<String>,
    pub preview: String,
    pub body: String,
    pub nbytes: i64,
    pub nlines: i64,
    pub is_error: bool,
    pub ts: Option<String>,
}

#[derive(Serialize, Debug, Default)]
pub struct IngestStats {
    pub sessions: i64,
    pub sessions_missing: i64,
    pub prompts_matched: i64,
    pub prompts_unresolved: i64,
    pub segments: i64,
    pub folded: i64,
}

/// DB 의 prompts 행 하나.
struct Row {
    id: i64,
    prompt: String,
    created_at: String,
    cc_turn_id: Option<String>,
    msg_uuid: Option<String>,
}

// ─────────────────────────────── 읽기 ───────────────────────────────

fn archive_root() -> PathBuf {
    let mut p = dirs::home_dir().expect("home dir not found");
    p.push(".claude");
    p.push("recall-archive");
    p
}

/// 세션 파일을 연다. 원본이 있으면 원본, 없으면 아카이브(.gz).
///
/// Claude Code 는 `cleanupPeriodDays` 가 지나면 원본을 지운다. 아카이브 폴백이 없으면
/// 그 순간부터 수집이 조용히 멈춘다.
fn open_transcript(path: &PathBuf) -> Option<Box<dyn BufRead>> {
    if let Ok(f) = File::open(path) {
        return Some(Box::new(BufReader::new(f)));
    }
    // 원본이 사라졌다 → 아카이브에서 같은 상대경로의 .gz 를 찾는다.
    let home = dirs::home_dir()?;
    let projects = home.join(".claude").join("projects");
    let rel = path.strip_prefix(&projects).ok()?;
    let gz = archive_root().join(format!("{}.gz", rel.display()));
    let f = File::open(gz).ok()?;
    Some(Box::new(BufReader::new(GzDecoder::new(f))))
}

fn parse_events(reader: Box<dyn BufRead>) -> Vec<Ev> {
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // 서브에이전트 이벤트는 부모 turn 에 섞이면 안 된다. 별도 파일로 따로 다룬다.
        if j.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        out.push(Ev {
            etype: j
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            uuid: j
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn_id: j
                .get("promptId")
                .and_then(|v| v.as_str())
                .map(String::from),
            origin_kind: j
                .pointer("/origin/kind")
                .and_then(|v| v.as_str())
                .map(String::from),
            is_meta: j.get("isMeta").and_then(|v| v.as_bool()) == Some(true),
            ts: j
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(String::from),
            content: j
                .pointer("/message/content")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        });
    }
    out
}

// ─────────────────────────── 세그먼트 만들기 ───────────────────────────

fn count_lines(s: &str) -> i64 {
    s.bytes().filter(|b| *b == b'\n').count() as i64 + 1
}

fn preview_of(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(max).collect()
}

/// `[tool: Bash — cargo test]` 처럼 한 줄로 읽히게 만든다.
fn tool_preview(name: &str, input: &serde_json::Value) -> String {
    let key = match name {
        "Bash" | "BashOutput" => "command",
        "Read" | "Write" | "NotebookEdit" => "file_path",
        "Edit" => "file_path",
        "Grep" | "Glob" => "pattern",
        "Agent" | "Task" => "description",
        "WebFetch" | "WebSearch" => "url",
        "Skill" => "skill",
        _ => "",
    };
    let hint = input
        .get(key)
        .and_then(|v| v.as_str())
        .or_else(|| input.get("query").and_then(|v| v.as_str()))
        .or_else(|| input.get("description").and_then(|v| v.as_str()));
    match hint {
        Some(h) => format!("{} {}", name, preview_of(h, 100)),
        None => name.to_string(),
    }
}

fn text_of_content(c: &serde_json::Value) -> String {
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// 접혀 들어오는(소유자가 아닌) user 이벤트의 종류를 붙인다.
/// **분류를 틀려도 본문은 절대 잃지 않는다** — 이건 표시용 라벨일 뿐이다.
fn injected_name(ev: &Ev, text: &str) -> (String, Option<String>) {
    if text.contains("[Request interrupted by user") {
        return ("interrupt".into(), None);
    }
    if ev.origin_kind.as_deref() == Some("task-notification") {
        return ("injected".into(), Some("task-notification".into()));
    }
    let t = text.trim_start();
    if t.starts_with("<command-name>")
        || t.starts_with("<command-message>")
        || t.starts_with("<local-command")
    {
        return ("injected".into(), Some("command".into()));
    }
    if t.starts_with("<bash-input>") || t.starts_with("<bash-stdout>") {
        return ("injected".into(), Some("bash".into()));
    }
    if ev.is_meta {
        return ("injected".into(), Some("meta".into()));
    }
    ("injected".into(), None)
}

fn seg(
    ev: &Ev,
    block_idx: i64,
    kind: &str,
    name: Option<String>,
    tool_id: Option<String>,
    preview: String,
    body: String,
    is_error: bool,
) -> Segment {
    Segment {
        src_uuid: ev.uuid.clone(),
        block_idx,
        kind: kind.to_string(),
        name,
        tool_id,
        preview,
        nbytes: body.len() as i64,
        nlines: count_lines(&body),
        body,
        is_error,
        ts: ev.ts.clone(),
    }
}

/// 한 이벤트에서 세그먼트들을 뽑는다.
/// `is_owner` 이면 그 이벤트의 텍스트는 **프롬프트 자체**이므로 세그먼트로 만들지 않는다.
fn segments_of(ev: &Ev, is_owner: bool) -> Vec<Segment> {
    let mut out = Vec::new();

    if ev.etype == "assistant" {
        let Some(arr) = ev.content.as_array() else {
            return out;
        };
        for (i, b) in arr.iter().enumerate() {
            let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match bt {
                "text" => {
                    let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if t.is_empty() {
                        continue;
                    }
                    out.push(seg(
                        ev,
                        i as i64,
                        "text",
                        None,
                        None,
                        preview_of(t, 200),
                        t.to_string(),
                        false,
                    ));
                }
                "tool_use" => {
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let input = b.get("input").cloned().unwrap_or(serde_json::json!({}));
                    let body = serde_json::to_string_pretty(&input).unwrap_or_default();
                    let id = b.get("id").and_then(|v| v.as_str()).map(String::from);
                    out.push(seg(
                        ev,
                        i as i64,
                        "tool_use",
                        Some(name.to_string()),
                        id,
                        tool_preview(name, &input),
                        body,
                        false,
                    ));
                }
                // thinking 은 저장하지 않는다. 실측: 2,987개 전부 본문이 비어 있고
                // signature 만 있다 — Claude Code 는 추론 내용을 영속화하지 않는다.
                _ => {}
            }
        }
        return out;
    }

    if ev.etype != "user" {
        return out;
    }

    // user 이벤트: tool_result 블록 + (소유자가 아니라면) 주입된 텍스트
    if let Some(arr) = ev.content.as_array() {
        for (i, b) in arr.iter().enumerate() {
            let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match bt {
                "tool_result" => {
                    let raw = b.get("content").cloned().unwrap_or(serde_json::Value::Null);
                    let body = match &raw {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => String::new(),
                        v => text_of_content(v),
                    };
                    let is_err = b.get("is_error").and_then(|v| v.as_bool()) == Some(true);
                    let id = b
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    out.push(seg(
                        ev,
                        i as i64,
                        "tool_result",
                        None,
                        id,
                        preview_of(&body, 200),
                        body,
                        is_err,
                    ));
                }
                "text" if !is_owner => {
                    let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if t.is_empty() {
                        continue;
                    }
                    let (kind, name) = injected_name(ev, t);
                    out.push(seg(
                        ev,
                        i as i64,
                        &kind,
                        name,
                        None,
                        preview_of(t, 200),
                        t.to_string(),
                        false,
                    ));
                }
                "image" if !is_owner => {
                    out.push(seg(
                        ev,
                        i as i64,
                        "image",
                        None,
                        None,
                        "[Image]".into(),
                        String::new(),
                        false,
                    ));
                }
                _ => {}
            }
        }
        return out;
    }

    // content 가 문자열인 user 이벤트 (슬래시커맨드·bash·task-notification 등)
    if let Some(s) = ev.content.as_str() {
        if is_owner || s.is_empty() {
            return out;
        }
        let (kind, name) = injected_name(ev, s);
        out.push(seg(
            ev,
            0,
            &kind,
            name,
            None,
            preview_of(s, 200),
            s.to_string(),
            false,
        ));
    }
    out
}

/// 검색·미리보기·손편집이 여는 산문. 도구 본문은 한 줄 요약으로 접는다.
pub fn narrative_of(segs: &[Segment]) -> String {
    let mut out = String::new();
    for s in segs {
        let line = match s.kind.as_str() {
            "text" => s.body.clone(),
            "tool_use" => format!("[tool: {}]", s.preview),
            "tool_result" => continue, // 산문에는 넣지 않는다 — 노이즈의 대부분이다
            "interrupt" => "[사용자가 중단함]".to_string(),
            "injected" => match s.name.as_deref() {
                Some("task-notification") => format!("[백그라운드 작업: {}]", preview_of(&s.preview, 80)),
                Some("command") => format!("[커맨드: {}]", preview_of(&s.preview, 80)),
                Some("bash") => format!("[bash: {}]", preview_of(&s.preview, 80)),
                _ => continue,
            },
            _ => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&line);
    }
    out
}

// ──────────────────────────── 소유자 해석 ────────────────────────────

/// 텍스트 비교용 정규화. lib.rs 의 것과 같은 규칙이어야 한다.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Owner {
    row_id: i64,
    ev_idx: usize,
    /// 'exact'(훅이 준 키) | 'uuid'(레거시 캐시) | 'fuzzy'(시각+텍스트) | 'conflict'(키와 텍스트 불일치)
    match_kind: &'static str,
    is_system: bool,
}

/// prompts 행들을 transcript 의 이벤트에 붙인다.
///
/// Tier 0 `cc_turn_id`  — 훅이 준 promptId. 정확하다. 텍스트가 달라도(슬래시커맨드) 맞는다.
/// Tier 1 `msg_uuid`    — 예전 버전이 캐시해 둔 이벤트 uuid.
/// Tier 2 시각+텍스트   — 레거시 행을 위한 휴리스틱. 마지막 수단.
/// 이 user 이벤트가 "프롬프트처럼 생겼는가" — 텍스트 내용이 있고 tool_result 전용이 아니다.
fn is_prompt_like(e: &Ev) -> bool {
    if e.etype != "user" {
        return false;
    }
    if let Some(s) = e.content.as_str() {
        return !s.trim().is_empty();
    }
    if let Some(arr) = e.content.as_array() {
        let has_text = arr
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"));
        return has_text;
    }
    false
}

/// 훅이 준 키와 프롬프트 텍스트가 서로를 지지하는가.
///
/// **이 검사를 최적화로 빼지 말 것.** `prompt_id` 는 Claude Code 의 가변 전역
/// (`Pt.promptId`)을 훅 시점에 읽은 값이다. 만약 그게 한 턴 어긋나면 Tier 0 는
/// **직전 턴의 응답을 완전한 확신을 갖고** 붙인다 — 지금의 퍼지 매처보다 엄격히 나쁘다.
/// 이 검사가 그 실패를 "가능성 낮음"이 아니라 **불가능**으로 만든다.
///
/// 관대해야 한다: 슬래시커맨드는 DB 에 `/memory-cleanup`, transcript 에는
/// `<command-name>/memory-cleanup</command-name>` 로 들어간다. 접두 비교는 실패하므로
/// **포함 관계**를 본다.
fn text_agrees(prompt: &str, ev: &Ev) -> bool {
    let p = norm(prompt);
    if p.chars().count() < 4 {
        return true; // 너무 짧으면 판단 근거가 없다. 키를 믿는다.
    }
    let needle: String = p.chars().take(16).collect();
    let hay = norm(&text_of_content(&ev.content));
    if hay.is_empty() {
        return true; // 이미지 전용 프롬프트 등. 키를 믿는다.
    }
    hay.contains(&needle) || p.contains(&hay.chars().take(16).collect::<String>())
}

fn resolve_owners(
    evs: &[Ev],
    rows: &[Row],
    offset: i64,
    parse_epoch: impl Fn(&str) -> Option<i64>,
) -> Vec<Owner> {
    // promptId -> 그 턴의 "프롬프트처럼 생긴" user 이벤트들 (transcript 순서대로).
    //
    // 첫 이벤트 하나만 잡으면 안 된다: queued 프롬프트(작업 중 미리 입력)는 **같은 promptId 를
    // 공유**한다 (실측: 741개 promptId 중 9개가 사람 프롬프트를 2개 이상 소유). 그때 두 DB 행이
    // 같은 이벤트를 가리키면 하나는 빈 턴이 되고 다른 하나가 전부를 삼킨다.
    let mut prompt_evs: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_uuid: HashMap<&str, usize> = HashMap::new();
    let mut first_of_turn: HashMap<&str, usize> = HashMap::new();
    for (i, e) in evs.iter().enumerate() {
        if let Some(t) = e.turn_id.as_deref() {
            first_of_turn.entry(t).or_insert(i);
            if is_prompt_like(e) {
                prompt_evs.entry(t).or_default().push(i);
            }
        }
        by_uuid.entry(e.uuid.as_str()).or_insert(i);
    }

    let mut owners = Vec::new();
    let mut taken: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // 같은 cc_turn_id 를 가진 행이 몇 번째인지 센다.
    let mut nth_of_turn: HashMap<&str, usize> = HashMap::new();

    for r in rows {
        let mut hit: Option<(usize, &'static str)> = None;
        let mut conflicted = false;

        // Tier 0 — 훅이 준 promptId. 텍스트가 달라도(슬래시커맨드) ID 가 맞으면 맞다.
        if let Some(t) = r.cc_turn_id.as_deref().filter(|s| !s.is_empty()) {
            let n = nth_of_turn.entry(t).or_insert(0);
            let cand = prompt_evs
                .get(t)
                .and_then(|v| v.get(*n).copied())
                .or_else(|| first_of_turn.get(t).copied());
            *n += 1;
            if let Some(i) = cand {
                if taken.contains(&i) {
                    // 같은 이벤트를 두 행이 가리킨다 — 키만 믿고 덮어쓰지 않는다.
                    conflicted = true;
                } else if text_agrees(&r.prompt, &evs[i]) {
                    hit = Some((i, "exact"));
                } else {
                    // 키와 텍스트가 어긋난다. 키를 믿지 않고 아래 단계로 떨어뜨린다.
                    conflicted = true;
                }
            }
        }
        // Tier 1 — 예전 버전이 캐시해 둔 이벤트 uuid. 그 자체가 휴리스틱의 산물이므로
        // 텍스트 검사를 똑같이 통과해야 한다.
        if hit.is_none() {
            if let Some(u) = r.msg_uuid.as_deref().filter(|s| !s.is_empty()) {
                if let Some(&i) = by_uuid.get(u) {
                    if !taken.contains(&i) && text_agrees(&r.prompt, &evs[i]) {
                        hit = Some((i, "uuid"));
                    }
                }
            }
        }
        // Tier 2 — 시각 ±30초, 텍스트 접두/포함으로 동률 보정
        if hit.is_none() {
            if let Some(target) = parse_epoch(&r.created_at).map(|t| t - offset) {
                let np = norm(&r.prompt);
                let prefix: String = np.chars().take(24).collect();
                let mut best: Option<(i64, usize)> = None;
                for (i, e) in evs.iter().enumerate() {
                    if e.etype != "user" || e.turn_id.is_none() || taken.contains(&i) {
                        continue;
                    }
                    let Some(et) = e.ts.as_deref().and_then(&parse_epoch) else {
                        continue;
                    };
                    let d = (et - target).abs();
                    if d > 30 {
                        continue;
                    }
                    let txt = norm(&text_of_content(&e.content));
                    // 슬래시커맨드는 DB 텍스트("/memory-cleanup")가 transcript 에서
                    // "<command-name>/memory-cleanup</command-name>" 로 확장된다.
                    // 접두 비교는 실패하므로 포함 관계도 본다.
                    let textual = !prefix.is_empty()
                        && (txt.starts_with(&prefix) || txt.contains(prefix.trim()));
                    let score = if textual { d } else { d + 1000 };
                    if best.map_or(true, |(b, _)| score < b) {
                        best = Some((score, i));
                    }
                }
                if let Some((_, i)) = best {
                    hit = Some((i, "fuzzy"));
                }
            }
        }

        let Some((ev_idx, mut match_kind)) = hit else {
            // 해석 실패 — **손대지 않는다.** 레거시 `prompts.response` 가 그대로 남는다.
            // 세그먼트도 만들지 않는다. 아무것도 잃지 않는다.
            continue;
        };
        // 키와 텍스트가 어긋났는데 다른 경로로 붙은 경우는 감사 가능하게 표시한다.
        if conflicted {
            match_kind = "conflict";
        }
        taken.insert(ev_idx);

        // 분류는 **추측이 아니라 매칭된 이벤트의 속성을 읽는 것**이다.
        let is_system = evs[ev_idx].origin_kind.as_deref() == Some("task-notification");

        owners.push(Owner {
            row_id: r.id,
            ev_idx,
            match_kind,
            is_system,
        });
    }

    owners.sort_by_key(|o| o.ev_idx);
    owners
}

/// 소유 턴별로 세그먼트를 만든다.
/// 소유자가 아닌 user 이벤트(task-notification·슬래시커맨드·bash·인터럽트)는 **접힌다.**
fn build_turns(evs: &[Ev], owners: &[Owner]) -> Vec<(i64, Vec<Segment>)> {
    // 사람 소유자만 턴을 시작한다. system(task-notification) 행은 턴을 시작하지 않고,
    // 그 내용은 직전 사람 턴 안으로 접힌다.
    let human: Vec<&Owner> = owners.iter().filter(|o| !o.is_system).collect();
    let mut out = Vec::new();

    for (n, o) in human.iter().enumerate() {
        let start = o.ev_idx;
        let end = human.get(n + 1).map(|x| x.ev_idx).unwrap_or(evs.len());
        let mut segs = Vec::new();
        for i in start..end {
            let is_owner = i == start;
            segs.extend(segments_of(&evs[i], is_owner));
        }
        out.push((o.row_id, segs));
    }
    out
}

// ───────────────────────────── 쓰기 ─────────────────────────────

pub fn ensure_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS turn_segments (
             id        INTEGER PRIMARY KEY,
             prompt_id INTEGER NOT NULL,
             src_uuid  TEXT    NOT NULL,
             block_idx INTEGER NOT NULL,
             seq       INTEGER NOT NULL,
             kind      TEXT    NOT NULL,
             name      TEXT,
             tool_id   TEXT,
             preview   TEXT    NOT NULL DEFAULT '',
             body      TEXT,
             nbytes    INTEGER NOT NULL DEFAULT 0,
             nlines    INTEGER NOT NULL DEFAULT 0,
             is_error  INTEGER NOT NULL DEFAULT 0,
             ts        TEXT,
             UNIQUE (prompt_id, src_uuid, block_idx)
         );
         CREATE INDEX IF NOT EXISTS idx_segments_prompt ON turn_segments(prompt_id, seq);

         -- 사람이 세그먼트에 단 수정. 위치(seq)가 아니라 (src_uuid, block_idx) 로 키잉한다 —
         -- 파서를 고치면 seq 는 바뀌지만 src_uuid 는 안 바뀐다. 위치로 키잉하면 파서 개선이
         -- 사람의 수정을 조용히 엉뚱한 블록에 재지정한다.
         CREATE TABLE IF NOT EXISTS segment_edits (
             prompt_id INTEGER NOT NULL,
             src_uuid  TEXT    NOT NULL,
             block_idx INTEGER NOT NULL,
             body      TEXT    NOT NULL,
             edited_at TEXT    NOT NULL DEFAULT (datetime('now')),
             PRIMARY KEY (prompt_id, src_uuid, block_idx)
         );

         -- 세션 기록이 자랐는지만 본다. 체크포인트가 아니라 더티 플래그다 --
         -- 바이트 오프셋을 저장하지 않으므로 커서를 잘못 잡아 뒷부분을 잃는 실패가 없다.
         -- 자란 파일은 언제나 처음부터 통째로 다시 읽는다.
         CREATE TABLE IF NOT EXISTS ingest_state (
             session_id  TEXT PRIMARY KEY,
             src_mtime   INTEGER NOT NULL,
             ingested_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )
    .map_err(|e| e.to_string())?;

    for ddl in [
        "ALTER TABLE prompts ADD COLUMN kind TEXT",
        "ALTER TABLE prompts ADD COLUMN narrative TEXT",
        "ALTER TABLE prompts ADD COLUMN match_kind TEXT",
        "ALTER TABLE prompts ADD COLUMN ingest_ver INTEGER",
    ] {
        let _ = conn.execute(ddl, []);
    }
    Ok(())
}

/// 세션 하나를 수집한다. `prompts.response` 는 절대 건드리지 않는다.
pub fn ingest_session(
    conn: &Connection,
    session_id: &str,
    path: &PathBuf,
    offset: i64,
    parse_epoch: &impl Fn(&str) -> Option<i64>,
    stats: &mut IngestStats,
) -> Result<(), String> {
    let Some(reader) = open_transcript(path) else {
        stats.sessions_missing += 1;
        return Ok(());
    };
    let evs = parse_events(reader);
    if evs.is_empty() {
        stats.sessions_missing += 1;
        return Ok(());
    }

    let rows: Vec<Row> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, prompt, created_at, cc_turn_id, msg_uuid
                 FROM prompts WHERE session_id = ?1 ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map(params![session_id], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    prompt: r.get(1)?,
                    created_at: r.get(2)?,
                    cc_turn_id: r.get(3)?,
                    msg_uuid: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        mapped.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
    };
    if rows.is_empty() {
        return Ok(());
    }

    let owners = resolve_owners(&evs, &rows, offset, parse_epoch);
    stats.prompts_matched += owners.len() as i64;
    stats.prompts_unresolved += (rows.len() - owners.len()) as i64;
    stats.folded += owners.iter().filter(|o| o.is_system).count() as i64;
    stats.sessions += 1;

    // 소유자로 잡힌 행에 분류·매칭방식을 기록한다.
    for o in &owners {
        conn.execute(
            "UPDATE prompts SET kind = ?1, match_kind = ?2 WHERE id = ?3",
            params![
                if o.is_system { "system" } else { "human" },
                o.match_kind,
                o.row_id
            ],
        )
        .map_err(|e| e.to_string())?;
    }

    for (row_id, segs) in build_turns(&evs, &owners) {
        let narrative = narrative_of(&segs);
        for (seq, s) in segs.iter().enumerate() {
            conn.execute(
                "INSERT INTO turn_segments
                   (prompt_id, src_uuid, block_idx, seq, kind, name, tool_id,
                    preview, body, nbytes, nlines, is_error, ts)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(prompt_id, src_uuid, block_idx) DO UPDATE SET
                    seq=excluded.seq, kind=excluded.kind, name=excluded.name,
                    tool_id=excluded.tool_id, preview=excluded.preview, body=excluded.body,
                    nbytes=excluded.nbytes, nlines=excluded.nlines,
                    is_error=excluded.is_error, ts=excluded.ts",
                params![
                    row_id,
                    s.src_uuid,
                    s.block_idx,
                    seq as i64,
                    s.kind,
                    s.name,
                    s.tool_id,
                    s.preview,
                    s.body,
                    s.nbytes,
                    s.nlines,
                    s.is_error as i64,
                    s.ts
                ],
            )
            .map_err(|e| e.to_string())?;
            stats.segments += 1;
        }
        conn.execute(
            "UPDATE prompts SET narrative = ?1, ingest_ver = ?2 WHERE id = ?3",
            params![narrative, INGEST_VER, row_id],
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn sessions_where(conn: &Connection, extra: &str) -> Result<Vec<(String, String)>, String> {
    let sql = format!(
        "SELECT p.session_id, MIN(p.cwd)
         FROM prompts p
         WHERE p.session_id IS NOT NULL AND p.cwd IS NOT NULL {extra}
         GROUP BY p.session_id"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mapped = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?;
    mapped.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// prompts 에 있는 모든 세션을 수집한다.
pub fn ingest_all(
    conn: &mut Connection,
    offset: i64,
    parse_epoch: &impl Fn(&str) -> Option<i64>,
    resolve_path: &impl Fn(&str, &str) -> Option<PathBuf>,
) -> Result<IngestStats, String> {
    let sessions = sessions_where(conn, "")?;
    ingest_sessions(conn, &sessions, offset, parse_epoch, resolve_path)
}

fn mtime_of(path: &PathBuf) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 세션 기록이 **자란 세션만** 수집한다. 앱이 15초마다 부르는 경로다.
///
/// mtime 으로 거르는 게 핵심이다. "세그먼트가 없는 프롬프트"로 거르면 안 된다 —
/// task-notification 행(`kind='system'`)은 턴을 소유하지 않으므로 **영원히 세그먼트가 없고**,
/// 그러면 조건이 절대 거짓이 되지 않아 11MB 파일을 15초마다 헛되이 다시 파싱한다.
///
/// 반대로 "한 번 수집했으면 끝"으로 잡아도 안 된다 — 진행 중인 턴은 응답이 계속 자라므로
/// **파일이 자라는 동안 계속 다시 읽어야** 응답이 실시간으로 채워진다.
/// mtime 은 그 둘을 정확히 가른다.
pub fn ingest_pending(
    conn: &mut Connection,
    offset: i64,
    parse_epoch: &impl Fn(&str) -> Option<i64>,
    resolve_path: &impl Fn(&str, &str) -> Option<PathBuf>,
) -> Result<IngestStats, String> {
    let recent = sessions_where(
        conn,
        "AND p.created_at >= datetime('now','-7 days','localtime')",
    )?;

    let mut todo = Vec::new();
    for (sid, cwd) in recent {
        let Some(path) = resolve_path(&cwd, &sid) else {
            continue;
        };
        let m = mtime_of(&path);
        if m == 0 {
            continue;
        }
        let seen: i64 = conn
            .query_row(
                "SELECT src_mtime FROM ingest_state WHERE session_id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if m > seen {
            todo.push((sid, cwd));
        }
    }
    ingest_sessions(conn, &todo, offset, parse_epoch, resolve_path)
}

fn ingest_sessions(
    conn: &mut Connection,
    sessions: &[(String, String)],
    offset: i64,
    parse_epoch: &impl Fn(&str) -> Option<i64>,
    resolve_path: &impl Fn(&str, &str) -> Option<PathBuf>,
) -> Result<IngestStats, String> {
    let sessions = sessions.to_vec();
    let mut stats = IngestStats::default();
    for (sid, cwd) in sessions {
        let Some(path) = resolve_path(&cwd, &sid) else {
            stats.sessions_missing += 1;
            continue;
        };
        // 파일을 읽기 **전에** mtime 을 잡는다. 읽는 도중에 자라면 다음 번에 다시 읽어야 하는데,
        // 나중에 잡으면 그 성장분을 이미 봤다고 기록해 버려 영영 놓친다.
        let m = mtime_of(&path);

        // 세션 하나를 한 트랜잭션으로. 절반만 수집된 상태를 만들지 않는다.
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        ingest_session(&tx, &sid, &path, offset, parse_epoch, &mut stats)?;
        if m > 0 {
            tx.execute(
                "INSERT INTO ingest_state (session_id, src_mtime, ingested_at)
                 VALUES (?1, ?2, datetime('now'))
                 ON CONFLICT(session_id) DO UPDATE SET
                   src_mtime = excluded.src_mtime, ingested_at = excluded.ingested_at",
                params![sid, m],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(json: serde_json::Value) -> Ev {
        Ev {
            etype: json["type"].as_str().unwrap_or("").to_string(),
            uuid: json["uuid"].as_str().unwrap_or("").to_string(),
            turn_id: json.get("promptId").and_then(|v| v.as_str()).map(String::from),
            origin_kind: json.pointer("/origin/kind").and_then(|v| v.as_str()).map(String::from),
            is_meta: json.get("isMeta").and_then(|v| v.as_bool()) == Some(true),
            ts: json.get("timestamp").and_then(|v| v.as_str()).map(String::from),
            content: json.pointer("/message/content").cloned().unwrap_or(serde_json::Value::Null),
        }
    }

    /// 골든 테스트. 이 시나리오가 접히지 않으면 응답의 60% 가 사라진다.
    ///
    ///   프롬프트 → assistant → task-notification(새 promptId!) → assistant 추가작업
    ///
    /// task-notification 은 자기만의 promptId 를 받으므로(실측 191/191) 순진하게 짜면
    /// 별도 턴으로 갈라진다. 하나의 턴으로 접혀야 한다.
    #[test]
    fn task_notification_folds_into_the_owning_turn() {
        let evs = vec![
            ev(serde_json::json!({
                "type":"user","uuid":"u1","promptId":"T1","timestamp":"2026-07-12T00:00:00Z",
                "promptSource":"typed","origin":{"kind":"human"},
                "message":{"content":"작업을 해줘"}
            })),
            ev(serde_json::json!({
                "type":"assistant","uuid":"a1","timestamp":"2026-07-12T00:00:05Z",
                "message":{"content":[{"type":"text","text":"에이전트를 띄웁니다."}]}
            })),
            // ★ 새 promptId. 순진한 파서는 여기서 턴을 끊는다.
            ev(serde_json::json!({
                "type":"user","uuid":"u2","promptId":"T2","timestamp":"2026-07-12T00:01:00Z",
                "promptSource":"system","origin":{"kind":"task-notification"},
                "message":{"content":"<task-notification>에이전트 완료</task-notification>"}
            })),
            ev(serde_json::json!({
                "type":"assistant","uuid":"a2","timestamp":"2026-07-12T00:01:05Z",
                "message":{"content":[{"type":"text","text":"결과를 반영했습니다."}]}
            })),
        ];
        let rows = vec![Row {
            id: 100,
            prompt: "작업을 해줘".into(),
            created_at: "2026-07-12T00:00:00".into(),
            cc_turn_id: Some("T1".into()),
            msg_uuid: None,
        }];

        let owners = resolve_owners(&evs, &rows, 0, |_| None);
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].match_kind, "exact");

        let turns = build_turns(&evs, &owners);
        assert_eq!(turns.len(), 1, "턴이 갈라졌다");
        let (id, segs) = &turns[0];
        assert_eq!(*id, 100);

        let kinds: Vec<&str> = segs.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, vec!["text", "injected", "text"], "접기가 안 됐다");
        assert_eq!(segs[1].name.as_deref(), Some("task-notification"));

        // 첫 응답 뒤의 작업이 살아 있는가 — 실측상 이 뒷부분이 그 턴 내용의 60% 다.
        assert!(segs[2].body.contains("결과를 반영했습니다"));
    }

    /// task-notification 도 prompts 행을 만든다(382개). 그 행은 턴을 소유하지 않고,
    /// kind='system' 으로 표시되어 목록에서 숨겨진다. 내용은 위 턴에 접혀 있으므로
    /// 아무것도 잃지 않는다.
    #[test]
    fn task_notification_row_is_marked_system_and_owns_no_turn() {
        let evs = vec![
            ev(serde_json::json!({
                "type":"user","uuid":"u1","promptId":"T1","message":{"content":"진짜 프롬프트"}
            })),
            ev(serde_json::json!({
                "type":"user","uuid":"u2","promptId":"T2","origin":{"kind":"task-notification"},
                "message":{"content":"<task-notification>완료</task-notification>"}
            })),
            ev(serde_json::json!({
                "type":"assistant","uuid":"a1",
                "message":{"content":[{"type":"text","text":"이어서 작업"}]}
            })),
        ];
        let rows = vec![
            Row { id: 1, prompt: "진짜 프롬프트".into(), created_at: "x".into(),
                  cc_turn_id: Some("T1".into()), msg_uuid: None },
            Row { id: 2, prompt: "<task-notification>완료".into(), created_at: "x".into(),
                  cc_turn_id: Some("T2".into()), msg_uuid: None },
        ];

        let owners = resolve_owners(&evs, &rows, 0, |_| None);
        assert_eq!(owners.len(), 2);
        assert!(!owners[0].is_system);
        assert!(owners[1].is_system, "task-notification 행이 system 으로 분류되지 않았다");

        let turns = build_turns(&evs, &owners);
        assert_eq!(turns.len(), 1, "system 행이 턴을 소유했다");
        assert_eq!(turns[0].0, 1);
        // 알림의 본문도, 그 뒤 작업도 사람 턴 안에 있다.
        let bodies: String = turns[0].1.iter().map(|s| s.body.as_str()).collect();
        assert!(bodies.contains("완료"));
        assert!(bodies.contains("이어서 작업"));
    }

    /// 커스텀 슬래시커맨드는 UserPromptSubmit 을 발생시켜 DB 행을 만들지만, transcript 에는
    /// <command-name>…</command-name> 으로 확장되어 promptSource 가 없다.
    /// cc_turn_id 조인은 텍스트가 달라도 맞는다 — 이게 소유권 규칙의 요점이다.
    #[test]
    fn slash_command_matches_by_id_not_text() {
        let evs = vec![ev(serde_json::json!({
            "type":"user","uuid":"u1","promptId":"T9",
            "message":{"content":"<command-name>/memory-cleanup</command-name>"}
        }))];
        let rows = vec![Row {
            id: 7,
            prompt: "/memory-cleanup".into(), // DB 에는 원문이 들어 있다
            created_at: "x".into(),
            cc_turn_id: Some("T9".into()),
            msg_uuid: None,
        }];
        let owners = resolve_owners(&evs, &rows, 0, |_| None);
        assert_eq!(owners.len(), 1, "텍스트가 다르다고 놓쳤다");
        assert_eq!(owners[0].match_kind, "exact");
    }

    /// 두 번 수집해도 세그먼트가 늘어나지 않아야 한다 (병합 키가 (prompt_id, src_uuid, block_idx)).
    #[test]
    fn ingest_is_idempotent() {
        let evs = vec![
            ev(serde_json::json!({"type":"user","uuid":"u1","promptId":"T1","message":{"content":"p"}})),
            ev(serde_json::json!({"type":"assistant","uuid":"a1",
                "message":{"content":[{"type":"text","text":"응답"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}})),
        ];
        let rows = vec![Row { id: 1, prompt: "p".into(), created_at: "x".into(),
                              cc_turn_id: Some("T1".into()), msg_uuid: None }];
        let a = build_turns(&evs, &resolve_owners(&evs, &rows, 0, |_| None));
        let b = build_turns(&evs, &resolve_owners(&evs, &rows, 0, |_| None));
        assert_eq!(a[0].1.len(), b[0].1.len());
        let keys: Vec<(String, i64)> = a[0].1.iter().map(|s| (s.src_uuid.clone(), s.block_idx)).collect();
        let mut uniq = keys.clone();
        uniq.sort(); uniq.dedup();
        assert_eq!(keys.len(), uniq.len(), "병합 키가 중복된다 — UPSERT 가 세그먼트를 잃는다");
    }

    /// 훅의 prompt_id 가 한 턴 어긋났다면(가변 전역 `Pt.promptId` 를 훅 시점에 읽는다),
    /// Tier 0 는 **직전 턴의 응답을 완전한 확신을 갖고** 붙인다 — 퍼지 매처보다 엄격히 나쁘다.
    /// 텍스트 검사가 그 실패를 불가능하게 만든다.
    #[test]
    fn key_that_disagrees_with_text_is_not_trusted() {
        let evs = vec![ev(serde_json::json!({
            "type":"user","uuid":"u1","promptId":"T1",
            "message":{"content":"전혀 다른 프롬프트입니다 정말로"}
        }))];
        let rows = vec![Row {
            id: 1,
            prompt: "테스트를 고쳐줘 제발".into(), // 이벤트 텍스트와 겹치지 않는다
            created_at: "x".into(),
            cc_turn_id: Some("T1".into()), // 키는 맞다고 주장한다
            msg_uuid: None,
        }];
        let owners = resolve_owners(&evs, &rows, 0, |_| None);
        // 키만 믿고 'exact' 로 붙이면 안 된다. 폴백도 실패하면 아예 안 붙는다.
        assert!(
            owners.is_empty() || owners[0].match_kind == "conflict",
            "키와 텍스트가 어긋나는데 exact 로 붙였다"
        );
    }

    /// queued 프롬프트(작업 중 미리 입력)는 같은 promptId 를 공유할 수 있다
    /// (실측: 741개 중 9개). 두 DB 행이 같은 이벤트를 가리키면 하나는 빈 턴이 되고
    /// 다른 하나가 전부를 삼킨다. 순서대로 각자의 이벤트에 붙어야 한다.
    #[test]
    fn queued_prompts_sharing_a_turn_id_get_distinct_events() {
        let evs = vec![
            ev(serde_json::json!({"type":"user","uuid":"u1","promptId":"T1",
                "message":{"content":"첫번째 요청입니다"}})),
            ev(serde_json::json!({"type":"user","uuid":"u2","promptId":"T1",
                "message":{"content":"두번째 요청입니다"}})),
            ev(serde_json::json!({"type":"assistant","uuid":"a1",
                "message":{"content":[{"type":"text","text":"둘 다 처리"}]}})),
        ];
        let rows = vec![
            Row { id: 1, prompt: "첫번째 요청입니다".into(), created_at: "x".into(),
                  cc_turn_id: Some("T1".into()), msg_uuid: None },
            Row { id: 2, prompt: "두번째 요청입니다".into(), created_at: "x".into(),
                  cc_turn_id: Some("T1".into()), msg_uuid: None },
        ];
        let owners = resolve_owners(&evs, &rows, 0, |_| None);
        assert_eq!(owners.len(), 2, "queued 프롬프트 하나가 사라졌다");
        assert_ne!(owners[0].ev_idx, owners[1].ev_idx, "두 행이 같은 이벤트를 가리킨다");
    }

    /// 인터럽트로 끝난 턴도 그때까지의 응답을 갖는다. Stop 훅만으로 수집하면 이게 사라진다
    /// (실측: 94개 프롬프트 · 0.39MB).
    #[test]
    fn interrupted_turn_keeps_its_partial_response() {
        let evs = vec![
            ev(serde_json::json!({"type":"user","uuid":"u1","promptId":"T1","message":{"content":"해줘"}})),
            ev(serde_json::json!({"type":"assistant","uuid":"a1",
                "message":{"content":[{"type":"text","text":"작업 중…"}]}})),
            ev(serde_json::json!({"type":"user","uuid":"u2","promptId":"T1",
                "message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}})),
        ];
        let rows = vec![Row { id: 1, prompt: "해줘".into(), created_at: "x".into(),
                              cc_turn_id: Some("T1".into()), msg_uuid: None }];
        let turns = build_turns(&evs, &resolve_owners(&evs, &rows, 0, |_| None));
        let kinds: Vec<&str> = turns[0].1.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, vec!["text", "interrupt"]);
        assert!(turns[0].1[0].body.contains("작업 중"));
    }
}
