# Recall

**English** · [한국어](README.ko.md)

**A local-first desktop app for browsing your Claude Code prompt history.**

Claude Code doesn't keep a searchable record of what you've asked it. The good prompt you wrote three weeks ago — the one that finally got the refactor right — is buried somewhere in a session transcript you'll never find again.

Recall fixes that. A hook logs every prompt you submit to a local SQLite database; the app turns that database into a searchable, taggable, bookmarkable archive — with Claude's actual responses pulled in alongside each prompt.

Everything stays on your machine. No network calls, no telemetry, no account.

![Recall — browsing prompt history, with the prompt and Claude's response side by side](docs/screenshot.png)

<p align="center">
  <em>Projects and tags on the left, your prompt history in the middle, the prompt and Claude's response on the right.<br>
  The prompt on screen is the one that added the "no response saved" badge you can see in the list behind it — Recall, recalling how Recall got built.</em>
</p>

---

## What it does

- **Search** across every prompt you've ever submitted.
- **Group by project** — prompts are bucketed by working directory, and you can give each one a readable alias (`/Users/you/dev/some-long-path` → `Payments API`).
- **Bookmark** the prompts worth keeping.
- **Tag** prompts freely and filter by tag.
- **Filter by date range.**
- **Read the response** — Recall finds the session transcript for a prompt and extracts what Claude replied, rendered as Markdown.
- **Edit and copy** — fix up a prompt for reuse, then copy it to the clipboard.
- **Clean up** — sweep the prompts that have no response. Recall first tries to recover each one from the transcripts, and only offers the genuinely unanswerable ones (a prompt you cancelled with ESC, a session whose transcript is gone) for deletion. It backs the database up before deleting anything.

---

## How it works

Recall reads two data sources that Claude Code already writes to your disk, and joins them:

```
                 ┌─────────────────────────────┐
  you submit ──▶ │  UserPromptSubmit hook      │
  a prompt       │  hooks/log_prompt.py        │
                 └──────────────┬──────────────┘
                                │ INSERT
                                ▼
                 ~/.claude/prompts.db          ← prompt text, session_id, cwd, timestamp
                                │
                                │  joined on prompt text
                                ▼
                 ~/.claude/projects/<cwd>/<session_id>.jsonl
                                               ← Claude Code's own session transcript,
                                                 where the assistant's replies live
                                │
                                ▼
                         ┌─────────────┐
                         │   Recall    │
                         └─────────────┘
```

The hook captures prompts as you write them. The responses aren't in the database — they're in Claude Code's session transcripts — so when you open a prompt, Recall locates the matching `.jsonl` file, finds the `user` event that produced it, collects the `assistant` blocks that follow, and caches the result back into the database.

Finding the right `user` event is the tricky part. Matching on the prompt text alone doesn't work: a transcript's `message.content` is sometimes a plain string, sometimes an array of blocks (any prompt with an image attachment), and sometimes a command wrapper. So Recall matches on the transcript event's **`uuid`** instead — resolved once by timestamp (±30s window, tie-broken by a text prefix), then cached in `prompts.msg_uuid` so later lookups are exact. Collection stops at the next real user prompt, which keeps an interrupted turn from swallowing the next answer.

**Stack:** Tauri 2 (Rust backend) + React 19 + TypeScript. Rust isn't here for speed — it's here because a browser sandbox can't read `~/.claude`.

---

## Setup

Recall shows you your own history, so it's only useful once history exists. **Install the hook first**, use Claude Code normally for a while, then run the app.

### 1. Install the prompt-logging hook

Copy the hook script into your Claude Code config directory:

```bash
mkdir -p ~/.claude/hooks
cp hooks/log_prompt.py ~/.claude/hooks/log_prompt.py
```

Register it as a `UserPromptSubmit` hook in `~/.claude/settings.json`:

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

The hook creates `~/.claude/prompts.db` on first run and appends one row per prompt. It exits silently on any error, so a broken hook can never block a prompt from being sent.

Verify it's working — submit a prompt in Claude Code, then:

```bash
sqlite3 ~/.claude/prompts.db "SELECT count(*) FROM prompts;"
```

### 2. Build and run the app

Requires [Rust](https://rustup.rs/) and Node.js.

```bash
npm install
npm run tauri dev      # development
npm run tauri build    # production bundle
```

> **Upgrading from an earlier version of the hook?** Recall stores fetched responses in a `response` column that older versions of `log_prompt.py` didn't create. If the app errors on startup, add it:
>
> ```bash
> sqlite3 ~/.claude/prompts.db "ALTER TABLE prompts ADD COLUMN response TEXT;"
> ```

---

## Your data

Recall is entirely offline. It touches exactly two locations, both already on your machine:

| Path | Access |
|---|---|
| `~/.claude/prompts.db` | read **and write** |
| `~/.claude/projects/**/*.jsonl` | read only |
| `~/.claude/recall-backups/` | written before any bulk delete |

The `prompts` table is created by the hook. On first launch, the app adds five tables of its own for the metadata it manages — bookmarks, tags, directory aliases, and a response cache — plus a `msg_uuid` column on `prompts` to cache the resolved transcript event:

```sql
prompts(id, session_id, cwd, prompt, created_at, response, msg_uuid)  -- rows written by the hook
cwd_aliases(cwd, alias, updated_at)
prompt_bookmarks(prompt_id, created_at)
tags(id, name, created_at)
prompt_tags(prompt_id, tag_id)
prompt_responses(prompt_id, response, fetched_at)                     -- legacy cache
```

**Recall can modify and delete your prompt history.** Editing a prompt runs an `UPDATE` on the `prompts` row, and deleting one is a real `DELETE` — there is no undo and no trash.

Bulk cleanup is the one destructive operation that protects itself:

- Prompts that carry a **bookmark or a tag are never offered for deletion** — you touched them, so Recall leaves them alone.
- Every prompt is **re-checked against the transcripts first**; anything whose response can be recovered is recovered and dropped from the list.
- The database is **snapshotted to `~/.claude/recall-backups/` before a single row is deleted** (via `VACUUM INTO`, so the copy is transactionally consistent). If the backup fails, nothing is deleted. The ten most recent snapshots are kept.

To undo a cleanup, copy the snapshot back over `~/.claude/prompts.db`.

---

## Known limitations

- **A prompt with no response is usually correct, not a bug.** If you cancelled a turn with ESC before Claude answered, there is genuinely no response to show. Recall no longer confuses this with a failed lookup.
- **Response lookup can still miss.** The transcript event is resolved by timestamp and text prefix, so a deleted or truncated `.jsonl` leaves the prompt without a response. The prompt itself still shows.
- **Responses are summarized, not replayed.** Text blocks are kept verbatim; tool calls are collapsed to a `[tool: Read]` marker. Recall shows you what Claude *said*, not everything it *did*.
- **Slash commands never reach the database.** The `UserPromptSubmit` hook receives no prompt text for `/some-command`, so no row is written. They exist in the transcripts but not in Recall.
- **Only prompts logged after the hook is installed appear.** There's no backfill from existing transcripts.
- **Sessions are grouped by their first working directory.** If you `cd` mid-session, every prompt in that session is still filed under where it started.
- macOS is the only platform this has been used on. Tauri should build on Linux and Windows, but neither is tested.

---

## License

MIT
