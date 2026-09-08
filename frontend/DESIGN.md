# WebCodex Runtime UI

This document is the visual contract for the Runtime Console. It is deliberately product-specific: WebCodex is a dense, local-first control surface for the hierarchy **Runner → Project → Session → Conversation**, not a generic AI landing page.

## Design synthesis

The system combines the useful parts of the repositories in the owner's `develop-ui` list without copying any one product:

- **Ant Design, Arco Design, Element Plus, and TDesign**: token-driven controls, predictable states, compact enterprise density, and the same component behavior in light/dark and desktop/mobile layouts.
- **developer-roadmap**: progressive disclosure. Show the current branch of the Runner/Project/Session hierarchy and keep fleet-wide detail one level deeper.
- **awesome-design-md**: record decisions as a durable design contract. Linear contributes the restrained dark surface ladder and single accent; Cursor contributes the warm light canvas; Apple contributes translucent functional chrome rather than decorative glass.
- **ui-skills and Emil Kowalski's skills**: spacing before dividers, one accent per view, 44 px coarse-pointer targets, visible focus, responsive press feedback, origin-aware popovers, short compositor-only motion, and reduced-motion/transparency fallbacks.

## Product character

The user-supplied robot/terminal mark in `src/webcodex-logo.png` is the shared
brand asset for README, Runtime, Project Review, Admin, and browser icons. Keep
the source image unchanged and use the fixed public asset path when serving it.

`/demo` is a separate, clearly labeled feature tour with fictional data, rather
than a credential mode of the Runtime Console. It uses its own page-memory state
and makes no API calls or browser-storage reads. Its preview of conversation,
execution evidence, and review is illustrative; it does not merge the real
Workflow Session and Project Review authorities.

Calm, precise, technical, and quietly premium. The interface should feel like a well-made developer tool, not a dashboard template and not a glass-effect demo.

- Use transparency only for floating functional layers: top bar, composer, menus, inspector.
- Use tonal contrast and spacing for structural hierarchy. Shadows are reserved for floating layers.
- Use one cool blue-violet accent for selection, focus, links, and the primary send action.
- Do not use decorative glow, multicolor gradients, oversized empty cards, or borders around every group.
- Use platform system fonts. Titles use 600 weight; body uses 400–500. Data and paths use the system monospace stack.

## Tokens

All spacing follows a 4 px base grid. Prefer `4 / 8 / 12 / 16 / 24 / 32` px. Component radii are `8 / 10 / 12 / 16` px; pills alone may use a full radius.

Layout proportions use the golden-ratio family as a guide, not as a rigid grid:

- The primary conversation measure is capped at `1120 px` in the normal workspace and `1160 px` when wide-screen context is docked. This keeps technical output readable without leaving an ultrawide display as an empty canvas.
- The navigation rail follows the smaller golden-ratio derivative in spirit and is bounded to `300–356 px`; device, Project, and Session labels keep enough room while the conversation remains dominant.
- Empty-state focus begins near the first `23.6%` vertical division rather than being mathematically centered, leaving room for the composer and placing the visual center near the upper golden section.
- Outgoing bubbles use at most the major `61.8%` of the conversation measure. Incoming content may use the complementary wider measure because technical output often needs longer lines.
- Below the drawer breakpoint, these proportions yield to full-width content with safe gutters. Golden-ratio geometry must never cause horizontal scrolling or undersized touch targets.

Control sizes:

- Compact: 28 px
- Default desktop: 34–36 px
- Primary/touch: 40–44 px

Surfaces form a stable ladder in both themes:

1. Canvas — conversation workspace.
2. Structural — sidebar and fixed regions.
3. Container — selected rows, fields, and grouped content.
4. Floating — composer, menus, and inspector.

## Components

### Information hierarchy

The interface has three explicit information levels. The same fact must not compete in more than one region.

1. **Primary / always visible** — device, Project and Session names; the current Session liveness; message content; composer; current work and attention.
2. **Secondary / quiet** — update time, author at the beginning of a message group, reply context, acknowledgement and exceptional status. These use muted type and never compete with content.
3. **Evidence / disclosed** — raw IDs, workspace paths, validation evidence, reported progress, full activity, Server metrics, Runner diagnostics, and durable Agent tooling. These remain available in collapsed context disclosures.

Navigation answers “where am I?”, conversation answers “what was said?”, and context answers “what evidence explains the state?”. Navigation never repeats diagnostic evidence, and normal messages do not repeat default type, priority, or open status.

### Navigation hierarchy

- A Runner header shows device identity, textual connection state, Project count, and a disclosure affordance.
- A Project row emphasizes its name first, then Session count/update time, then exceptional status. Workspace path belongs to the context evidence layer, not the navigation tree.
- Sessions live directly under their selected Project. A Session row shows only the title, liveness, and update time; validation and activity previews belong to context. Never wrap the entire hierarchy in one heavy card.
- Search and Runner filtering are proper controls, not hidden diagnostics.

### Conversation

- Messages submitted by the current browser tab may be presented as outgoing/right using a tab-local presentation hint. Messages with trusted `author_session_id` provenance are incoming/left. Retained messages without either signal remain neutral; reply topology and message kind never invent authorship.
- The bubble contains message content only.
- Trusted incoming and provenance-neutral bubbles use a quiet neutral surface; current-tab outgoing bubbles use a restrained true blue rather than the violet selection accent. Both themes preserve this semantic distinction without using status colors as message fills.
- Bubbles shrink to their content and stop at a bounded conversational measure. A uniform `22 px` radius, earlier wrapping, and no hard outline keep short messages capsule-like and prevent long messages from reading as rectangular cards.
- Author, exceptional kind/status, time, reply context, acknowledgement, resolution, and actions live outside the bubble. Default note/normal/open metadata and raw message IDs are not rendered as visible labels.
- Identity is carried by a compact author line at the beginning of each same-side message group. `Agent` is shown only for trusted author provenance and `You` only for a current-tab presentation hint; otherwise the row is labelled as retained without claiming an author. Presentation hints never grant mutation authority.
- Consecutive messages from the same side use a tighter gap; a side change creates a larger conversational beat.
- Reply depth is communicated by a small context line, not growing horizontal indentation.
- Opening or switching a Session positions the transcript at its latest retained message. Newly observed messages follow smoothly; a poll with no new message preserves the reader's current history position.
- Scrolling upward explicitly pauses automatic follow. Newly retained messages then appear behind a bounded “new messages” control and a dedicated screen-reader announcement; they never pull the reader away from history.
- Message content may contain paragraphs, headings, lists, quotes, links, and fenced code. Rendering is DOM-based and text-safe; code has a copy action, and unusually long messages collapse behind an explicit disclosure.

### Composer

- The composer is the primary floating material. It may use restrained translucency because content scrolls behind it.
- Text input owns most of the area and grows with content. `Enter` sends on every device, while `Shift+Enter` inserts a newline; IME composition Enter is never treated as submit.
- The default surface exposes one tools entry and one send action. Kind, priority, and acknowledgement live in an upward disclosure so default note/normal messages do not carry permanent form chrome.
- The send action becomes visually active only when content exists. Native selects remain native for keyboard and mobile reliability, but share the same disclosed control shell and focus treatment.
- Focus lifts the composer by one pixel and strengthens its edge/shadow; opening options, reply/edit context, and newly retained messages use short transform-and-opacity transitions.
- Each Project/Session pair owns a tab-scoped draft. Switching Sessions and refreshing the page restores that draft; editing an existing retained message temporarily replaces the field and restores the draft when editing ends.

### Browser storage and credentials

- Web Storage is an intentional Runtime Console dependency. Non-secret UI preferences such as language, appearance, workspace view, and disclosure state may use `localStorage` so refreshes preserve the workspace.
- When **Keep me signed in for this tab** is enabled, the Runtime Bearer credential is stored only in tab-scoped `sessionStorage`; it is never written to `localStorage` or cookies. **Lock** removes it, and closing the tab ends that browser-session storage.
- Session drafts use `sessionStorage`. Current-tab message presentation hints stay in page memory and may disappear on refresh or Session switching. Neither value becomes authentication, authorization, durable author provenance, or Workflow Session identity.
- Web Storage is readable by same-origin page script. On a shared or untrusted browser profile, disable credential retention or use **Lock** before handing the browser to another person.

### Runtime and Session context

- The primary navigation exposes two task spaces: **Projects & Sessions** and **Runtime & Agents**. Server metrics, Runner diagnostics, Agent identity, inboxes, and durable conversations live in the second space instead of competing with a Session conversation.
- The Session context inspector contains only evidence about the selected Session: identity, workspace path, validation, reported progress, and activity. Current work and attention stay near the top while raw evidence is disclosed one level deeper.
- At `1600 px` and above, the otherwise empty right-side remainder docks a Session context rail by default, but remains user-collapsible. Below that breakpoint context is a non-blocking popover, then a full-width edge sheet on smaller screens.
- Runtime administration is a scrollable card grid with stable anchors for overview, Runner fleet, and Agent communication. On narrow screens it becomes a single column; it never shares the conversational composer or message canvas.

## Responsive behavior

- At 900 px and below, navigation becomes a dismissible drawer and the main conversation remains full width.
- At 1600 px and above, navigation, conversation, and context form a bounded three-column working surface when context is open; the context rail can be collapsed by the user or disappears before it can crowd the primary task.
- At 600 px and below, optional labels collapse, controls wrap without horizontal scrolling, bubbles can use up to 92% width, and safe-area insets are respected.
- At 600 px and below, language, appearance, refresh, and lock move into one labelled overflow menu. The menu closes on outside press and Escape and restores focus to its trigger.
- Coarse pointers receive at least 44 px targets.
- `prefers-reduced-motion`, `prefers-reduced-transparency`, `prefers-contrast`, and forced colors receive explicit fallbacks.

## Motion

- Press feedback: `transform: scale(.97)` for 100–140 ms.
- Small popovers: opacity plus a scale starting near `.98`, 140–180 ms, with origin at the trigger.
- Drawers: transform plus opacity, no layout-property animation.
- Composer growth follows content immediately to avoid lag, while material/focus state transitions over 220–280 ms. A newly available send action uses one short scale settle, never a repeating pulse.
- Only newly observed collaboration messages enter with motion; polling must not replay animation on the retained transcript.
- Routine list selection does not animate spatially.
