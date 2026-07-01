---
name: artifact-document
description: Create durable chat artifacts such as Markdown, HTML, CSV, JSON, and binary document outputs using scoped session file tools.
---

Use this skill when the user asks for a file, report, export, document, spreadsheet, presentation outline, webpage, script output, or other durable artifact.

File placement:

1. Use `session_tmp` for scratch scripts, previews, extracted intermediate data, and throwaway conversion inputs.
2. Use `session_artifacts` for final files the user asked to keep.
3. Use `session_attachments` only as read-only input.
4. Do not write outside session roots or approved folders unless the user explicitly approved that folder.

Writing rules:

- Use `files.write` for text and binary outputs.
- For binary files, pass `content_base64`.
- Keep generated outputs scoped to the current chat. For `session_tmp` and `session_artifacts`, omit `session_id` unless you intentionally need another session.
- Prefer simple durable formats first: Markdown, HTML, CSV, JSON, plain text. For Office/PDF-style outputs, create the source representation and any helper script under `session_tmp`, then write the final binary under `session_artifacts` when the conversion is available.

Verification:

1. Re-read or list the final artifact path with `files.read` / `files.list` when practical.
2. Report the artifact path and format.
3. If a requested format cannot be generated in the current helper environment, produce the closest useful source artifact and explain the missing converter.
