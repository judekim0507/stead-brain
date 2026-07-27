---
name: notion-browser
description: Work in Notion with browser-native snapshots, careful editor focus, and compact verification loops.
---

Use this skill for Notion pages, databases, comments, and editor workflows.

Perception order:

1. Use `await page.snapshot()` inside `browser_exec` to identify page title, blocks, database rows, toolbar controls, and dialogs.
2. Prefer `page.getByRole`, `getByText`, and `getByLabel`; use `page.evaluate` only for targeted inspection of custom controls, slash menus, popovers, and database property menus.
3. Use `await page.screenshot()` only when visual hierarchy, drag targets, or table layout matters.

Editing workflow:

1. Snapshot first and identify the exact block, title, row, or property.
2. Prefer semantic focus/click/fill when the field is exposed.
3. For rich text blocks that do not expose a clean fill action, focus the target and use `page.keyboard` input.
4. Re-snapshot after editing and verify the changed block or property before claiming success.

Safety:

- Database deletes, permission changes, sharing changes, and destructive bulk edits require explicit confirmation.
- Avoid raw drag/drop unless necessary and confirmed; prefer semantic controls.
- If Notion redirects to login, use `browser-credential-handoff`.
