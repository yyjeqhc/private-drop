# Runtime Console navigation

Open `/runtime` and connect with an existing runtime credential.

The sign-in page also links to `/demo`, a separate interactive feature tour with
fictional Projects, Runners, task history, logs, and code review. It requires no
credential and never requests runtime APIs or reads browser credentials. See the
[demo guide](demo.md) for the local preview and example interactions.

The workspace start page offers **Find a project**, **Runtime overview**, and
**Durable Agents**. Press **Command+K** on macOS or **Ctrl+K** on Windows/Linux
while connected to open Projects & Sessions and focus project search. On narrow
screens this also opens the navigation drawer. These shortcuts navigate only;
they do not create Sessions or send messages.

- **Projects & Sessions** is the collaboration workspace. Select a Project and
  Workflow Session in the sidebar. Recent Sessions starts expanded and can be
  collapsed when more room is needed.
- **Context** opens the selected Session's context. **Overview** shows work,
  attention, validation, and model-reported progress directly. **Activity** shows
  retained events and the existing follow-latest control. **Details** shows
  identity, lifecycle, mode, timestamps, and workspace information.
- **Runtime & Agents** provides three separate destinations: **Overview**,
  **Runner fleet**, and **Durable Agents**. Selecting a destination shows its
  full content and updates the navigation highlight. Switching destinations
  keeps existing forms mounted so unsent input is retained.

The context panel still adapts between a docked rail, popover, and mobile sheet.
Closing it leaves a labeled Context entry in the header. Context navigation uses
ordinary keyboard-focusable buttons, with the current choice announced as pressed.
Mobile operation navigation closes after selection and focuses the destination.

These are presentation changes. Workflow Session and durable Agent identities,
credential scopes, refresh behavior, and mutation handling keep their existing
contracts. Model-reported progress remains informational.

Each selected Project/workspace has its own keyboard-accessible disclosure below
its Runner. Closing it hides that workspace's Sessions without clearing the
selected Session or composer; its preference survives refresh. Selecting another
workspace opens its Session list.

At widths of 1280px and above, Context docks beside a narrower conversation, with
more room for readable status and progress. Its Overview starts with the latest
retained Agent-authored message. Resolution text is also visible directly below
the original message, rather than only in a tooltip.

Project search continues to query authorized Projects by name, id, Runner, or
workspace path. Separate searches filter the loaded Sessions by title/id/lifecycle
and retained messages by body, resolution, id, or author Session. Match counts
refer only to loaded results; these controls do not search unretained history or
host transcripts and do not change the selected Session. Expand **Search retained
messages** above the board to filter messages. Its match count stays visible when
collapsed; closing the search keeps the current filter.

The Session board is not a mirrored host chat transcript. An observed ACK records
an explicit model-context acknowledgement, not a reply, read receipt, or completed
work. Model replies appear only when explicitly posted to the Session; reported
progress is separately labeled as informational. An empty latest-message card
means no Agent message exists in the currently retained window.
