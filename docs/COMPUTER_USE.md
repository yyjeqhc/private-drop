# Computer Use architecture

This page describes the implemented Computer Use capabilities, native-platform design constraints, and extension boundaries. Current authority, scope, retry, and privacy contracts remain defined by [Authentication](AUTH_MODEL.md#computer-observation-and-control-authorization), the model-facing tool definitions, and the Runner implementation.

## Current substrate

Computer Use is semantic-first rather than coordinate-first. The current model-facing path is:

```text
computer_list_targets
  -> computer_list_windows
  -> computer_accessibility_tree / computer_snapshot
  -> computer_control(press|focus)
  -> computer_input_text
```

The current contract deliberately keeps read observation separate from effects. Effects name exact opaque Runner/window/element identities, revalidate native state before acting, and never gain AppleScript, shell, paste, coordinate, or synthetic-input fallbacks. A response lost after possible effect dispatch is `outcome_unknown`; callers observe current UI state and reconcile before deciding whether another effect is safe.

The design preserves these invariants while reducing unnecessary model round-trips. The primary optimization target is redundant observation, not collapsing distinct effects into opaque workflows.

### Windows and macOS application discovery and bounded launch

Windows and macOS expose the same two narrow model-facing operations: `computer_list_applications(client_id, limit?)` and `computer_launch_application(client_id, application_id)`. Discovery is bounded to at most 64 returned entries and reports `applications`, `count`, and `truncated`; each entry contains only a fresh opaque `application_id` plus a bounded display name. Windows uses the AppsFolder Shell namespace. macOS asks Foundation for the OS-defined user/local/system application and administration-application directories, then performs a bounded breadth-first enumeration with depth 2 and the shared 1024-entry native scan ceiling. macOS skips symlinks, never escapes a canonical application root, and treats every `.app` candidate as a leaf without traversing bundle contents. Neither implementation scans `PATH`, invokes a process/shell or AppleScript, or recursively scans arbitrary filesystems. There is no pagination, search DSL, executable-path result, package/AUMID/bundle result, or generic application framework.

Every fresh list replaces the Runner's process-local application registry, so IDs from an earlier generation become stale. Runner restart also invalidates every prior ID. The private Windows record stores the exact AppsFolder PIDL. The private macOS record stores the canonical application URL/path, bundle identifier/display/executable metadata, and `stat` identity for the bundle directory, `Contents/Info.plist`, and executable. Immediately before launch, Windows performs another bounded AppsFolder enumeration and requires an exact PIDL match; macOS reobtains the Foundation application roots and reconstructs the same bounded bundle/filesystem identity at the canonical path. Disappearance, ambiguity, metadata change, executable change, or same-path replacement returns `stale_application` before native launch. These private identities never cross the Runner boundary.

Launch accepts only `client_id` and the opaque `application_id`. Windows submits the freshly revalidated Shell item by PIDL. macOS submits the freshly revalidated application URL to public `NSWorkspace.openApplication(at:configuration:completionHandler:)` exactly once with activation, prompting/error UI, Recent Items, hide/hide-others, forced new instance, and alternate-running-copy substitution disabled; arguments and environment are explicitly empty, and no document, URL target, or custom Apple event is supplied. It accepts no caller executable/path/bundle ID, argv, cwd, environment, shell command, PowerShell/cmd/script, URL/protocol launcher, `run_process` fallback, focus, activation, or input. The launch request is not a promise to create a new process, window, or instance: either platform may route it to the exact already-running application. Success means only that the native launch request completed successfully; it does not mean a usable window is ready, and WebCodex does not additionally activate or focus anything.

The canonical workflow is therefore:

```text
computer_list_applications
  -> computer_launch_application
  -> fresh computer_list_windows
  -> identify the exact new/current surface
  -> computer_activate_window only if needed
  -> semantic work
```

Pre-native validation failures, including malformed or `stale_application` IDs and definite non-dispatch, are `not_started` with `state_changed=false`. The asynchronous macOS request has a five-second completion bound: only an unambiguous `(application, no error)` callback is success. Once either native launch may have been dispatched, timeout, callback/response loss, Runner interruption, native failure after submission, ambiguous completion, or inconsistent success metadata is `outcome_unknown`; there is no automatic replay or launch idempotency key, and callers must reconcile with a fresh `computer_list_windows` before deciding whether another launch is safe. The two additive Runner capabilities, `computer_application_discovery` and `computer_application_launch`, default false when missing, are advertised by native Windows and macOS Runners, and never imply one another or any existing observe/control capability. Other platforms fail closed.

Durable audit keeps discovery result metadata to `count`/`truncated` only; application names and native identities are omitted. Launch audit may retain the opaque `application_id` plus bounded lifecycle/success metadata, never the native PIDL, application URL/path, executable path, bundle/AUMID/package/filesystem identity, or launch parameters.

### Full-display discovery and exact display snapshot

Full-display observation is a wider privacy authority than ordinary window observation. `computer_list_displays(client_id, limit?)` and `computer_snapshot_display(client_id, display_id, max_width?, max_height?)` therefore require both `computer:read` and the explicit `computer:display_read` scope, plus the independent Runner capability `computer_display_observe`. Missing capability fields are false and are never inferred from `computer_observe`, region snapshot support, or platform identity. Exact native implementations are available on Windows and macOS; other unproven backends keep the capability false and fail closed.

Discovery returns at most 16 entries with only `display_id`, display-relative source-pixel `width`/`height`, and `primary`, plus `count`/`truncated`. Every list creates fresh opaque process-local IDs and replaces the prior display registry, so previous IDs become stale and Runner restart invalidates all of them. On macOS this source geometry is the current `CGDisplayMode` backing/source pixel geometry from `pixelWidth`/`pixelHeight`; on Retina/HiDPI configurations it can therefore be larger than Quartz display bounds or UI logical/"looks like" dimensions. Native monitor/interface identity, device paths, logical/global desktop coordinates, scale/DPI mapping, and other topology remain Runner-private.

`computer_snapshot_display` captures exactly one previously discovered display; there is no virtual-desktop mosaic, caller region, global coordinate, pointer/click input, activation, or fallback. On Windows the Runner re-enumerates native displays and requires an exact private monitor-interface identity plus unchanged display-relative source geometry. On macOS it binds the public ColorSync display UUID and bounded CoreGraphics device identity to the discovery-time native display id and current-mode source pixel geometry, rejects ambiguous identity or a backing-mode geometry change, and uses the no-UI Screen Recording preflight before exact-display capture. The resulting `CGImage` pixel width/height must exactly equal those bound source pixels. Both implementations revalidate identity again after capture so a hotplug/replacement/mode-change race discards the bytes rather than accepting the wrong display. `max_width`/`max_height` reuse the existing bounded JPEG pipeline and only apply aspect-preserving downscale; they never upscale. The output separates `source_width`/`source_height` from encoded `width`/`height`; logical/global display coordinates, scale/DPI, native topology, and any future source-pixel-to-native-pointer mapping remain Runner-private.

Each successful snapshot returns a positive process-local `snapshot_generation`. The Runner keeps a bounded binding from that generation to the exact opaque display handle, private native identity, and source geometry. Display list/snapshot remain read-only observations: malformed, stale, permission, capture, and transport failures do not use effect/outcome-unknown semantics; callers may safely reacquire a fresh display list and observe again.

Durable audit keeps display-list results to minimal `count`/`truncated` metadata. Display snapshot audit may retain the opaque `display_id`, generation, source/encoded dimensions, MIME, digest, byte count, and capture timestamp, but never the image body, native monitor identity/device path, global origin, scale/DPI, or topology. Model/Server output validation rejects Runner fields outside the closed public shape.

### macOS and Windows snapshot-fenced coordinate pointer

macOS and Windows expose the same `computer_pointer_move(client_id, display_id, snapshot_generation, x, y)` and `computer_pointer_click(client_id, display_id, snapshot_generation, x, y)` surface. The click slice is fixed to one left click; there is no caller button, double/right/middle/X click, drag, wheel, button-down/up primitive, window-relative coordinate API, or global desktop coordinate API. `x`/`y` are integer coordinates in the exact display-local `source_width`/`source_height` space returned by `computer_snapshot_display`, never the downscaled JPEG dimensions.

The `snapshot_generation` is a one-effect freshness fence. Pointer admission requires the latest successful generation for the current process-local `display_id`, private native identity, and source geometry, and the generation must still be unspent. A newer snapshot, fresh display list, Runner restart, display replacement/hotplug, geometry mismatch, or prior pointer use makes it stale. Platform mapping, exact native revalidation, permission and initial shared-input checks, and all fallible native event construction complete before the generation is spent. Crossing the shared effect-admission boundary marks the generation spent before the first native pointer post. A later final pre-post fence can therefore return a spent `not_started` only when it proves that no native pointer event was posted; after any native post, an unprovable result is `outcome_unknown`. Either spent result requires a fresh `computer_snapshot_display` and the old generation is never restored or retried.

The Windows backend re-enumerates and exactly revalidates the private display identity and geometry, then maps display-local source coordinates into Windows virtual-desktop absolute input coordinates. The xcap monitor rectangle union must exactly equal Windows virtual-desktop metrics or the effect fails closed. A move is one absolute virtual-desktop `SendInput`; a click first submits that move, proves the exact cursor with `GetCursorPos`, performs a fresh held-state check, and only then submits one bounded `LEFTDOWN` + `LEFTUP`. Zero move insertion after generation spend is definite `not_started`; partial insertion or any unprovable postcondition is `outcome_unknown`.

The macOS backend supports only exact 0-degree display rotation. It maps source pixels independently per axis as `global_origin + (source_coordinate / source_extent) * global_bounds_extent` using the exact current `CGDisplayBounds`, without hard-coded scale, integer rounding, encoded-image dimensions, or `CGDisplayPixelsWide/High`. It revalidates the same private display identity/source geometry and exact bounds/rotation before preparation and again immediately before the first native post. Both the initial and final held-state guards read `CombinedSessionState`: all 32 Quartz mouse-button slots must be up for move/click, and click additionally requires Shift/Control/Option/Command inactive. Event-post permission uses `CGPreflightPostEventAccess` without requesting permission.

macOS prepares one `CombinedSessionState` `CGEventSource` plus the complete fixed event set before generation spend: exactly one `MouseMoved` for move, or one `MouseMoved` plus one `LeftMouseDown` and one `LeftMouseUp` for click. Dispatch posts only at the HID event tap and has no warp, NSEvent injection, AppleScript, fallback, repair, or retry. After the single `MouseMoved` post, the Runner uses a short monotonic bounded settle window with repeated read-only public Quartz cursor-location observations (`CGEventCreate(NULL)` plus `CGEventGetLocation`); the first observation that exactly equals the prepared fractional global target proves the move, with no epsilon or rounding. Click then performs a second fresh held-state proof before posting down/up. After `LeftMouseUp`, the same bounded exact observation settle must again prove the cursor target before the shared left-button state is required to be up. Quartz posting exposes no insertion-count acknowledgement, so exhaustion without an exact observation, interruption, dirty second click preflight, or any failed final proof after a post is `outcome_unknown`; button events are not attempted when the move proof or second held-state proof fails.

Pointer input uses the shared interactive desktop and does not isolate the agent from simultaneous human input. The Runner never releases or repairs human-held mouse/modifier state. Public results and durable audit contain only the existing opaque display/generation/source-coordinate lifecycle metadata; native display identity, bounds/origin, rotation, global target, scale/mapping, cursor readback coordinates, event source, and raw held-input state remain Runner-private.

Canonical pointer flow:

```text
computer_list_displays
  -> computer_snapshot_display
  -> model computes source-space x/y
  -> computer_pointer_move OR computer_pointer_click
  -> fresh computer_snapshot_display / semantic observation
  -> decide next effect
```

Pointer control requires all four scopes `computer:read`, `computer:display_read`, `computer:control`, and explicit `computer:pointer_control`, plus the independent `computer_pointer_control` Runner capability. The capability defaults false and is advertised by the native macOS and Windows implementations only. The tools perform no implicit snapshot, display/window listing, activation, focus, retry, shell/script/process fallback, clipboard operation, OCR, or browser automation.

### Windows and macOS bounded Unicode-text clipboard

Windows and macOS expose the same two independent global clipboard tools: `computer_read_clipboard(client_id)` and `computer_write_clipboard(client_id, text)`. The native plain-text representation is `CF_UNICODETEXT` on Windows and `NSPasteboardTypeString` on the macOS general pasteboard. Neither path exposes binary/image/HTML/RTF/file-drop/file-URL/custom formats, native format IDs, clipboard history, delayed rendering, owner/window handles, change or sequence numbers, or arbitrary format enumeration. There is no clipboard generation/token framework. The independent Runner capabilities `computer_clipboard_read` and `computer_clipboard_write` default false, are not inferred from each other or from ordinary Computer observation/control, and are advertised by native Windows and macOS implementations only.

Clipboard read is a pure observation requiring both `computer:read` and explicit `computer:clipboard_read`. Windows opens the global clipboard only for one bounded read, performs no internal retry, checks `CF_UNICODETEXT`, bounds native storage before locking it, requires a UTF-16 terminating NUL within that storage, and converts to UTF-8. macOS requests only `NSPasteboardTypeString` from `NSPasteboard.generalPasteboard`; it does not enumerate or request other types and checks the native string's UTF-8 byte length before copying it into the result. Both paths reject rather than truncate text beyond 16 KiB. Absence of readable plain Unicode text is `available=false`; an existing empty string is `available=true` with empty `text`. The returned model text is the authorized data plane, but durable audit retains only bounded metadata such as `available` and `text_bytes`: clipboard text, native string/storage bytes, hashes, handles, owners, and change counts never enter durable audit.

Clipboard write is an independent effect requiring both `computer:control` and explicit `computer:clipboard_write`; write authority does not grant read authority. Caller text must be non-empty, NUL-free, and at most 16 KiB of UTF-8. On Windows, UTF-16 conversion, terminating NUL construction, checked allocation sizing, movable global-memory allocation/copy, and a short-lived invisible Runner-owned non-NULL message-only HWND all complete before mutation. After `OpenClipboard(owner_hwnd)` succeeds, the first successful `EmptyClipboard` is the effect boundary: failure before it is definite `not_started`; `SetClipboardData` or close failure after it is `outcome_unknown`. Successful `SetClipboardData(CF_UNICODETEXT, ...)` transfers HGLOBAL ownership to Windows.

On macOS, validation plus `NSString`, pasteboard, and type preparation complete before mutation. Calling `clearContents` is the native effect boundary and returns the ownership change count; `setString(_:forType: NSPasteboardTypeString)` must succeed and the change count must still match before completion is proven. Any definite failure before `clearContents` is `not_started`; once clear has been called, a failed set, ownership change, lost response, or otherwise unprovable replacement is `outcome_unknown`. Neither platform retries, restores previous content, performs hidden body readback, submits a second write, or repairs an uncertain result.

`computer_write_clipboard` is intentionally a **replacement** operation: the native clear removes any previous image, rich-text, HTML/RTF, file-drop/file-URL, or custom formats before installing only the bounded plain Unicode text. It does not merge or preserve rich formats. Neither clipboard tool pastes anything, sends a paste shortcut, focuses/activates a target, or becomes a fallback for `computer_input_text`, key input, semantic control, or pointer operations. An `outcome_unknown` write is never blindly retried; a caller that separately holds clipboard-read authority may explicitly issue a fresh `computer_read_clipboard`, while a caller without that read scope must retain the unknown outcome rather than bypass authorization. Live shared-clipboard validation on macOS remains a validation gap; native compilation and non-mutating tests alone do not establish that behavior.

## Semantic operations

### Semantic element finder

`computer_find_elements` provides bounded read-only search over the Accessibility observation path.

The Control-side adapter uses the canonical bounded Accessibility tree. It reuses `computer:read`, exact `client_id`/`surface_id`, and the existing `computer_accessibility_observe` capability. Return only deterministic bounded matches and fresh ephemeral `element_id` values already produced by the underlying observation.

Filters remain closed and simple: role/subrole, literal semantic label text, focused, enabled, and limit. Do not add regex, fuzzy scoring, a query DSL, arbitrary AXValue search, or effect behavior. If future measurements show material transport/latency cost from fetching the bounded tree internally, the same semantics may be moved down to the Runner behind an additive capability.

Required behavior:

- Common known-element tasks no longer require the model to read and parse a full AX tree.
- Finder results preserve existing protected/secure-content behavior and tree bounds.
- Traversal order and truncation are deterministic and explicit.
- A stale result remains an ephemeral handle, not a durable bearer reference.

### Exact window activation

`computer_activate_window` activates/raises an already observed exact `surface_id`.

The operation must revalidate the exact native surface and may not accept an application name, PID guess, executable path, bundle path, shell command, AppleScript, or fallback to another window. Lost-response behavior follows the existing Computer effect contract.

This is intentionally separate from application launch. It solves the observed failure mode where an exact element is known but its window is not frontmost without widening Computer into a process launcher.

### Normalized element state and observation generation

Expose low-cost read-only state needed to choose the next semantic action without returning sensitive values. Prefer normalized WebCodex affordances over platform-specific AX vocabulary, for example:

```text
role / subrole
enabled
focused
value_empty
protected
can_press
can_focus
can_input_text
```

macOS AX and Windows UI Automation map into the same model-facing vocabulary.

Record an observation generation (and observation time where useful) with element registrations. Keep lineage/fingerprint re-resolution as the authoritative semantic stale check. Do not impose one aggressively short wall-clock TTL on all current AX effects merely to add a timer; future geometry/focus-sensitive effects may require a fresher generation than semantic press/text effects.

The implementation uses a positive process-local generation per observed surface and does not add a wall-clock TTL or observation timestamp. `computer_accessibility_tree` and the Control-side finder expose the generation attached to their fresh element handles. `computer_element_state(surface_id, element_id)` re-resolves an existing handle and reports that same generation; it neither creates a new handle nor advances generation. `value_empty` is actionable only for supported unprotected text input and is `null` for protected/secure content or when unavailable.

Stale recovery should remain re-observation, normally:

```text
stale_element -> computer_find_elements(...) -> new element_id
stale_surface -> computer_list_windows -> new surface_id
```

The model-facing failure envelope now makes the same fact explicit as `recovery_kind=reobserve` with a bounded `recovery_tool` when the target is known. The current high-value mappings are `stale_element -> computer_find_elements`, `stale_surface -> computer_list_windows`, `stale_application -> computer_list_applications`, and `stale_display -> computer_list_displays`. This metadata is advisory control information only; it does not refresh or upgrade an old opaque identity.

Do not add a `refresh_element` operation that upgrades an old handle into new authority.

### Region snapshot

Extend window snapshot observation with a bounded window-relative region and optional output dimension bounds. The Runner must first revalidate the exact `surface_id`, then capture/crop within that surface; caller coordinates must never be treated as global desktop coordinates.

The operation does not expose caller-controlled image quality or a general screenshot backend. Prefer bounded system-selected encoding for model observation. Return enough metadata to correlate the image with what was captured, including source/output dimensions, captured region, MIME type, byte count, digest, and capture time when available.

The implementation keeps `computer_snapshot` as the single model-facing operation. Calls with only `client_id` + `surface_id` retain the existing `computer_snapshot` wire and `computer_observe` capability for rolling compatibility. Supplying `region`, `max_width`, or `max_height` switches internally to the additive `computer_snapshot_region` Runner wire and requires both the baseline `computer_observe` capability and the additive `computer_snapshot_region` capability, so an older Runner can still serve whole-window snapshots but fails closed for the new transform. Region coordinates remain in the revalidated surface coordinate space; the Runner maps them into captured pixels, crops inside the bounded image, applies aspect-preserving downscale only when requested, and keeps JPEG encoding/quality system-selected.

### Snapshot to project artifact

Use a separate `computer_save_snapshot(project, path, client_id, surface_id, region?, max_width?, max_height?, session_id?)` operation rather than a `save=true` flag on observation. The Control reuses the window/region snapshot capture path and then sends the validated bounded image directly to the target project's existing artifact write path; the model never receives or resubmits the Base64 image body. Whole-window capture retains the existing rolling-compatible `computer_observe` path, while region/downscale capture still additionally requires `computer_snapshot_region`; saving a snapshot adds no new Runner Computer protocol capability.

The artifact write is create-only: callers cannot request overwrite, encoding format, quality, or arbitrary content. The target path is validated by the existing project-artifact policy, and the target Runner must still advertise `file_write` when the request is admitted under the registry lock. The source Computer Runner and target project Runner may be different devices.

This operation crosses Computer observation and project artifact authority and therefore requires both `computer:read` and `project:write`. Capture remains observational, but artifact persistence is an effect. A definite pre-dispatch rejection reports `not_started`; if a write may have been dispatched and its result is lost or inconsistent, the result is `outcome_unknown` and includes the exact project/path plus expected digest, byte count, and MIME. Reconcile with `read_project_artifact_metadata` before deciding whether another create-only attempt is safe; never blind-retry by overwriting the path.
The uncertain artifact-write response also exposes `recovery_kind=reconcile` and `recovery_tool=read_project_artifact_metadata` while retaining `execution_state=outcome_unknown`, the exact project/path, expected digest/byte-count/MIME evidence, and the existing `reconcile_with` compatibility field.

### Bounded scroll

Semantic scrolling uses `computer_scroll_to_element(client_id, surface_id, element_id)`, a distinct model-facing effect with its own additive Runner capability and wire kind. It is not an action value on `computer_control`, and an older Runner that advertises only `computer_control` must fail closed rather than being treated as scroll-capable.

On macOS the Runner keeps the existing exact surface/element correlation and protected-content fences, re-resolves the ephemeral Accessibility handle, verifies native `AXScrollToVisible` support, and performs only that semantic action. The caller supplies no wheel delta, direction, distance, or coordinates. Unsupported targets fail deterministically; a response lost after dispatch remains `outcome_unknown`, so current UI state must be observed before retrying.

No lower-level wheel fallback is added by semantic scrolling. If a concrete product need emerges later, it must remain separately bounded and tied to an exactly revalidated active/focused surface rather than widening this semantic contract.

### Closed key input

`computer_key_input(client_id, surface_id, key, modifiers?)` is a distinct effect with its own additive Runner capability and wire kind. It is not another `computer_control` action, and an older Runner that only advertises `computer_control` fails closed. The model-facing vocabulary is deliberately limited to Enter, Escape, Tab, arrows, PageUp/PageDown, Home/End plus the bounded `shift`, `control`, `option`, and `command` modifier set. Ordinary text continues through `computer_input_text`; callers cannot supply characters, virtual keycodes, repeat counts, held-key state, or modifier key-down/up sequences.

On macOS the Runner revalidates the exact surface through the existing native window identity path, then requires its application to already be frontmost and `AXFocusedWindow` to equal that exact window. It never activates or focuses the surface implicitly. When an `AXFocusedUIElement` is available, explicitly protected or secure content fails closed. Accessibility and event-posting permission checks are preflight-only and do not request permission UI. Both key-down and key-up events are constructed and flagged before the first effect, then posted only to the exact surface PID. Quartz exposes no atomic/batch post for the pair, so the two posts are adjacent with no fallible work between them; if the Runner process terminates in that narrow interval, the result is a partial `outcome_unknown`, not a supported held-key state.

The effect keeps the existing Computer delivery fence: definite non-dispatch is `not_started`; possible dispatch, lost response, partial key-pair delivery, or inconsistent success metadata is `outcome_unknown` and requires UI re-observation before retry. There is no clipboard, paste, AppleScript, shell, pointer, or global-key fallback.

### Windows UIA — read-only semantic observation

Windows UIA supports the read-only `computer_accessibility_status`, `computer_accessibility_tree`, `computer_find_elements`, and `computer_element_state` model surface. It does not add `windows_*` tools or grant any Windows Computer effect capability.

The Windows Runner revalidates the exact xcap-observed HWND/PID before obtaining the UIA root, walks the UIA Control View with the same depth/node bounds, and registers the same ephemeral `element_id` plus observation-generation lineage used by macOS. The native client uses `CUIAutomation8` / `IUIAutomation2` with bounded connection/transaction timeouts plus a top-level observation deadline, so a stalled provider does not turn node bounds into an unbounded call. UIA control types are projected into the existing semantic role vocabulary where a stable equivalent exists. Password/protected elements suppress value observation.

`computer_find_elements` remains a Control-side adapter over the canonical bounded tree, so Windows support adds no finder-specific Runner protocol. `computer_element_state` re-resolves the exact UIA lineage before returning normalized state. Mutation affordances reflect the revalidated native element state and supported UIA patterns; observation alone does not grant an effect capability or authorize mutation.

### Windows UIA — exact window activation

Windows reuses the existing `computer_activate_window` effect and its independent `computer_window_activate` capability. The caller supplies only an exact previously observed `surface_id`; there is no app-name, PID, title, executable-path, command, or fallback target.

Immediately before the effect the Runner revalidates the xcap surface identity, exact native HWND/PID, and UIA root. Non-Window UIA roots fail closed before any effect. An already-foreground non-minimized window is a no-op success. A minimized exact window is restored with `ShowWindowAsync(SW_RESTORE)` plus a bounded local-state wait so a stalled foreign UI thread cannot block the Runner; then the exact UIA Window root receives `IUIAutomationElement::SetFocus`. Success still requires `GetForegroundWindow()` to equal the exact HWND. Once either restore or UIA focus has been attempted, any native error, timeout, or mismatched foreground postcondition is `outcome_unknown` and requires fresh observation before any retry.

A background Runner is normally denied by `SetForegroundWindow`, so the implementation does not pretend that API provides parity and does not bypass the policy with `AttachThreadInput`, synthetic Alt/key input, app/PID selection, or generic automation fallback. Exact UIA focus is the native automation path. Shared post-dispatch delivery fencing remains authoritative when the Runner response itself is lost.

### Windows UIA — semantic press and focus

Windows reuses the existing `computer_control(surface_id, element_id, action)` contract for the same closed `press|focus` vocabulary. `press` requires the exact re-resolved UIA element to expose `InvokePattern` and calls only `IUIAutomationInvokePattern::Invoke`. The first bounded Windows `focus` slice is narrower: the exact surface must already be foreground and the exact element must normalize to `AXTextField`, be enabled, unprotected, and keyboard-focusable before `IUIAutomationElement::SetFocus` is attempted. Buttons and other controls are not treated as reliable exact-focus targets merely because a provider reports keyboard-focusability; their semantic action remains `press` when `InvokePattern` is available. Focus never activates a background window implicitly. No Windows-specific model tool, LegacyIAccessible fallback, coordinate click, `SendInput`, script, shell, or generic action path is added.

`computer_element_state` now derives `can_press` from live `InvokePattern` support and exposes Windows `can_focus=true` only for an enabled, unprotected, keyboard-focusable `AXTextField` on the exact foreground surface; Windows `can_input_text` remains false until the separate text-input parity slice. Focus state/read-back uses `IUIAutomation::GetFocusedElement` plus `CompareElements` against the exact re-resolved element rather than trusting a provider-local focus flag. Protected or disabled targets, background-surface focus, roles outside the bounded focus set, and missing patterns fail closed before an effect. Once `Invoke` or `SetFocus` has been attempted, native failure, deadline loss, or a failed bounded exact-focus read-back is `outcome_unknown`; callers must re-observe before considering another effect.

### Windows UIA — bounded text input

Windows reuses `computer_input_text(surface_id, element_id, text)` and its independent `computer_text_input` capability. The first Windows mutation slice remains closed to an exact normalized `AXTextField`: the exact surface must already be foreground, the exact re-resolved element must already hold UIA keyboard focus, and the target must be enabled, unprotected/non-password, positively correlated, expose a writable `ValuePattern`, and have an empty current value. Caller text keeps the existing non-empty, NUL-free, 2048-byte UTF-8 bound and is passed verbatim only to `IUIAutomationValuePattern::SetValue`. There is no implicit activation/focus, key event, paste, clipboard, script, shell, coordinate, or generic automation fallback.

`computer_element_state.can_input_text` reflects the same writable/foreground/focused/empty predicate; the current value itself never leaves the Runner. Emptiness is re-read immediately before `SetValue`, so a non-empty field fails closed instead of being overwritten. Once `SetValue` has been attempted, any native HRESULT failure is `outcome_unknown`; callers must re-observe the exact element before considering another write. Successful responses contain only bounded metadata such as `text_bytes`, never caller text or field contents.

### Windows UIA — semantic scroll

Windows reuses `computer_scroll_to_element(surface_id, element_id)` and its independent `computer_scroll_to_element` capability. Immediately before the effect the Runner revalidates the exact xcap surface, HWND/PID UIA root, and the complete RuntimeId-bearing root-to-target lineage, rejects protected/password lineage, and requires the exact element to expose `ScrollItemPattern`. The only native effect is `IUIAutomationScrollItemPattern::ScrollIntoView`; there is no wheel, delta/percentage `ScrollPattern`, `SendInput`, coordinate, pointer, LegacyIAccessible, script, shell, or application-specific fallback.

Missing `ScrollItemPattern` and other pre-effect validation failures are deterministic. Once `ScrollIntoView` has been invoked, any HRESULT failure or deadline loss is `outcome_unknown`; callers must make a fresh semantic observation before deciding whether another scroll is safe. The result remains closed metadata containing only platform, surface/element identity, and success.

### Windows UIA — closed key input

Windows reuses `computer_key_input(surface_id, key, modifiers?)` and its independent `computer_key_input` capability. Immediately before input the Runner revalidates the exact xcap surface, HWND/PID UIA root, requires `GetForegroundWindow()` to equal that HWND, obtains the current focused UIA element, and walks a bounded Control View ancestry chain until it proves that focus belongs to the exact window root. Password/protected focused content or ancestry fails closed. The Runner performs this exact foreground/root/focus preflight again immediately before native input and never activates or focuses a window implicitly.

The existing closed key vocabulary is mapped to native Windows virtual keys internally. `shift` maps to Shift, `control` to Control, and model-facing `option` to Alt. `command` deliberately has no Windows mapping and fails before any native input. Windows also rejects `option+Tab`, `option+Escape`, and any `control+Escape` chord before native input because those combinations invoke OS-level switching or shell UI outside the exact surface. The full bounded `INPUT` sequence is constructed before the first effect: modifier downs, one key down/up pair, then modifier ups in reverse order. Navigation keys that require it use the internally selected extended-key flag. The entire sequence is sent with one `SendInput` call and the returned inserted-event count must exactly equal the prepared sequence length. Immediately before that call, the Runner also fails closed if Shift, Control, Alt, either Windows key, or the requested target key is already physically down, because existing keyboard state can otherwise change the effective chord.

Windows `SendInput` uses the shared interactive-desktop input stream; the exact surface/focus checks and held-key guard bound the action but do not make it a concurrent-user isolation primitive. Do not assume Windows key input can run independently while a person is simultaneously changing focus or keyboard state on the same desktop. Before `SendInput`, deterministic foreground/focus/mapping/held-key failures are pre-effect. A zero inserted-event return is also deterministic because no keyboard event entered the input stream. Partial insertion, deadline loss after insertion, interruption, lost response, or inconsistent success metadata is `outcome_unknown`; callers must re-observe before considering a retry. There is no PostMessage/SendMessage, `keybd_event`, caller virtual-key/scan-code control, clipboard/paste, script/shell, coordinate/pointer fallback, implicit activation, or cross-call held-key state. Successful output remains closed metadata containing only platform, surface identity, key, modifiers, and success.

## Cross-platform design

Windows UI Automation and macOS AX map into the same WebCodex semantic
action/affordance vocabulary. Keep platform-specific identity and effect checks
inside the native backend. Future work should address observed product limitations.

Full-display observation receives separate authority from single-window observation because it widens the privacy surface. Windows and macOS share the same exact-display discovery, snapshot-generation, and bounded JPEG contract. macOS display/source geometry is the current display mode's backing pixel space, not Quartz logical bounds; the native macOS pointer path privately maps those source pixels into Quartz global coordinates without exposing scale/DPI/topology, while Windows maps them into virtual-desktop absolute input coordinates. Pointer actions on both platforms depend on a fresh snapshot generation, exact display-local geometry, native-coordinate revalidation, and post-effect reconciliation. Windows and macOS clipboard read/write share the same separately scoped bounded global authority while retaining platform-native plain-text representations and effect boundaries.

## Explicitly deferred

Do not introduce these as shortcuts when extending Computer Use:

- a natural-language `computer_do_everything` or generic automation command;
- implicit focus/send/paste/retry flags on existing effects;
- blind retry after `outcome_unknown`;
- arbitrary application/executable paths or shell/AppleScript fallback;
- global desktop coordinates for pointer or region input;
- a general plugin/callback framework for Computer backends;
- browser tasks forced through desktop coordinates when a future DOM/ARIA-native browser surface is the better substrate.

Browser-native automation may become a separate branch of Computer Use later, sharing high-level safety principles without forcing DOM/ARIA semantics into the desktop AX/UIA backend.

## Validation loop

Keep a small set of representative scenarios and use them to reorder work when repeated friction is demonstrated:

1. Edge: find an empty search/text field, focus it, input text, verify semantically.
2. Messaging app: activate an observed window, find a contact/search target, navigate to a conversation, enter a message, explicitly send, then verify.
3. Unfamiliar UI: start with semantic observation and use snapshot only when semantics are insufficient.
4. Stale element: recover by semantic re-observation without reviving the old handle.
5. Unknown effect outcome: observe and reconcile without blind retry.

Track interaction quality by observation economy rather than an absolute call-count target. A healthy happy path should not repeatedly fetch full trees or screenshots, while distinct effects such as focus, text input, press, activation, and scroll/key input remain explicit when they have different outcome semantics.

For every new Computer feature:

1. define observation/effect semantics, stale behavior, timeout/lost-response handling, bounds, and explicit non-goals;
2. add only the capability/scope distinction required by the new authority surface, with missing old-Runner fields failing closed;
3. close the full Server/protocol/Runner/model projection before local hardening;
4. revalidate exact native surface/element identity at the Runner immediately before an effect;
5. keep sensitive screenshot/text bodies out of audit metadata;
6. test stale handles, replacement/races, permission revocation, protected/secure content, response loss, process restart, and platform geometry where relevant;
7. run live platform validation for new native effects before treating them as production-ready.
