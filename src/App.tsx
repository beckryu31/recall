import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

type Prompt = {
  id: number;
  session_id: string | null;
  cwd: string | null;
  prompt: string;
  created_at: string;
  bookmarked: boolean;
  tags: string[];
  has_response: boolean;
};

type CwdGroup = {
  cwd: string | null;
  alias: string | null;
  count: number;
};

type TagGroup = {
  name: string;
  count: number;
};

type PromptResponse = {
  response: string;
  fetched_at: string;
  source: "saved" | "cache" | "jsonl";
};

type BatchFetchResult = {
  total: number;
  fetched: number;
  not_found: number;
  failed: number;
};

type PurgeCandidate = {
  id: number;
  prompt: string;
  created_at: string;
  cwd: string | null;
};

type PurgeScan = {
  scanned: number;
  recovered: number;
  protected: number;
  /// 세션 기록이 없어 판단 자체가 불가능했던 것. 삭제 후보가 아니다.
  unknown: number;
  candidates: PurgeCandidate[];
};

type DeleteResult = {
  deleted: number;
  backup_path: string;
};

const sourceLabel = (s: PromptResponse["source"]) => {
  if (s === "saved") return "저장됨";
  if (s === "cache") return "캐시됨";
  return "JSONL에서 가져옴";
};

/// 원형 되돌리기 화살표. 유니코드 ⟳ 는 폰트가 크기·두께를 정해버려 작게 나오므로 직접 그린다.
const RefetchIcon = ({ size = 16 }: { size?: number }) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth={2.6}
    strokeLinecap="round"
    strokeLinejoin="round"
    style={{ display: "block", flexShrink: 0 }}
  >
    <path d="M20.5 12a8.5 8.5 0 1 1-2.5-6" />
    <polyline points="20.5 3.2 20.5 9 14.7 9" />
  </svg>
);

const NULL_CWD_KEY = "__NULL__";
const ALL_KEY = "__ALL__";

const shortenCwd = (cwd: string) => {
  const parts = cwd.split("/").filter(Boolean);
  return parts.slice(-2).join("/") || cwd;
};

const displayName = (g: CwdGroup) => {
  if (g.alias && g.alias.trim()) return g.alias;
  if (!g.cwd) return "(경로 없음)";
  return shortenCwd(g.cwd);
};

export default function App() {
  const [cwds, setCwds] = useState<CwdGroup[]>([]);
  const [cwdFilter, setCwdFilter] = useState<string>(ALL_KEY);
  const [tagList, setTagList] = useState<TagGroup[]>([]);
  const [tagFilter, setTagFilter] = useState<string>("");
  const [prompts, setPrompts] = useState<Prompt[]>([]);
  const [selected, setSelected] = useState<Prompt | null>(null);
  const [draft, setDraft] = useState("");
  const [search, setSearch] = useState("");
  const [onlyBookmarked, setOnlyBookmarked] = useState(false);
  const [dateFrom, setDateFrom] = useState("");
  const [dateTo, setDateTo] = useState("");
  const [toast, setToast] = useState("");
  const [report, setReport] = useState("");
  const [error, setError] = useState("");
  const [editingCwd, setEditingCwd] = useState<string | null>(null);
  const [editValue, setEditValue] = useState("");
  const [tagInput, setTagInput] = useState("");
  const [tagInputVisible, setTagInputVisible] = useState(false);
  const [response, setResponse] = useState<PromptResponse | null>(null);
  const [responseDraft, setResponseDraft] = useState("");
  const [responseLoading, setResponseLoading] = useState(false);
  const [responseError, setResponseError] = useState("");
  const [responseEditing, setResponseEditing] = useState(false);
  const [batchLoading, setBatchLoading] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [purgeScanning, setPurgeScanning] = useState(false);
  const [purgeScan, setPurgeScan] = useState<PurgeScan | null>(null);
  const [purgeDeleting, setPurgeDeleting] = useState(false);
  const [lastBackup, setLastBackup] = useState("");
  const [appVersion, setAppVersion] = useState("");
  const [refetchAllLoading, setRefetchAllLoading] = useState(false);
  const [confirmRefetchAll, setConfirmRefetchAll] = useState(false);

  const loadCwds = async () => {
    try {
      const rows = await invoke<CwdGroup[]>("list_cwds");
      setCwds(rows);
    } catch (e) {
      setError(String(e));
    }
  };

  const loadTags = async () => {
    try {
      const rows = await invoke<TagGroup[]>("list_tags");
      setTagList(rows);
    } catch (e) {
      setError(String(e));
    }
  };

  const refreshAll = async () => {
    await Promise.all([loadCwds(), loadTags(), loadPrompts()]);
    showToast("새로고침됨");
  };

  const fetchRecentResponses = async () => {
    if (batchLoading) return;
    setBatchLoading(true);
    try {
      const r = await invoke<BatchFetchResult>("fetch_recent_responses");
      showReport(
        `최근 24시간 · 대상 ${r.total}개 중 ${r.fetched}개 응답을 가져왔습니다.` +
          (r.not_found ? ` 응답을 찾지 못함 ${r.not_found}개.` : "") +
          (r.failed ? ` 세션 기록이 없어 건너뜀 ${r.failed}개.` : "")
      );
      await loadPrompts();
      if (selected) await loadResponse(selected.id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBatchLoading(false);
    }
  };

  // 1단계: 응답이 없는 프롬프트를 훑어 JSONL 에서 되살릴 수 있는 것은 되살리고,
  // 끝내 응답을 찾지 못한 것만 삭제 후보로 받아온다. 이 단계에서는 아무것도 지우지 않는다.
  const scanUnanswered = async () => {
    if (purgeScanning) return;
    setPurgeScanning(true);
    try {
      const scan = await invoke<PurgeScan>("scan_unanswered_prompts");
      setPurgeScan(scan);
      if (scan.recovered > 0) {
        await loadPrompts();
        if (selected) await loadResponse(selected.id);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setPurgeScanning(false);
    }
  };

  // 2단계: 화면에서 확인한 후보만 실제로 삭제한다.
  const purgeCandidates = async () => {
    if (!purgeScan || purgeDeleting) return;
    const ids = purgeScan.candidates.map((c) => c.id);
    setPurgeDeleting(true);
    try {
      const r = await invoke<DeleteResult>("delete_prompts", { ids });
      setPurgeScan(null);
      if (selected && ids.includes(selected.id)) setSelected(null);
      await Promise.all([loadCwds(), loadTags(), loadPrompts()]);
      setLastBackup(r.backup_path);
      showReport(`${r.deleted}개를 삭제했습니다. 삭제 전 DB 백업이 저장되었습니다.`);
    } catch (e) {
      setError(String(e));
    } finally {
      setPurgeDeleting(false);
    }
  };

  // 저장된 응답을 전부 버리고 현재 매칭 로직으로 다시 추출한다.
  // 앱을 새 버전으로 올린 직후 한 번 돌리면 과거 기록까지 새 로직이 적용된다.
  const refetchAllResponses = async () => {
    if (refetchAllLoading) return;
    setConfirmRefetchAll(false);
    setRefetchAllLoading(true);
    try {
      const r = await invoke<BatchFetchResult>("refetch_all_responses");
      showReport(
        `전체 재추출 · 프롬프트 ${r.total}개 중 ${r.fetched}개를 현재 로직으로 다시 추출했습니다.` +
          (r.not_found
            ? ` 세션 기록에 응답이 없어 그대로 둔 것 ${r.not_found}개 (ESC로 취소한 턴 등).`
            : "") +
          (r.failed
            ? ` 세션 기록이 사라져 손대지 않은 것 ${r.failed}개 — 저장돼 있던 응답은 그대로 유지됩니다.`
            : "")
      );
      await loadPrompts();
      if (selected) await loadResponse(selected.id);
    } catch (e) {
      setError(String(e));
    } finally {
      setRefetchAllLoading(false);
    }
  };

  const loadPrompts = async () => {
    try {
      let cwdArg: string | null = null;
      if (cwdFilter !== ALL_KEY) cwdArg = cwdFilter;
      const rows = await invoke<Prompt[]>("list_prompts", {
        limit: 200,
        offset: 0,
        search: search || null,
        cwd: cwdArg,
        onlyBookmarked,
        tag: tagFilter || null,
        dateFrom: dateFrom || null,
        dateTo: dateTo || null,
      });
      setPrompts(rows);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    loadCwds();
    loadTags();
    getVersion().then(setAppVersion).catch(() => {});
  }, []);

  // 되돌릴 수 없는 작업의 확인 상태는 ESC 로도 빠져나올 수 있어야 한다.
  // 실행이 이미 시작됐다면 취소하지 않는다.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (!refetchAllLoading) setConfirmRefetchAll(false);
      if (!purgeDeleting) setPurgeScan(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [refetchAllLoading, purgeDeleting]);

  useEffect(() => {
    loadPrompts();
  }, [search, cwdFilter, onlyBookmarked, tagFilter, dateFrom, dateTo]);

  const loadResponse = async (promptId: number, refresh = false) => {
    setResponseLoading(true);
    setResponseError("");
    setResponse(null);
    setResponseDraft("");
    try {
      const r = await invoke<PromptResponse | null>("get_response", {
        promptId,
        refresh,
      });
      setResponse(r);
      setResponseDraft(r?.response ?? "");
      if (!r) setResponseError("이 프롬프트에 해당하는 응답을 찾을 수 없습니다");
      else markHasResponse(promptId, true);
    } catch (e) {
      setResponseError(String(e));
    } finally {
      setResponseLoading(false);
    }
  };

  const markHasResponse = (promptId: number, has: boolean) => {
    setPrompts((prev) =>
      prev.map((x) => (x.id === promptId ? { ...x, has_response: has } : x))
    );
    setSelected((prev) =>
      prev && prev.id === promptId ? { ...prev, has_response: has } : prev
    );
  };

  const saveResponseToPrompt = async () => {
    if (!selected) return;
    await invoke("save_response", {
      promptId: selected.id,
      response: responseDraft,
    });
    showToast("응답 저장됨");
    setResponse({
      response: responseDraft,
      fetched_at: new Date().toISOString(),
      source: "saved",
    });
    markHasResponse(selected.id, !!responseDraft);
  };

  const select = (p: Prompt) => {
    setSelected(p);
    setDraft(p.prompt);
    setTagInput("");
    setTagInputVisible(false);
    setResponseEditing(false);
    setConfirmingDelete(false);
    loadResponse(p.id);
  };

  const refreshResponse = () => {
    if (selected) loadResponse(selected.id, true);
  };

  const copyResponse = async () => {
    if (!responseDraft) return;
    await writeText(responseDraft);
    showToast("응답 복사됨");
  };

  // 짧은 확인용. "저장됨" 같은 스쳐도 되는 메시지에만 쓴다.
  const showToast = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(""), 2500);
  };

  // 배치 작업 결과는 읽고 판단해야 하는 데이터라 자동으로 사라지면 안 된다.
  // 직접 닫을 때까지 남는 배너로 보여준다.
  const showReport = (msg: string) => setReport(msg);

  const save = async () => {
    if (!selected) return;
    await invoke("update_prompt", { id: selected.id, prompt: draft });
    showToast("저장됨");
    loadPrompts();
  };

  const copy = async () => {
    await writeText(draft);
    showToast("복사됨");
  };

  const addTagToSelected = async () => {
    if (!selected) return;
    const name = tagInput.trim();
    if (!name) return;
    if (selected.tags.includes(name)) {
      setTagInput("");
      setTagInputVisible(false);
      return;
    }
    await invoke("add_prompt_tag", { promptId: selected.id, name });
    const nextTags = [...selected.tags, name];
    const updated = { ...selected, tags: nextTags };
    setSelected(updated);
    setPrompts((prev) =>
      prev.map((x) => (x.id === selected.id ? { ...x, tags: nextTags } : x))
    );
    setTagInput("");
    setTagInputVisible(false);
    loadTags();
    showToast("태그 추가됨");
  };

  const removeTagFromSelected = async (name: string) => {
    if (!selected) return;
    await invoke("remove_prompt_tag", { promptId: selected.id, name });
    const nextTags = selected.tags.filter((t) => t !== name);
    const updated = { ...selected, tags: nextTags };
    setSelected(updated);
    setPrompts((prev) =>
      prev.map((x) => (x.id === selected.id ? { ...x, tags: nextTags } : x))
    );
    loadTags();
    if (tagFilter === name) {
      loadPrompts();
    }
    showToast("태그 삭제됨");
  };

  const toggleBookmark = async (p: Prompt) => {
    const next = !p.bookmarked;
    await invoke("toggle_bookmark", { promptId: p.id, bookmarked: next });
    setPrompts((prev) =>
      prev.map((x) => (x.id === p.id ? { ...x, bookmarked: next } : x))
    );
    if (selected?.id === p.id) {
      setSelected({ ...p, bookmarked: next });
    }
    if (onlyBookmarked && !next) {
      setPrompts((prev) => prev.filter((x) => x.id !== p.id));
    }
    showToast(next ? "북마크됨" : "북마크 해제됨");
  };

  const remove = async () => {
    if (!selected) return;
    try {
      await invoke("delete_prompt", { id: selected.id });
      setConfirmingDelete(false);
      setSelected(null);
      setDraft("");
      loadPrompts();
      loadCwds();
      showToast("삭제됨");
    } catch (e) {
      setError(String(e));
      showToast("삭제 실패");
    }
  };

  const beginEdit = (g: CwdGroup) => {
    if (!g.cwd) {
      showToast("경로 없음 항목은 별칭 지정 불가");
      return;
    }
    setEditingCwd(g.cwd);
    setEditValue(g.alias ?? "");
  };

  const cancelEdit = () => {
    setEditingCwd(null);
    setEditValue("");
  };

  const saveAlias = async () => {
    if (!editingCwd) return;
    await invoke("set_cwd_alias", { cwd: editingCwd, alias: editValue });
    showToast(editValue.trim() ? "별칭 저장됨" : "별칭 삭제됨");
    setEditingCwd(null);
    setEditValue("");
    loadCwds();
  };

  const totalCount = cwds.reduce((s, g) => s + g.count, 0);

  const cwdItemStyle = (active: boolean): React.CSSProperties => ({
    padding: "10px 14px",
    borderBottom: "1px solid #f0f0f0",
    background: active ? "#eef" : "transparent",
    cursor: "pointer",
    display: "flex",
    alignItems: "center",
    gap: 8,
    fontSize: 13,
  });

  return (
    <div
      style={{
        display: "flex",
        height: "100vh",
        fontFamily: "system-ui",
        padding: 16,
        gap: 16,
        boxSizing: "border-box",
        background: "#fafafa",
      }}
    >
      <aside
        style={{
          width: 240,
          border: "1px solid #e5e5e5",
          borderRadius: 8,
          background: "#fff",
          display: "flex",
          flexDirection: "column",
          overflow: "hidden",
        }}
      >
        <div
          style={{
            flex: 1,
            display: "flex",
            flexDirection: "column",
            minHeight: 0,
            borderBottom: "1px solid #eee",
          }}
        >
        <div
          style={{
            padding: "12px 14px",
            borderBottom: "1px solid #eee",
            fontSize: 12,
            fontWeight: 600,
            color: "#555",
            letterSpacing: 0.3,
          }}
        >
          경로 (CWD)
        </div>
        <ul
          style={{
            listStyle: "none",
            margin: 0,
            padding: 0,
            overflowY: "auto",
            flex: 1,
          }}
        >
          <li
            onClick={() => setCwdFilter(ALL_KEY)}
            style={cwdItemStyle(cwdFilter === ALL_KEY)}
          >
            <span style={{ flex: 1, fontWeight: 500 }}>전체</span>
            <span style={{ fontSize: 11, color: "#888" }}>{totalCount}</span>
          </li>
          {cwds.map((g, i) => {
            const key = g.cwd ?? NULL_CWD_KEY;
            const active = cwdFilter === key;
            const isEditing = editingCwd !== null && editingCwd === g.cwd;
            if (isEditing) {
              return (
                <li
                  key={i}
                  style={{
                    padding: "10px 14px",
                    borderBottom: "1px solid #f0f0f0",
                    background: "#fffbe6",
                    display: "flex",
                    flexDirection: "column",
                    gap: 6,
                  }}
                >
                  <div
                    style={{
                      fontSize: 11,
                      color: "#888",
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                    title={g.cwd ?? ""}
                  >
                    {g.cwd}
                  </div>
                  <input
                    autoFocus
                    value={editValue}
                    placeholder="별칭 (비우면 삭제)"
                    onChange={(e) => setEditValue(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") saveAlias();
                      if (e.key === "Escape") cancelEdit();
                    }}
                    style={{
                      padding: 6,
                      border: "1px solid #ddd",
                      borderRadius: 4,
                      fontSize: 12,
                    }}
                  />
                  <div style={{ display: "flex", gap: 6, justifyContent: "flex-end" }}>
                    <button onClick={cancelEdit} style={{ fontSize: 11, padding: "2px 8px" }}>
                      취소
                    </button>
                    <button onClick={saveAlias} style={{ fontSize: 11, padding: "2px 8px" }}>
                      저장
                    </button>
                  </div>
                </li>
              );
            }
            return (
              <li
                key={i}
                onClick={() => setCwdFilter(key)}
                style={cwdItemStyle(active)}
                title={g.cwd ?? "(경로 없음)"}
              >
                <span
                  style={{
                    flex: 1,
                    whiteSpace: "nowrap",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                  }}
                >
                  {displayName(g)}
                </span>
                <span style={{ fontSize: 11, color: "#888" }}>{g.count}</span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    beginEdit(g);
                  }}
                  style={{
                    border: "none",
                    background: "transparent",
                    cursor: "pointer",
                    fontSize: 12,
                    color: "#888",
                    padding: "2px 4px",
                  }}
                  title="별칭 편집"
                >
                  ✎
                </button>
              </li>
            );
          })}
        </ul>
        </div>

        <div
          style={{
            flex: 1,
            display: "flex",
            flexDirection: "column",
            minHeight: 0,
          }}
        >
          <div
            style={{
              padding: "12px 14px",
              borderBottom: "1px solid #eee",
              fontSize: 12,
              fontWeight: 600,
              color: "#555",
              letterSpacing: 0.3,
            }}
          >
            태그
          </div>
          <ul
            style={{
              listStyle: "none",
              margin: 0,
              padding: 0,
              overflowY: "auto",
              flex: 1,
            }}
          >
            <li
              onClick={() => setTagFilter("")}
              style={cwdItemStyle(tagFilter === "")}
            >
              <span style={{ flex: 1, fontWeight: 500 }}>전체</span>
              <span style={{ fontSize: 11, color: "#888" }}>
                {tagList.reduce((s, t) => s + t.count, 0)}
              </span>
            </li>
            {tagList.map((t) => (
              <li
                key={t.name}
                onClick={() => setTagFilter(t.name)}
                style={cwdItemStyle(tagFilter === t.name)}
                title={t.name}
              >
                <span
                  style={{
                    flex: 1,
                    whiteSpace: "nowrap",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                  }}
                >
                  #{t.name}
                </span>
                <span style={{ fontSize: 11, color: "#888" }}>{t.count}</span>
              </li>
            ))}
            {tagList.length === 0 && (
              <li style={{ padding: 12, color: "#aaa", fontSize: 12 }}>
                태그 없음
              </li>
            )}
          </ul>
        </div>
        <div
          style={{
            borderTop: "1px solid #eee",
            padding: "8px 12px",
            fontSize: 11,
            color: "#aaa",
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
          }}
        >
          <span>Recall</span>
          <span>{appVersion ? `v${appVersion}` : ""}</span>
        </div>
      </aside>

      <aside
        style={{
          width: 360,
          border: "1px solid #e5e5e5",
          borderRadius: 8,
          background: "#fff",
          display: "flex",
          flexDirection: "column",
          overflow: "hidden",
        }}
      >
        <div
          style={{
            padding: 12,
            borderBottom: "1px solid #eee",
            display: "flex",
            gap: 8,
            alignItems: "center",
            flexWrap: "wrap",
          }}
        >
          {/* 첫 줄은 필터 계열(검색 + 북마크)이 차지하고, 동작 버튼들은 아래 줄에 놓는다.
              목록 패널이 좁아 전부 한 줄에 두면 검색창이 찌그러진다. */}
          <div
            style={{
              display: "flex",
              gap: 8,
              alignItems: "center",
              flex: "1 1 100%",
            }}
          >
          <div style={{ position: "relative", flex: 1 }}>
            <input
              placeholder="검색..."
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              style={{
                width: "100%",
                padding: 10,
                paddingRight: search ? 32 : 10,
                border: "1px solid #ddd",
                borderRadius: 6,
                boxSizing: "border-box",
                fontSize: 13,
              }}
            />
            {search && (
              <button
                onClick={() => setSearch("")}
                title="검색어 지우기"
                style={{
                  position: "absolute",
                  right: 4,
                  top: "50%",
                  transform: "translateY(-50%)",
                  width: 22,
                  height: 22,
                  border: "none",
                  borderRadius: "50%",
                  background: "#eee",
                  color: "#666",
                  cursor: "pointer",
                  fontSize: 13,
                  lineHeight: 1,
                  padding: 0,
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                }}
              >
                ×
              </button>
            )}
          </div>
            <button
              onClick={() => setOnlyBookmarked((v) => !v)}
              title={onlyBookmarked ? "전체 보기" : "북마크만 보기"}
              style={{
                padding: "8px 12px",
                border: "1px solid #ddd",
                borderRadius: 6,
                background: onlyBookmarked ? "#fff3cd" : "#fff",
                cursor: "pointer",
                fontSize: 14,
                color: onlyBookmarked ? "#b58900" : "#888",
                flexShrink: 0,
              }}
            >
              {onlyBookmarked ? "★" : "☆"}
            </button>
          </div>
          <button
            onClick={refreshAll}
            title="새로고침"
            style={{
              padding: "8px 12px",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: "#fff",
              cursor: "pointer",
              fontSize: 14,
            }}
          >
            ↻
          </button>
          <button
            onClick={fetchRecentResponses}
            disabled={batchLoading}
            title="최근 24시간 내 모든 프롬프트의 응답을 가져와 갱신"
            style={{
              padding: "8px 10px",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: batchLoading ? "#eef" : "#fff",
              cursor: batchLoading ? "default" : "pointer",
              fontSize: 12,
              whiteSpace: "nowrap",
              color: batchLoading ? "#888" : "#2556a0",
            }}
          >
            {batchLoading ? "가져오는 중…" : "⤓ 24h"}
          </button>
          {confirmRefetchAll ? (
            <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
              <button
                onClick={refetchAllResponses}
                disabled={refetchAllLoading}
                title="모든 프롬프트의 응답을 세션 기록에서 다시 읽어 갱신합니다. 기록이 없는 응답은 그대로 둡니다."
                style={{
                  padding: "8px 10px",
                  border: "1px solid #2556a0",
                  borderRadius: 6,
                  background: "#2556a0",
                  color: "#fff",
                  cursor: "pointer",
                  fontSize: 12,
                  whiteSpace: "nowrap",
                  display: "flex",
                  alignItems: "center",
                  gap: 5,
                }}
              >
                <RefetchIcon size={16} />
                모두 다시 가져오기
              </button>
              <button
                onClick={() => setConfirmRefetchAll(false)}
                disabled={refetchAllLoading}
                style={{
                  padding: "8px 10px",
                  border: "1px solid #ddd",
                  borderRadius: 6,
                  background: "#fff",
                  cursor: "pointer",
                  fontSize: 12,
                  whiteSpace: "nowrap",
                  color: "#666",
                }}
              >
                취소
              </button>
            </div>
          ) : (
            <button
              onClick={() => setConfirmRefetchAll(true)}
              disabled={refetchAllLoading}
              title="모든 프롬프트의 응답을 세션 기록에서 다시 읽어 갱신합니다. 앱을 새 버전으로 올린 뒤 한 번 실행하면 과거 기록에도 최신 로직이 적용됩니다. 기록이 사라진 응답은 그대로 유지됩니다."
              style={{
                padding: "8px 10px",
                border: "1px solid #ddd",
                borderRadius: 6,
                background: refetchAllLoading ? "#eef" : "#fff",
                cursor: refetchAllLoading ? "default" : "pointer",
                fontSize: 12,
                whiteSpace: "nowrap",
                color: refetchAllLoading ? "#888" : "#2556a0",
                display: "flex",
                alignItems: "center",
                gap: 5,
              }}
            >
              <RefetchIcon size={16} />
              {refetchAllLoading ? "재추출 중…" : "전체"}
            </button>
          )}
          <button
            onClick={scanUnanswered}
            disabled={purgeScanning}
            title="응답을 되살릴 수 없는 프롬프트를 찾아 한꺼번에 정리"
            style={{
              padding: "8px 10px",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: purgeScanning ? "#eef" : "#fff",
              cursor: purgeScanning ? "default" : "pointer",
              fontSize: 12,
              whiteSpace: "nowrap",
              color: purgeScanning ? "#888" : "#a33",
            }}
          >
            {purgeScanning ? "검사 중…" : "🧹 정리"}
          </button>
        </div>
        <div
          style={{
            padding: "8px 12px",
            borderBottom: "1px solid #eee",
            display: "flex",
            gap: 6,
            alignItems: "center",
            fontSize: 12,
            color: "#666",
            background: "#fafafa",
          }}
        >
          <input
            type="date"
            value={dateFrom}
            onChange={(e) => setDateFrom(e.target.value)}
            title="시작일"
            style={{
              flex: 1,
              padding: "6px 8px",
              border: "1px solid #ddd",
              borderRadius: 4,
              fontSize: 12,
              minWidth: 0,
            }}
          />
          <span style={{ color: "#aaa" }}>~</span>
          <input
            type="date"
            value={dateTo}
            onChange={(e) => setDateTo(e.target.value)}
            title="종료일"
            style={{
              flex: 1,
              padding: "6px 8px",
              border: "1px solid #ddd",
              borderRadius: 4,
              fontSize: 12,
              minWidth: 0,
            }}
          />
          {(dateFrom || dateTo) && (
            <button
              onClick={() => {
                setDateFrom("");
                setDateTo("");
              }}
              title="날짜 초기화"
              style={{
                border: "none",
                background: "transparent",
                cursor: "pointer",
                fontSize: 14,
                color: "#888",
                padding: "2px 6px",
              }}
            >
              ×
            </button>
          )}
        </div>
        {error && (
          <div
            style={{
              margin: 12,
              padding: 10,
              background: "#fee",
              color: "#900",
              fontSize: 12,
              borderRadius: 6,
            }}
          >
            {error}
          </div>
        )}
        <ul
          style={{
            listStyle: "none",
            margin: 0,
            padding: 0,
            overflowY: "auto",
            flex: 1,
          }}
        >
          {prompts.map((p) => (
            <li
              key={p.id}
              onClick={() => select(p)}
              style={{
                padding: "12px 14px",
                borderBottom: "1px solid #f0f0f0",
                background: selected?.id === p.id ? "#eef" : "transparent",
                cursor: "pointer",
                display: "flex",
                gap: 8,
                alignItems: "flex-start",
              }}
            >
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  toggleBookmark(p);
                }}
                title={p.bookmarked ? "북마크 해제" : "북마크"}
                style={{
                  border: "none",
                  background: "transparent",
                  cursor: "pointer",
                  fontSize: 16,
                  lineHeight: 1,
                  padding: 0,
                  color: p.bookmarked ? "#f5a623" : "#ccc",
                  flexShrink: 0,
                  marginTop: 1,
                }}
              >
                {p.bookmarked ? "★" : "☆"}
              </button>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div
                  style={{
                    fontSize: 11,
                    color: "#888",
                    marginBottom: 4,
                    display: "flex",
                    alignItems: "center",
                    gap: 6,
                  }}
                >
                  <span>{p.created_at}</span>
                  {!p.has_response && (
                    <span
                      title="응답 저장 안 됨"
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 3,
                        padding: "1px 6px",
                        background: "#fff1f0",
                        color: "#c0392b",
                        border: "1px solid #f5c6c0",
                        borderRadius: 8,
                        fontSize: 10,
                        lineHeight: 1.4,
                      }}
                    >
                      <span
                        style={{
                          width: 5,
                          height: 5,
                          borderRadius: "50%",
                          background: "#c0392b",
                          display: "inline-block",
                        }}
                      />
                      응답 없음
                    </span>
                  )}
                </div>
                <div
                  style={{
                    whiteSpace: "nowrap",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    fontSize: 13,
                  }}
                >
                  {p.prompt.slice(0, 80)}
                </div>
              </div>
            </li>
          ))}
          {prompts.length === 0 && (
            <li style={{ padding: 16, color: "#888", fontSize: 13 }}>
              표시할 프롬프트가 없습니다
            </li>
          )}
        </ul>
      </aside>

      <main
        style={{
          flex: 1,
          display: "flex",
          flexDirection: "column",
          padding: 20,
          border: "1px solid #e5e5e5",
          borderRadius: 8,
          background: "#fff",
          minWidth: 0,
        }}
      >
        {selected ? (
          <>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                marginBottom: 12,
              }}
            >
              <button
                onClick={() => toggleBookmark(selected)}
                title={selected.bookmarked ? "북마크 해제" : "북마크"}
                style={{
                  border: "none",
                  background: "transparent",
                  cursor: "pointer",
                  fontSize: 18,
                  lineHeight: 1,
                  padding: 0,
                  color: selected.bookmarked ? "#f5a623" : "#ccc",
                  flexShrink: 0,
                }}
              >
                {selected.bookmarked ? "★" : "☆"}
              </button>
              <div
                style={{
                  fontSize: 12,
                  color: "#666",
                  wordBreak: "break-all",
                  flex: 1,
                  minWidth: 0,
                }}
              >
                #{selected.id} · {selected.created_at} · {selected.cwd ?? ""}
              </div>
            </div>
            <div
              style={{
                display: "flex",
                flexWrap: "wrap",
                gap: 6,
                marginBottom: 12,
                alignItems: "center",
              }}
            >
              {selected.tags.map((t) => (
                <span
                  key={t}
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: 4,
                    padding: "3px 8px",
                    background: "#eef4ff",
                    color: "#2556a0",
                    border: "1px solid #d0dcf0",
                    borderRadius: 12,
                    fontSize: 11,
                  }}
                >
                  #{t}
                  <button
                    onClick={() => removeTagFromSelected(t)}
                    title="태그 삭제"
                    style={{
                      border: "none",
                      background: "transparent",
                      cursor: "pointer",
                      color: "#6987bc",
                      padding: 0,
                      fontSize: 12,
                      lineHeight: 1,
                    }}
                  >
                    ×
                  </button>
                </span>
              ))}
              {tagInputVisible ? (
                <input
                  autoFocus
                  value={tagInput}
                  placeholder="태그 이름"
                  onChange={(e) => setTagInput(e.target.value)}
                  onBlur={() => {
                    if (!tagInput.trim()) {
                      setTagInputVisible(false);
                    }
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") addTagToSelected();
                    if (e.key === "Escape") {
                      setTagInput("");
                      setTagInputVisible(false);
                    }
                  }}
                  style={{
                    padding: "3px 8px",
                    fontSize: 11,
                    border: "1px solid #bbb",
                    borderRadius: 12,
                    outline: "none",
                    minWidth: 100,
                  }}
                />
              ) : (
                <button
                  onClick={() => setTagInputVisible(true)}
                  title="태그 추가"
                  style={{
                    border: "1px dashed #bbb",
                    background: "transparent",
                    cursor: "pointer",
                    color: "#888",
                    padding: "3px 8px",
                    borderRadius: 12,
                    fontSize: 11,
                  }}
                >
                  + 태그
                </button>
              )}
            </div>
            <div
              style={{
                display: "flex",
                flexDirection: "column",
                flex: 1,
                minHeight: 0,
                gap: 16,
              }}
            >
              <section
                style={{
                  display: "flex",
                  flexDirection: "column",
                  flex: 1,
                  minHeight: 0,
                }}
              >
                <div style={{ fontSize: 12, fontWeight: 600, color: "#555", marginBottom: 6 }}>
                  프롬프트
                </div>
                <textarea
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  style={{
                    flex: 1,
                    fontFamily: "ui-monospace, monospace",
                    fontSize: 14,
                    padding: 14,
                    border: "1px solid #e0e0e0",
                    borderRadius: 6,
                    resize: "none",
                  }}
                />
                <div style={{ display: "flex", gap: 10, marginTop: 10, alignItems: "center" }}>
                  <button onClick={copy}>복사</button>
                  <button onClick={save}>저장</button>
                  {confirmingDelete ? (
                    <div
                      style={{
                        marginLeft: "auto",
                        display: "flex",
                        gap: 8,
                        alignItems: "center",
                      }}
                    >
                      <span style={{ fontSize: 12, color: "#c00" }}>삭제할까요?</span>
                      <button
                        onClick={remove}
                        style={{
                          color: "#fff",
                          background: "#c00",
                          border: "none",
                          borderRadius: 4,
                          padding: "4px 12px",
                          cursor: "pointer",
                        }}
                      >
                        삭제
                      </button>
                      <button onClick={() => setConfirmingDelete(false)}>취소</button>
                    </div>
                  ) : (
                    <button
                      onClick={() => setConfirmingDelete(true)}
                      style={{ marginLeft: "auto", color: "#c00" }}
                    >
                      삭제
                    </button>
                  )}
                </div>
              </section>

              <section
                style={{
                  display: "flex",
                  flexDirection: "column",
                  flex: 3,
                  minHeight: 0,
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    marginBottom: 6,
                  }}
                >
                  <div style={{ fontSize: 12, fontWeight: 600, color: "#555" }}>
                    응답
                  </div>
                  {response && (
                    <span style={{ fontSize: 11, color: "#888" }}>
                      ({sourceLabel(response.source)}) · {response.fetched_at}
                    </span>
                  )}
                  <button
                    onClick={() => setResponseEditing((v) => !v)}
                    disabled={responseLoading || !!responseError}
                    style={{
                      marginLeft: "auto",
                      fontSize: 11,
                      padding: "3px 10px",
                      border: "1px solid #ddd",
                      borderRadius: 4,
                      background: responseEditing ? "#fff3cd" : "#fff",
                      cursor: "pointer",
                      color: responseEditing ? "#b58900" : "#555",
                    }}
                  >
                    {responseEditing ? "보기" : "편집"}
                  </button>
                </div>
                {responseLoading ? (
                  <div
                    style={{
                      flex: 1,
                      padding: 14,
                      border: "1px solid #e0e0e0",
                      borderRadius: 6,
                      background: "#fafafa",
                      color: "#888",
                    }}
                  >
                    로딩 중...
                  </div>
                ) : responseError ? (
                  <div
                    style={{
                      flex: 1,
                      padding: 14,
                      border: "1px solid #e0e0e0",
                      borderRadius: 6,
                      background: "#fff5f5",
                      color: "#900",
                      fontSize: 13,
                      whiteSpace: "pre-wrap",
                    }}
                  >
                    {responseError}
                  </div>
                ) : responseEditing ? (
                  <textarea
                    value={responseDraft}
                    onChange={(e) => setResponseDraft(e.target.value)}
                    style={{
                      flex: 1,
                      fontFamily: "ui-monospace, monospace",
                      fontSize: 13,
                      padding: 14,
                      border: "1px solid #e0e0e0",
                      borderRadius: 6,
                      resize: "none",
                      background: "#fafafa",
                    }}
                  />
                ) : (
                  <div
                    className="markdown-body"
                    style={{
                      flex: 1,
                      padding: "14px 18px",
                      border: "1px solid #e0e0e0",
                      borderRadius: 6,
                      background: "#fff",
                      overflowY: "auto",
                      fontSize: 13,
                      lineHeight: 1.6,
                      color: "#222",
                    }}
                  >
                    {responseDraft ? (
                      <ReactMarkdown remarkPlugins={[remarkGfm]}>
                        {responseDraft}
                      </ReactMarkdown>
                    ) : (
                      <div style={{ color: "#aaa" }}>(응답 없음)</div>
                    )}
                  </div>
                )}
                <div style={{ display: "flex", gap: 10, marginTop: 10 }}>
                  <button onClick={copyResponse} disabled={!responseDraft}>
                    복사
                  </button>
                  <button
                    onClick={saveResponseToPrompt}
                    disabled={!responseDraft || responseLoading}
                  >
                    저장
                  </button>
                  <button
                    onClick={refreshResponse}
                    disabled={responseLoading}
                    style={{ marginLeft: "auto" }}
                  >
                    ↻ 다시 가져오기
                  </button>
                </div>
              </section>
            </div>
          </>
        ) : (
          <div style={{ color: "#888", margin: "auto" }}>
            왼쪽에서 프롬프트를 선택하세요
          </div>
        )}
        {toast && (
          <div
            style={{
              position: "fixed",
              // 결과 배너가 떠 있으면 그 위로 밀어 겹치지 않게 한다.
              bottom: report ? 110 : 28,
              right: 28,
              background: "#333",
              color: "#fff",
              padding: "10px 16px",
              borderRadius: 6,
              fontSize: 13,
            }}
          >
            {toast}
          </div>
        )}
      </main>

      {report && (
        <div
          style={{
            position: "fixed",
            bottom: 28,
            right: 28,
            maxWidth: 480,
            background: "#1f2937",
            color: "#f3f4f6",
            borderRadius: 10,
            padding: "14px 16px",
            fontSize: 13,
            lineHeight: 1.6,
            display: "flex",
            gap: 12,
            alignItems: "flex-start",
            boxShadow: "0 8px 28px rgba(0,0,0,0.28)",
            zIndex: 95,
          }}
        >
          <div style={{ flex: 1 }}>{report}</div>
          <button
            onClick={() => setReport("")}
            style={{
              background: "transparent",
              border: "1px solid #4b5563",
              color: "#d1d5db",
              borderRadius: 6,
              padding: "3px 9px",
              fontSize: 12,
              cursor: "pointer",
              flexShrink: 0,
            }}
          >
            닫기
          </button>
        </div>
      )}

      {lastBackup && (
        <div
          style={{
            position: "fixed",
            bottom: 28,
            left: 28,
            maxWidth: 460,
            background: "#f2faf5",
            border: "1px solid #cfe8d8",
            borderRadius: 8,
            padding: "10px 12px",
            fontSize: 12,
            color: "#2a6",
            display: "flex",
            gap: 10,
            alignItems: "center",
            zIndex: 90,
          }}
        >
          <div style={{ flex: 1, wordBreak: "break-all" }}>
            삭제 전 백업이 저장되었습니다
            <div style={{ color: "#555", marginTop: 2 }}>{lastBackup}</div>
          </div>
          <button
            onClick={async () => {
              await writeText(lastBackup);
              showToast("경로 복사됨");
            }}
            style={{ fontSize: 11, padding: "4px 8px" }}
          >
            경로 복사
          </button>
          <button
            onClick={() => setLastBackup("")}
            style={{ fontSize: 11, padding: "4px 8px" }}
          >
            닫기
          </button>
        </div>
      )}

      {purgeScan && (
        <div
          onClick={() => !purgeDeleting && setPurgeScan(null)}
          style={{
            position: "fixed",
            inset: 0,
            background: "rgba(0,0,0,0.35)",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            zIndex: 100,
          }}
        >
          <div
            onClick={(e) => e.stopPropagation()}
            style={{
              width: 560,
              maxHeight: "76vh",
              background: "#fff",
              borderRadius: 10,
              padding: 20,
              display: "flex",
              flexDirection: "column",
              gap: 12,
              boxShadow: "0 12px 40px rgba(0,0,0,0.25)",
            }}
          >
            <div style={{ fontSize: 15, fontWeight: 600 }}>
              응답 없는 프롬프트 정리
            </div>

            <div style={{ fontSize: 13, color: "#555", lineHeight: 1.7 }}>
              48시간이 지났고 응답이 비어 있던 <b>{purgeScan.scanned}개</b>를
              검사했습니다.
              <br />
              그중 <b style={{ color: "#2556a0" }}>
                {purgeScan.recovered}개
              </b>{" "}
              는 세션 기록에서 응답을 되살렸습니다.
              {purgeScan.unknown > 0 && (
                <>
                  <br />
                  세션 기록이 없어 <b>{purgeScan.unknown}개</b>는 판단할 수
                  없었습니다 — <b>삭제 대상이 아닙니다.</b>
                </>
              )}
              {purgeScan.protected > 0 && (
                <>
                  <br />
                  북마크·태그가 달린 <b>{purgeScan.protected}개</b>는 검사에서
                  제외했습니다.
                </>
              )}
            </div>

            {purgeScan.candidates.length === 0 ? (
              <div
                style={{
                  padding: "16px 0",
                  fontSize: 13,
                  color: "#2a7",
                  textAlign: "center",
                }}
              >
                지울 프롬프트가 없습니다.
              </div>
            ) : (
              <>
                <div style={{ fontSize: 13, color: "#a33" }}>
                  아래 <b>{purgeScan.candidates.length}개</b>는 세션 기록을 찾아
                  확인했지만 그 턴에 응답이 없었습니다. 삭제하면 되돌릴 수
                  없습니다.
                </div>
                <div
                  style={{
                    fontSize: 12,
                    color: "#2a6",
                    background: "#f2faf5",
                    border: "1px solid #d6ecdd",
                    borderRadius: 6,
                    padding: "6px 10px",
                  }}
                >
                  삭제 직전에 DB 전체가 <code>~/.claude/recall-backups/</code> 에
                  자동 백업됩니다. 되돌리려면 그 파일을{" "}
                  <code>~/.claude/prompts.db</code> 로 복사하세요.
                </div>
                <div
                  style={{
                    flex: 1,
                    overflowY: "auto",
                    border: "1px solid #eee",
                    borderRadius: 6,
                  }}
                >
                  {purgeScan.candidates.map((c) => (
                    <div
                      key={c.id}
                      style={{
                        padding: "8px 10px",
                        borderBottom: "1px solid #f2f2f2",
                        fontSize: 12,
                      }}
                    >
                      <div style={{ color: "#999", fontSize: 11 }}>
                        {c.created_at}
                        {c.cwd ? ` · ${shortenCwd(c.cwd)}` : ""}
                      </div>
                      <div
                        style={{
                          whiteSpace: "nowrap",
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          color: "#333",
                        }}
                      >
                        {c.prompt.replace(/\s+/g, " ").slice(0, 90)}
                      </div>
                    </div>
                  ))}
                </div>
              </>
            )}

            <div
              style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}
            >
              <button
                onClick={() => setPurgeScan(null)}
                disabled={purgeDeleting}
              >
                닫기
              </button>
              {purgeScan.candidates.length > 0 && (
                <button
                  onClick={purgeCandidates}
                  disabled={purgeDeleting}
                  style={{
                    background: "#c0392b",
                    color: "#fff",
                    border: "none",
                    borderRadius: 6,
                    padding: "8px 14px",
                    cursor: purgeDeleting ? "default" : "pointer",
                  }}
                >
                  {purgeDeleting
                    ? "삭제 중…"
                    : `${purgeScan.candidates.length}개 삭제`}
                </button>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
