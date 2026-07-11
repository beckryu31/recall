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

The hook captures prompts as you write them. The responses aren't in the database — they're in Claude Code's session transcripts — so when you open a prompt, Recall locates the matching `.jsonl` file, scans for the `user` line whose text matches the prompt exactly, collects the `assistant` blocks that follow, and caches the result back into the database.

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

The `prompts` table is created by the hook. On first launch, the app adds five tables of its own for the metadata it manages — bookmarks, tags, directory aliases, and a response cache:

```sql
prompts(id, session_id, cwd, prompt, created_at, response)  -- written by the hook
cwd_aliases(cwd, alias, updated_at)
prompt_bookmarks(prompt_id, created_at)
tags(id, name, created_at)
prompt_tags(prompt_id, tag_id)
prompt_responses(prompt_id, response, fetched_at)           -- legacy cache
```

**Recall can modify and delete your prompt history.** Editing a prompt runs an `UPDATE` on the `prompts` row, and deleting one is a real `DELETE` — there's no undo and no trash. Back up `~/.claude/prompts.db` if that history matters to you.

---

## Known limitations

- **Response lookup matches on exact prompt text.** If the same prompt appears twice in one session, Recall attaches the first response it finds. If a prompt was edited or stored in a form that differs from the transcript, the lookup can miss entirely — the prompt still shows, just without a response.
- **Responses are summarized, not replayed.** Text blocks are kept verbatim; tool calls are collapsed to a `[tool: Read]` marker. Recall shows you what Claude *said*, not everything it *did*.
- **Only prompts logged after the hook is installed appear.** There's no backfill from existing transcripts.
- **Sessions are grouped by their first working directory.** If you `cd` mid-session, every prompt in that session is still filed under where it started.
- macOS is the only platform this has been used on. Tauri should build on Linux and Windows, but neither is tested.

---

## License

MIT
