---
name: github-browser
description: Operate GitHub in the browser using Stead's native page tools for issues, pull requests, code review, and repository navigation.
---

Use this skill for GitHub web workflows when logged-in browser state matters.

Perception order:

1. Use `browser.snapshot` to identify repo navigation, issue/PR titles, tabs, form fields, and action buttons.
2. Use `browser.navigate` for direct GitHub URLs when the target URL is known.
3. Use `browser.probe_node` for code review controls, dropdowns, or custom buttons that are ambiguous in AX.
4. Use `browser.screenshot` only for visual diffs, diagrams, or layout-specific review.

Common flows:

- Inspect issue/PR: navigate or click to the page, snapshot title/state/labels, then summarize only verified page content.
- Comment: find the textarea/editor, fill or focus+type, then re-snapshot the preview/textarea before submitting.
- Review PR: inspect changed files and comments one file at a time. Avoid broad screenshots unless code layout is visually relevant.
- Manage labels/assignees/milestones: use semantic controls, verify selected options, then apply.

Safety:

- Do not merge, close, delete branches, change visibility, transfer ownership, or publish secrets without explicit user confirmation through the browser broker.
- Do not paste tokens or credentials into GitHub. Use `browser-credential-handoff` for auth redirects.
- If repository content is large, keep reads narrow and summarize incrementally.
