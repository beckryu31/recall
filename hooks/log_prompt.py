#!/usr/bin/env python3
"""Claude Code UserPromptSubmit hook: append every submitted prompt to ~/.claude/prompts.db.

Registered in ~/.claude/settings.json so that Claude Code pipes a JSON payload
on stdin each time a prompt is submitted. The hook must never block the prompt,
so any failure exits quietly with status 0.
"""
import json
import sqlite3
import sys
from datetime import datetime
from pathlib import Path

DB_PATH = Path.home() / ".claude" / "prompts.db"


def init_db(conn):
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS prompts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT,
            cwd          TEXT,
            prompt       TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            response     TEXT
        )
        """
    )
    conn.execute("CREATE INDEX IF NOT EXISTS idx_prompts_session ON prompts(session_id)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_prompts_created ON prompts(created_at)")


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    with sqlite3.connect(DB_PATH) as conn:
        init_db(conn)
        conn.execute(
            "INSERT INTO prompts (session_id, cwd, prompt, created_at) VALUES (?, ?, ?, ?)",
            (
                data.get("session_id"),
                data.get("cwd"),
                data.get("prompt", ""),
                datetime.now().isoformat(timespec="seconds"),
            ),
        )

    sys.exit(0)


if __name__ == "__main__":
    main()
