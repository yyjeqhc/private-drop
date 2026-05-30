# GPT workflow for Private Drop v4

Use `/codex-openapi-compact.json` for GPT Actions. It keeps the action set small while preserving the main Codex loop.

## Core loop

1. Observe state with `getProjectContextBatch`.
2. Read only the files needed for the task.
3. Prefer `applyProjectEdit` for deterministic edits.
4. Run the smallest useful check first, then broader checks.
5. Commit with `runProjectGit` after checks pass.
6. Write a final report with `writeProjectReport`.

## Goal-scoped workflow

Goal-scoped execution reduces repeated approvals without removing the user approval boundary.

1. GPT proposes a goal with `runCommandRequestOp`:

```json
{"op":"create_goal","project":"private-drop-v4","title":"Implement bounded task","summary":"What GPT will and will not touch.","ttl_secs":7200}
```

2. The goal starts as `pending` and grants no execution rights.
3. The user explicitly approves the returned `goal_id` in chat.
4. GPT activates the goal:

```json
{"op":"approve_goal","goal_id":"<goal-id>"}
```

5. While the goal is active, GPT may run bounded operations such as:

```json
{"op":"create_and_approve","project":"private-drop-v4","goal_id":"<goal-id>","command":"test","reason":"verify changes"}
```

or:

```json
{"op":"create_raw_and_approve","project":"private-drop-v4","goal_id":"<goal-id>","command_text":"git status --short","reason":"inspect state"}
```

6. GPT closes the goal when done:

```json
{"op":"close_goal","goal_id":"<goal-id>","reason":"completed"}
```

## Safety rules

- `create_goal` does not grant execution rights.
- Only `active` and unexpired goals allow `*_and_approve` operations.
- `pending`, `rejected`, `expired`, and `closed` goals cannot auto-approve commands.
- Raw commands still require `allow_raw_command_requests = true`.
- Configured commands still require `allow_command_requests = true`.
- Raw commands are single-line, length-limited, and checked for high-risk tokens.
- All command executions still create normal `command_requests` audit records.

## Diagram assets

- `docs/diagrams/goal-workflow.svg`: browser-friendly static diagram.
- `docs/diagrams/goal-workflow.mmd`: Mermaid source for Markdown renderers.
- `docs/diagrams/goal-workflow.html`: standalone HTML diagram.
- `docs/diagrams/goal-workflow.excalidraw.json`: editable Excalidraw scene.
