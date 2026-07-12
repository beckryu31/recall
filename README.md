# Recall

**English** · [한국어](README.ko.md)

**A local-first desktop app for browsing your Claude Code prompt history.**

Claude Code doesn't keep a searchable record of what you've asked it. The good prompt you wrote three weeks ago — the one that finally got the refactor right — is buried somewhere in a session transcript you'll never find again. And Claude Code deletes those transcripts after 30 days, by default.

Recall fixes that. A hook logs every prompt you submit to a local SQLite database, a background sweeper captures Claude's full response for each one, and the app turns all of it into a searchable, taggable, bookmarkable archive.

**You don't press anything.** Prompts appear as you submit them; responses fill themselves in as Claude answers.

Everything stays on your machine. Recall makes no network calls — no telemetry, no account, no server — and the
webview runs under a CSP that blocks remote resources, so nothing in a captured response can phone out either.

> **The app's interface, and every warning it emits, is in Korean.** This README is bilingual; the app is not.
> Button labels below are quoted exactly as they appear on screen.

![Recall — a prompt, and Claude's full response beside it: prose rendered as Markdown, tool calls collapsed to one line each](docs/screenshot.png)

<p align="center">
  <em>Projects on the left, your prompt history in the middle, the prompt and Claude's response on the right.<br>
  Tool calls are collapsed — <code>▸ Bash …&nbsp;3 KB</code>, <code>▸ 결과&nbsp;2 KB</code> — and open on click.<br>
  The response on screen is the one that stitched a chopped-up turn back together: a prompt whose stored response<br>
  was <strong>0 KB</strong> became a complete 205 KB story. Recall, recalling how Recall got built.</em>
</p>

---

## What it does

**Captures, automatically**

- **Every prompt**, the moment you submit it — including custom skills and slash commands.
- **Every response, in full** — Claude's prose, the tools it called, the arguments it passed, and what came back. Tool output is collapsed to one line until you open it, so a 300 KB command dump doesn't get in your way.
- **One prompt = one complete story.** A turn that spans background-agent notifications, a `/skill` invocation, a `!bash` command, or an ESC interrupt is stitched back into a single response instead of being scattered across rows.
- **The raw transcripts**, copied out of Claude Code's retention window (`cleanupPeriodDays`, 30 days out of the box) into an archive Recall owns.

**Browse**

- **Search** across prompts and responses.
- **Group by project** — prompts are bucketed by working directory, and you can give each one a readable alias (`/Users/you/dev/some-long-path` → `Payments API`).
- **Bookmark** and **tag** the prompts worth keeping; filter by either.
- **Filter by date range.**
- **Edit and copy** a prompt to reuse it.

**Tells you when it breaks**

If capture stops — a stuck database, a stopped sweeper, a full disk — Recall says so *inside Claude Code*, because you might not open the app for a week. See [Health warnings](#health-warnings).

---

## Setup

Recall shows you your own history, so it's only useful once history exists. **Install the hook first**, use Claude Code normally for a while, then run the app.

### 1. Install the hook and the sweeper

```bash
git clone https://github.com/beckryu31/recall.git
cd recall

# The sweeper runs this binary. Build it BEFORE registering the sweeper.
cargo build --release --bin recall-ingest --manifest-path src-tauri/Cargo.toml

bash hooks/install.sh
```

`install.sh` does three things, and **refuses to finish if any of them didn't actually work**:

- Symlinks `~/.claude/hooks/log_prompt.py` → the repo's copy. A symlink, not a copy, because a copy drifts: you fix the hook, the fix never runs, and you don't find out for weeks.
- **Copies** `sweep.sh`, `archive_transcripts.py` and `recall-ingest` into `~/.claude/recall-bin/`. Not a symlink this time — macOS TCC does not let a `launchd` agent read `~/Documents`, `~/Desktop`, `~/Downloads` or iCloud Drive, so a sweeper that points into a repo cloned there fails on every run with `Operation not permitted`, silently, while `launchctl bootstrap` reports success. The copies live where `launchd` can reach them. (They can go stale — the hook watches for that and warns you.)
- Registers a `launchd` agent that runs the sweeper every hour: it archives the raw transcripts and ingests responses even while the app is closed. **`install.sh` then waits and checks the agent's exit status**, because "registered" is not "ran".

Re-run `bash hooks/install.sh` after you change the code or move the repo — the sweeper is a copy.

Then register the hook in `~/.claude/settings.json`:

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

> Claude Code snapshots its hook **config** when a session starts, so this takes effect in your next session. It re-reads the hook **script** on every call, so editing `log_prompt.py` takes effect immediately.

Verify — submit a prompt in Claude Code, then:

```bash
sqlite3 ~/.claude/prompts.db "SELECT count(*) FROM prompts;"
```

### 2. Stop Claude Code from deleting your transcripts so fast

Claude Code prunes session transcripts after **30 days** by default. A response Recall never got to is gone forever when that happens. Give yourself margin, in `~/.claude/settings.json`:

```json
{ "cleanupPeriodDays": 90 }
```

Recall's archive keeps the raw bytes indefinitely once it has seen them, so this only has to cover the gap between a session ending and the sweeper next running. But a bigger number costs disk, and a smaller one costs data.

### 3. Build and run the app

Requires [Rust](https://rustup.rs/), Node.js, and **Python 3** (macOS ships `/usr/bin/python3`). The hook and the archiver are Python; if `python3` is not on your `PATH` the hook silently does nothing — including the health check that would have told you so.

```bash
npm install
npm run tauri dev      # development
npm run tauri build    # production bundle → src-tauri/target/release/bundle/
```

You already built the headless ingester in step 1. If `~/.claude/recall-archive/archive.log` ever says `수집기가 없습니다`, the sweeper has been archiving but ingesting nothing — rebuild it and re-run `hooks/install.sh`.

On macOS, install the bundle and clear the quarantine flag (the build is ad-hoc signed):

```bash
rm -rf /Applications/Recall.app
ditto src-tauri/target/release/bundle/macos/Recall.app /Applications/Recall.app
xattr -dr com.apple.quarantine /Applications/Recall.app
```

---

## Using it

**In normal use you press nothing.** The app polls every 3 seconds; new prompts appear on their own, and responses fill in as Claude answers — including while a turn is still running.

If the list is scrolled, new prompts don't shove it around. A **`새 프롬프트 N개 ↑`** pill appears instead; click it to jump to the top.

### Reading a response

A response is a sequence of **segments**:

| | |
|---|---|
| Prose | Rendered as Markdown. |
| `▸ Bash cargo test --lib` | A tool call. The command is right there in the label. Click to see the full arguments. |
| `▸ 결과 · 12 KB` | What the tool returned. Collapsed, with its size. Tool bodies are never sent to the UI until you open one — that's why the biggest response in my archive (550 KB across 700 segments) opens instantly. Short prose is sent inline; anything over 24 KB or 400 lines is fetched on demand. |
| `▸ 백그라운드 작업 알림` | A background agent finished and Claude kept working. Folded into this turn, not split off into its own row. |
| `사용자가 중단함` | You pressed ESC. Everything Claude said before that is still here. |

Very long prose (a pasted lockfile diff, say) renders as plain text rather than Markdown. Rendering 60,000 lines as Markdown builds ~90,000 DOM nodes and freezes the window for over a second; as plain text it's one node and opens instantly.

### The buttons

You shouldn't need these. They're kept as manual overrides for when automation is wrong or stuck.

| | |
|---|---|
| `↻` | Reload the list now instead of waiting 3 seconds. |
| `⟳ 수집` | Re-ingest every session now instead of waiting for the sweeper. It rebuilds `turn_segments` and `narrative` from the transcripts — idempotently, on the `(prompt_id, src_uuid, block_idx)` merge key — and **never touches `prompts.response`**. A session whose transcript is gone is skipped, not blanked. |
| `⤓ 24h` | Legacy. Re-extracts responses from the last 24 hours into the old `prompts.response` column. |
| `전체` | Legacy. Re-extracts **every** response into `prompts.response`, overwriting what's there. Snapshots the database first. This is the blunt instrument — reach for it only if something is visibly wrong. |
| `🧹 정리` | Offer prompts that have no response for deletion. See [Deletion](#deletion-is-the-dangerous-one). |

---

## How it works

```
                   ┌──────────────────────────────┐
   you submit ───▶ │  UserPromptSubmit hook       │
   a prompt        │  hooks/log_prompt.py         │
                   └───────────────┬──────────────┘
                                   │ spool the payload, then INSERT
                                   ▼
                        ~/.claude/prompts.db
                        prompts(prompt, cc_turn_id, …)
                                   │
                                   │ joined on cc_turn_id  ⟷  promptId
                                   ▼
   Claude Code ────▶ ~/.claude/projects/**/*.jsonl   (deleted after cleanupPeriodDays)
   writes                          │
                                   │ copied, gzipped
                                   ▼
                        ~/.claude/recall-archive/    (kept)
                                   │
                                   │ hourly sweeper — and the app, every 15s
                                   ▼
                        turn_segments(kind, name, body, …)
                                   │
                                   ▼
                            ┌─────────────┐
                            │   Recall    │
                            └─────────────┘
```

### Matching a prompt to its turn

This is the part that is easy to get subtly, silently wrong.

Every Claude Code hook payload carries a **`prompt_id`**, and every `user` event in the transcript carries a **`promptId`**. They are the same value — both read the same variable inside Claude Code. The hook stores it as `prompts.cc_turn_id` and Recall joins on it. **No timestamp window, no text search.** (A text *sanity check* still runs on the joined pair: if the ID and the prompt text disagree, Recall refuses the match rather than trusting the key — `prompt_id` is read from a mutable global inside Claude Code, and a key that is confidently wrong is worse than a fuzzy one.)

That matters, because text comparison *doesn't work*: a custom slash command is stored as `/my-skill` in the database but expands to `<command-name>/my-skill</command-name>` in the transcript. Matching on text loses it. Matching on the ID doesn't care.

> **`promptId` is a *turn* ID, not a prompt ID.** It is not globally unique — subagent transcripts inherit their parent's, and two prompts queued back-to-back can share one. Never put a `UNIQUE` index on it; you would silently drop real prompts.

Rows written before the hook stored `cc_turn_id` fall back to the old heuristic (a cached transcript uuid, then a ±30s timestamp window with a text-prefix tiebreak). Every row records which path matched it, in `prompts.match_kind`.

### Deciding where a turn ends

> **A transcript `user` event owns a turn if and only if `UserPromptSubmit` fired for it** — that is, if a `prompts` row exists for it.
>
> The hook firing *is* the evidence that a human typed something.

Everything else — background-agent notifications, built-in slash commands, `!bash` lines, ESC interrupts — is **folded into the turn it interrupted**. That is why one prompt gives you one complete story.

The obvious alternative (filter the transcript's `promptSource` field) is wrong: measured against a real database, it dropped prompts in 21 of 133 sessions.

### Why there is no `Stop` hook

The natural design is to hook `Stop` and capture the response the moment a turn ends. Measured against ~1,700 real prompts, that design **loses about 10% of them**:

- `Stop` doesn't fire when a turn is aborted — you pressed ESC, the session died, the machine slept. Those turns still contain everything Claude said before you stopped it.
- `Stop` fires **more than once** for 13% of prompts, because a background-agent notification arrives *after* the turn ends and Claude keeps working. Naively checkpointing at the first `Stop` throws away the rest — which is 60% of the content in those turns.

So a sweeper is needed anyway. And once a sweeper exists, a hook only buys latency — paid for by adding work to every single turn of your main tool, forever. Re-reading every transcript from scratch is a few seconds of work in a job that runs hourly, in the background, at low I/O priority. (2.2 s over a 391 MB corpus when I measured it in Python; the archive grows and that number won't shrink.) It isn't worth a hook.

The sweeper has no byte cursor either. It re-parses whole files and merges on `(prompt_id, src_uuid, block_idx)`. A cursor you can place wrong is a cursor that can silently drop the end of a turn; there isn't one, so it can't.

**Stack:** Tauri 2 (Rust) + React 19 + TypeScript. Rust isn't here for speed — it's here because a browser sandbox can't read `~/.claude`.

---

## Your data

Recall is entirely offline. Everything lives under `~/.claude/`:

| Path | Access | What |
|---|---|---|
| `prompts.db` | read **and write** | Prompts, responses, segments, your bookmarks and tags |
| `projects/**/*.jsonl` | **read only** | Claude Code's own transcripts. Recall never writes here. |
| `recall-archive/` | written | gzipped copies of the transcripts, off the retention clock (~30% of the original size) |
| `recall-backups/` | written | Database snapshots, taken before **every** delete (single or bulk) and before a bulk overwrite. Ten auto-snapshots kept; snapshots you make by hand are never pruned. |
| `recall-spool/` | written | Prompts that could not reach the database. Normally empty. |

### The schema

`prompts` **and `capture_health`** are created by the hook — it has to be able to raise the alarm even when the app has never been launched. The app creates everything else on first launch, and adds the `msg_uuid` / `kind` / `narrative` / `match_kind` / `ingest_ver` columns to `prompts`.

```sql
prompts(id, session_id, cwd, prompt, created_at,
        response,          -- legacy text. FROZEN: automation never writes it again.
        msg_uuid,          -- legacy: cached transcript event uuid
        cc_turn_id,        -- = the hook's prompt_id = the transcript's promptId
        transcript_path,
        kind,              -- 'human' | 'system' (an injected event; hidden from the list)
        narrative,         -- prose-only flattening. This is what search matches.
        match_kind,        -- 'exact' | 'uuid' | 'fuzzy' | 'conflict'
        ingest_ver)

turn_segments(prompt_id, src_uuid, block_idx, seq,
              kind,        -- text | tool_use | tool_result | injected | interrupt | image
              name, tool_id, preview, body, nbytes, nlines, is_error, ts)
              -- UNIQUE(prompt_id, src_uuid, block_idx) is the merge key, so re-ingesting
              --   the same transcript is idempotent and a parser fix can't duplicate rows.

segment_edits(prompt_id, src_uuid, block_idx, body, edited_at)  -- yours; automation never touches it
ingest_state(session_id, src_mtime, ingested_at)                -- "has this transcript grown?"
capture_health(id, problems, checked_at)                        -- written by the hook, read by the app

cwd_aliases(cwd, alias, updated_at)
prompt_bookmarks(prompt_id, created_at)
tags(id, name, created_at)
prompt_tags(prompt_id, tag_id)
prompt_responses(prompt_id, response, fetched_at)               -- legacy cache
```

### The safety rule

> **The hook writes new `prompts` rows and `capture_health`.**
> **The ingester writes `turn_segments`, `ingest_state`, and the `narrative` / `kind` / `match_kind` / `ingest_ver` columns of rows that already exist — and nothing else.**
> **You write `prompts.response` and `segment_edits`.**
> **There is no code path from the ingester to a byte a human wrote, and `ingest.rs` contains no `DELETE`.**

`prompts.response` — the text extracted by older versions of Recall — is never modified by automation again. For prompts whose transcript has already been pruned, **it is the only copy that will ever exist.** The full-fidelity capture went into a table that didn't exist yet, so the upgrade had nothing to destroy.

### Deletion is the dangerous one

Editing a prompt is an `UPDATE`. Deleting one is a real `DELETE`. There is no trash.

`🧹 정리` (bulk cleanup) demands **positive evidence of absence** before it offers anything for deletion:

1. The transcript was **found**, and
2. that turn provably contains **no response**, and
3. the prompt is more than **48 hours** old, and
4. it carries **no bookmark and no tag**, and
5. it has **no segments**.

If the transcript is missing, or the lookup failed, the prompt is `unknown` — **not deletable.** Absence of evidence is not evidence of absence.

That distinction is not academic. An earlier version treated *"I tried to recover this and failed"* as *"safe to delete"* — and a bug in the recovery path turned that into **438 deleted prompts.** The snapshot in `recall-backups/` was the only thing that brought them back.

The database is snapshotted (`VACUUM INTO`, so the copy is transactionally consistent) before any bulk delete or bulk overwrite. **If the backup fails, nothing is deleted.** To undo, copy the snapshot back over `~/.claude/prompts.db`.

---

## Health warnings

Capture is designed to fail **silently**. A hook that crashes puts a traceback under your prompt, and a prompt logger must never get in the way of writing a prompt. But that means *every* failure is silent — and a silent failure surfaces 30–90 days later as missing data, by which point the transcripts are gone.

So the alarm goes where you cannot miss it. `UserPromptSubmit`'s stdout is injected into Claude's context, so **Claude tells you**:

```
[Recall 수집 상태 경고 — 사용자에게 알려주세요]
- Recall 이 4일째 응답을 수집하지 못했습니다
- 세션 기록 아카이브가 4일째 갱신되지 않았습니다 (launchctl list | grep recall 확인)
```

It watches for a spool backlog (prompts not reaching the database — the worst case, because a prompt cannot be regenerated from anything else on disk), a stopped sweeper, a full disk, and stale ingestion. At most once an hour.

The app shows the same warning as a banner with **no dismiss button**. It isn't a toast you flick away; it's *data is disappearing right now*, and it goes away when you fix it.

The checks that matter most run **without the database**, deliberately. A spool backlog, a stopped sweeper, a stale sweeper copy and a full disk are all visible from the filesystem alone — and those are the only signals left when the database is the thing that broke. (Stale *ingestion* is the one check that needs the database. When the database is unreachable that check is skipped, and so is the write to `capture_health` that feeds the app's banner — leaving stdout as the only channel, which is the point.)

---

## Known limitations

- **Claude Code does not save its own reasoning.** Every `thinking` block in a transcript has an empty body; only an opaque signature is kept. "Everything Claude said" has a hard ceiling, and no design can raise it.
- **Only prompts logged after the hook is installed appear.** There is no backfill from existing transcripts.
- **A prompt with no response is usually correct, not a bug.** If you cancelled a turn with ESC before Claude answered, there is genuinely nothing to show.
- **Subagent transcripts are archived but not ingested.** What an agent *reported back* is captured (it arrives as a tool result); what it *did internally* is in the archive, not in the app.
- **Sessions are grouped by their first working directory.** If you `cd` mid-session, every prompt in that session is still filed under where it started.
- **Search is a `LIKE` scan** over the prompt, the prose flattening, and the legacy response column. Fine at this size; it will not stay fine forever.
- **The hook is a symlink into the repo.** Checking out a different branch changes the hook that runs on your next prompt. That is the price of never running a stale copy.
- **macOS only.** The Tauri app would probably build elsewhere, but `hooks/install.sh` installs a `launchd` agent and there is no Linux/Windows equivalent — and the sweeper is the only automatic capture path when the app is closed. You would have to schedule `hooks/sweep.sh` yourself.

---

## License

MIT
