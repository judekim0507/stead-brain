---
name: gmail-browser
description: Drive Gmail through Stead's Playwright-compatible browser REPL for reading, searching, composing, and triaging mail.
---

Use this skill when operating Gmail or Google Mail in a browser tab.

Perception order:

1. Start with `await page.snapshot()` inside `browser_exec` for the active Gmail tab.
2. Use Gmail's visible labels, search box, message list rows, compose dialog, and toolbar buttons from the snapshot before falling back.
3. Prefer `page.getByRole`, `getByText`, and `getByLabel`; use `page.evaluate` only for a narrowly targeted ambiguous custom control.
4. Use `await page.screenshot()` only when layout or visual grouping matters.

Common flows:

- Search mail: fill the Gmail search box, submit with `page.keyboard.press('Enter')`, then re-snapshot.
- Open a thread: click the message row with a semantic locator, then verify the subject/sender in the new snapshot.
- Compose/reply: click Compose or Reply, fill To/Subject/body fields semantically when exposed; otherwise focus the editor and type through brokered keys only after the semantic path fails.
- Triage: archive, star, mark unread, or label only after identifying the selected thread/row in a fresh snapshot.

Safety:

- Do not send, delete, archive, or label mail without verifying the target thread or recipient in the latest snapshot.
- Treat attachments as file-access actions. Only upload user-approved paths through `stead.fileChooser.setFiles(handle, paths)`.
- Never read or expose credential fields. If Gmail redirects to login, use `browser-credential-handoff`.
