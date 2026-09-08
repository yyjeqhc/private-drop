import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  initialRuntimeConsoleState,
  runtimeDeviceIds,
  runtimeProjectsForDevice,
  filterAndSortRuntimeProjects,
  runtimeProjectIdentityText,
  preferredRuntimeProjectSelection,
  runtimeCommunicationTranscriptAfterSeq,
  runtimeWorkflowSessionSummaryRevision,
  runtimeWorkflowSessionSummaryChanged,
  beginRuntimeCredential,
  refreshRuntimeOverview,
  isCurrentRuntimeOverviewRequest,
  refreshRuntimeProjects,
  isCurrentRuntimeProjectsRequest,
  refreshRuntimeRunner,
  isCurrentRuntimeRunnerRequest,
  selectRuntimeRunnerFilter,
  selectRuntimeProject,
  refreshRuntimeSessionList,
  isCurrentRuntimeSessionListRequest,
  selectRuntimeWorkflowSession,
  selectRuntimeSessionLocation,
  refreshRuntimeWorkflowSession,
  isCurrentRuntimeWorkflowSessionRequest,
  adoptRuntimeWorkflowSessionDetail,
  runtimeCollaborationRequest,
  isCurrentRuntimeCollaborationRequest,
  adoptRuntimeCollaborationList,
  adoptRuntimeCollaborationObservation,
  setRuntimeCollaborationAvailable,
  setRuntimeCollaborationPhase,
  runtimeCollaborationNeedsRefreshRecovery,
  mergeRuntimeCollaborationMessages,
  runtimeCollaborationObservationAction,
  runtimeCollaborationMessageCanMutate,
  runtimeCollaborationMessageSides,
  setRuntimeCollaborationReplyTarget,
  setRuntimeCollaborationEditTarget,
  runtimeCollaborationEditTarget,
  markRuntimeCollaborationMutationUncertain,
  runtimeCollaborationMutationRecovery,
  completeRuntimeCollaborationMutationRecovery,
  takeRuntimeCollaborationMutationNotice,
  resolveRunnerDisclosure,
  runtimeWindowShortKey,
  runtimeWindowActivityLabel,
  resolveRuntimeContextState,
  resolveRuntimeContextPresentationMode,
  reduceRuntimeContextUserIntent,
  resolveRuntimeContextFocusTransition,
} from "../dist/runtime_console_state.js";

test("window activity presentation is hashed-id safe and WebCodex-specific", () => {
  assert.equal(runtimeWindowShortKey("0123456789abcdef0123456789abcdef"), "01234567…cdef");
  assert.equal(runtimeWindowShortKey("short"), "short");
  assert.equal(runtimeWindowActivityLabel(null, 10_000), "No WebCodex activity");
  assert.equal(runtimeWindowActivityLabel(9_500, 10_000), "just now");
  assert.equal(runtimeWindowActivityLabel(5_000, 10_000), "5s ago");
  assert.equal(runtimeWindowActivityLabel(0, 10_000), "No WebCodex activity");
});

test("communication transcript window follows the latest bounded page", () => {
  assert.equal(runtimeCommunicationTranscriptAfterSeq(0), 0);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(100), 0);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(101), 1);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(250), 150);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(250, 50), 200);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(-1), 0);
  assert.equal(runtimeCommunicationTranscriptAfterSeq(Number.NaN), 0);
});

test("Workflow Session summary revision changes only for detail-relevant list state", () => {
  const base = {
    session_id: "wc_sess_a",
    title: "Work",
    lifecycle: "active",
    mode: "normal",
    updated_at: 100,
    running_call: false,
    running_jobs: 0,
    running_jobs_complete: true,
    current_activity: null,
    last_activity: { kind: "Edited", summary: "file" },
    overview: { attention: { open_todos: 0 } },
  };
  assert.equal(runtimeWorkflowSessionSummaryRevision(null), "");
  assert.equal(runtimeWorkflowSessionSummaryChanged(base, { ...base }), false);
  assert.equal(runtimeWorkflowSessionSummaryChanged(base, { ...base, updated_at: 101 }), true);
  assert.equal(runtimeWorkflowSessionSummaryChanged(base, { ...base, running_jobs: 1 }), true);
  assert.equal(
    runtimeWorkflowSessionSummaryChanged(base, {
      ...base,
      overview: { attention: { open_todos: 1 } },
    }),
    true
  );
});

test("runtime credential and project generations fence stale project responses", () => {
  const state = initialRuntimeConsoleState();
  const firstProjects = beginRuntimeCredential(state);
  const newerProjects = refreshRuntimeProjects(state);
  assert.equal(firstProjects.clientId, "");
  assert.equal(firstProjects.query, "");
  assert.equal(isCurrentRuntimeProjectsRequest(state, firstProjects), false);
  assert.equal(isCurrentRuntimeProjectsRequest(state, newerProjects), true);

  const listA = selectRuntimeProject(state, "device-a", "agent:a:project");
  assert.equal(state.selectedDevice, "device-a");
  assert.equal(state.selectedProject, "agent:a:project");
  const projectsDuringA = refreshRuntimeProjects(state, "  webcodex  ");
  assert.equal(projectsDuringA.clientId, "device-a");
  assert.equal(projectsDuringA.query, "webcodex");
  const fleetProjectsDuringA = refreshRuntimeProjects(state, "", "");
  assert.equal(fleetProjectsDuringA.clientId, "");
  const refreshedA = refreshRuntimeSessionList(state);
  assert.equal(isCurrentRuntimeSessionListRequest(state, listA), false);
  assert.equal(isCurrentRuntimeSessionListRequest(state, refreshedA), true);

  const listB = selectRuntimeProject(state, "device-b", "agent:b:project");
  assert.equal(state.selectedDevice, "device-b");
  assert.equal(state.selectedProject, "agent:b:project");
  assert.equal(isCurrentRuntimeProjectsRequest(state, projectsDuringA), false);
  assert.equal(isCurrentRuntimeSessionListRequest(state, refreshedA), false);
  assert.equal(isCurrentRuntimeSessionListRequest(state, listB), true);
  assert.equal(state.workflow.selectedSessionId, "");
  assert.equal(state.workflow.snapshot, null);
});

test("server and Runner requests are fenced across credential and Runner changes", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  const overviewA = refreshRuntimeOverview(state);
  const listA = selectRuntimeProject(state, "runner-a", "agent:runner-a:p");
  const runnerA = refreshRuntimeRunner(state);
  assert.equal(isCurrentRuntimeOverviewRequest(state, overviewA), true);
  assert.equal(isCurrentRuntimeRunnerRequest(state, runnerA), true);
  assert.equal(isCurrentRuntimeSessionListRequest(state, listA), true);

  selectRuntimeProject(state, "runner-b", "agent:runner-b:p");
  const runnerB = refreshRuntimeRunner(state);
  assert.equal(isCurrentRuntimeRunnerRequest(state, runnerA), false);
  assert.equal(isCurrentRuntimeRunnerRequest(state, runnerB), true);
  assert.equal(isCurrentRuntimeSessionListRequest(state, listA), false);

  beginRuntimeCredential(state);
  assert.equal(isCurrentRuntimeOverviewRequest(state, overviewA), false);
  assert.equal(isCurrentRuntimeRunnerRequest(state, runnerB), false);
});

test("runtime home defaults to All Runners with no automatic Project selection", () => {
  const projects = [
    { id: "opaque-b", client_id: "device-b", name: "Beta" },
    { id: "agent:not-device-a:project", client_id: "device-a", name: "Zulu" },
    { id: "opaque-a2", client_id: "device-a", name: "Alpha" },
    { id: "opaque-a1", client_id: "device-a", name: "Alpha" },
  ];
  assert.deepEqual(runtimeDeviceIds(projects), ["device-a", "device-b"]);
  assert.deepEqual(runtimeProjectsForDevice(projects, "device-a").map((project) => project.id), ["opaque-a1", "opaque-a2", "agent:not-device-a:project"]);
  assert.deepEqual(runtimeProjectsForDevice(projects, "").map((project) => project.id), ["opaque-a1", "opaque-a2", "opaque-b", "agent:not-device-a:project"]);
  assert.deepEqual(preferredRuntimeProjectSelection(projects, "device-b", "agent:not-device-a:project"), { device: "device-a", project: "agent:not-device-a:project" });
  assert.deepEqual(preferredRuntimeProjectSelection(projects, "device-a", "missing-project"), { device: "device-a", project: "" });
  assert.deepEqual(preferredRuntimeProjectSelection(projects, "", ""), { device: "", project: "" });
});

test("runtime refresh preserves an authorized selected device and project", () => {
  const projects = [
    { id: "project-2", client_id: "device-z", name: "Second" },
    { id: "project-1", client_id: "device-z", name: "First" },
    { id: "project-3", client_id: "device-a", name: "Other" },
  ];
  assert.deepEqual(preferredRuntimeProjectSelection(projects, "device-z", "project-2"), { device: "device-z", project: "project-2" });
});

test("Project list supports All Runners, Runner filter/search, and running attention recent ranking", () => {
  const projects = [
    { id: "agent:r:idle", client_id: "runner", name: "Idle", path: "/root/git/idle", sessions: { running_sessions: 0, attention: {}, latest_updated_at: 100 } },
    { id: "agent:r:recent", client_id: "runner", name: "Recent", path: "/root/git/webcodex-worktrees/recent", sessions: { running_sessions: 0, attention: {}, latest_updated_at: 400 } },
    { id: "agent:r:attention", client_id: "runner", name: "Needs review", sessions: { running_sessions: 0, attention: { open_guidance: 1 }, latest_updated_at: 50 } },
    { id: "agent:r:working", client_id: "runner", name: "Working", sessions: { running_sessions: 1, attention: {}, latest_updated_at: 10 } },
    { id: "agent:other:x", client_id: "other", name: "External" },
  ];
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "runner", "").map((project) => project.id),
    ["agent:r:working", "agent:r:attention", "agent:r:recent", "agent:r:idle"]
  );
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "runner", "REVIEW").map((project) => project.id),
    ["agent:r:attention"]
  );
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "runner", "agent:r:recent").map((project) => project.id),
    ["agent:r:recent"]
  );
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "runner", "webcodex-worktrees").map((project) => project.id),
    ["agent:r:recent"]
  );
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "", "").map((project) => project.id),
    ["agent:r:working", "agent:r:attention", "agent:r:recent", "agent:r:idle", "agent:other:x"]
  );
  assert.deepEqual(
    filterAndSortRuntimeProjects(projects, "", "OTHER").map((project) => project.id),
    ["agent:other:x"]
  );
});

test("Runtime Project identity preserves Linux macOS and Windows workspace paths exactly", () => {
  assert.equal(
    runtimeProjectIdentityText({ id: "agent:special:webcodex", client_id: "special", path: "/root/git/webcodex" }),
    "Runner: special · Project: agent:special:webcodex · Workspace: /root/git/webcodex"
  );
  assert.equal(
    runtimeProjectIdentityText({ id: "agent:mini:webcodex", client_id: "mini", path: "/Users/demo/git/webcodex" }),
    "Runner: mini · Project: agent:mini:webcodex · Workspace: /Users/demo/git/webcodex"
  );
  assert.equal(
    runtimeProjectIdentityText({ id: "agent:msi:webcodex", client_id: "msi", path: "E:\\git\\webcodex" }),
    "Runner: msi · Project: agent:msi:webcodex · Workspace: E:\\git\\webcodex"
  );
});

test("runtime workflow detail identity includes project plus session id", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "device-a", "agent:a:project");
  const detailA = selectRuntimeWorkflowSession(state, "wc_sess_same");
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, detailA), true);
  assert.equal(adoptRuntimeWorkflowSessionDetail(state, detailA, { title: "A" }), true);
  assert.equal(state.workflow.snapshot.title, "A");

  selectRuntimeProject(state, "device-b", "agent:b:project");
  assert.equal(state.workflow.snapshot, null);
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, detailA), false);
  const detailB = selectRuntimeWorkflowSession(state, "wc_sess_same");
  assert.equal(detailB.project, "agent:b:project");
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, detailB), true);
  assert.equal(adoptRuntimeWorkflowSessionDetail(state, detailA, { title: "late A" }), false);
  assert.equal(adoptRuntimeWorkflowSessionDetail(state, detailB, { title: "B" }), true);
  assert.equal(state.workflow.snapshot.title, "B");
});

test("Recent Session navigation atomically establishes Runner Project and Session and fences old collaboration", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner-a", "agent:runner-a:project-a");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const oldDetail = refreshRuntimeWorkflowSession(state);
  const oldCollaboration = runtimeCollaborationRequest(state);

  const location = selectRuntimeSessionLocation(
    state,
    "runner-b",
    "agent:runner-b:project-b",
    "wc_sess_b"
  );
  assert.equal(state.selectedDevice, "runner-b");
  assert.equal(state.selectedProject, "agent:runner-b:project-b");
  assert.equal(state.workflow.selectedSessionId, "wc_sess_b");
  assert.equal(isCurrentRuntimeSessionListRequest(state, location.sessionListRequest), true);
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, location.detailRequest), true);
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, oldDetail), false);
  assert.equal(isCurrentRuntimeCollaborationRequest(state, oldCollaboration), false);
  const currentCollaboration = runtimeCollaborationRequest(state);
  assert.equal(currentCollaboration.project, "agent:runner-b:project-b");
  assert.equal(currentCollaboration.sessionId, "wc_sess_b");
});

test("removed Project clears stale detail while preserving a still-valid Runner filter", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:gone");
  const detail = selectRuntimeWorkflowSession(state, "wc_sess_gone");
  const remaining = [{ id: "agent:runner:remaining", client_id: "runner", name: "Remaining" }];
  const selection = preferredRuntimeProjectSelection(remaining, state.selectedDevice, state.selectedProject);
  assert.deepEqual(selection, { device: "runner", project: "" });
  selectRuntimeRunnerFilter(state, selection.device);
  assert.equal(state.selectedDevice, "runner");
  assert.equal(state.selectedProject, "");
  assert.equal(state.workflow.selectedSessionId, "");
  assert.equal(isCurrentRuntimeWorkflowSessionRequest(state, detail), false);
});

test("session switch invalidates old collaboration responses", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const requestA = runtimeCollaborationRequest(state);
  assert.equal(isCurrentRuntimeCollaborationRequest(state, requestA), true);
  selectRuntimeWorkflowSession(state, "wc_sess_b");
  const requestB = runtimeCollaborationRequest(state);
  assert.equal(isCurrentRuntimeCollaborationRequest(state, requestA), false);
  assert.equal(isCurrentRuntimeCollaborationRequest(state, requestB), true);
  assert.equal(adoptRuntimeCollaborationList(state, requestA, [{ message_id: "wc_msg_old" }]), false);
});

test("collaboration delta replaces message state by id and completion renders todo resolution plus answer", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_todo", kind: "todo", status: "open", created_at: 1, message: "do work" },
  ]);
  assert.equal(adoptRuntimeCollaborationObservation(state, request, {
    observation_token: "opaque-1",
    messages: [
      { message_id: "wc_msg_todo", kind: "todo", status: "resolved", created_at: 1, message: "do work", resolved_by_message_id: "wc_msg_answer" },
      { message_id: "wc_msg_answer", kind: "answer", status: "open", created_at: 2, message: "done", reply_to: "wc_msg_todo", author_session_id: "wc_sess_worker" },
    ],
  }), true);
  assert.equal(state.collaboration.messages.length, 2);
  assert.equal(state.collaboration.messages[0].status, "resolved");
  assert.equal(state.collaboration.messages[1].reply_to, "wc_msg_todo");
  assert.equal(state.collaboration.observationToken, "opaque-1");
});

test("history loss reloads and has_more drains without duplicate message ids", () => {
  assert.equal(runtimeCollaborationObservationAction({ history_lost: true, has_more: true }), "reload");
  assert.equal(runtimeCollaborationObservationAction({ history_lost: false, has_more: true }), "drain");
  assert.equal(runtimeCollaborationObservationAction({ wait_outcome: "timeout" }), "wait");
  const merged = mergeRuntimeCollaborationMessages(
    [{ message_id: "a", created_at: 1, status: "open" }],
    [{ message_id: "a", created_at: 1, status: "resolved" }, { message_id: "b", created_at: 2 }]
  );
  assert.deepEqual(merged.map((message) => message.message_id), ["a", "b"]);
  assert.equal(merged[0].status, "resolved");
});

test("project-read-only degradation keeps project selection while collaboration is marked unavailable", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  assert.equal(setRuntimeCollaborationAvailable(state, request, false), true);
  assert.equal(state.selectedProject, "agent:runner:project");
  assert.equal(state.workflow.selectedSessionId, "wc_sess_a");
  assert.equal(state.collaboration.available, false);
});

test("manual Refresh recovery is required only after collaboration is paused", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const requestA = runtimeCollaborationRequest(state);
  assert.equal(setRuntimeCollaborationPhase(state, requestA, "live"), true);
  assert.equal(runtimeCollaborationNeedsRefreshRecovery(state), false);
  assert.equal(setRuntimeCollaborationPhase(state, requestA, "paused"), true);
  assert.equal(runtimeCollaborationNeedsRefreshRecovery(state), true);
  selectRuntimeWorkflowSession(state, "wc_sess_b");
  assert.equal(state.collaboration.phase, "idle");
  assert.equal(setRuntimeCollaborationPhase(state, requestA, "paused"), false);
  assert.equal(runtimeCollaborationNeedsRefreshRecovery(state), false);
});

test("collaboration Edit and Reply are mutually exclusive and context switches clear edit state", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  let request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_edit", kind: "guidance", status: "open", priority: "high", requires_ack: true, first_ack_observed_at: 10, created_at: 1, message: "old" },
  ]);
  assert.equal(setRuntimeCollaborationEditTarget(state, "wc_msg_edit"), true);
  assert.equal(runtimeCollaborationEditTarget(state).message_id, "wc_msg_edit");
  assert.equal(state.collaboration.replyTargetId, "");
  setRuntimeCollaborationReplyTarget(state, "wc_msg_edit");
  assert.equal(runtimeCollaborationEditTarget(state), null);
  assert.equal(state.collaboration.replyTargetId, "wc_msg_edit");
  assert.equal(setRuntimeCollaborationEditTarget(state, "wc_msg_edit"), true);
  assert.equal(state.collaboration.replyTargetId, "");

  selectRuntimeWorkflowSession(state, "wc_sess_b");
  assert.equal(runtimeCollaborationEditTarget(state), null);
  assert.equal(state.collaboration.replyTargetId, "");
  request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_b", kind: "note", status: "open", created_at: 2, message: "b" },
  ]);
  assert.equal(setRuntimeCollaborationEditTarget(state, "wc_msg_b"), true);
  selectRuntimeProject(state, "other", "agent:other:project");
  assert.equal(runtimeCollaborationEditTarget(state), null);
  assert.equal(state.collaboration.replyTargetId, "");
});

test("incoming authoritative closure cancels edit while preserving refreshed state", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_todo", kind: "todo", status: "open", created_at: 1, message: "work" },
  ]);
  assert.equal(setRuntimeCollaborationEditTarget(state, "wc_msg_todo"), true);
  adoptRuntimeCollaborationObservation(state, request, {
    messages: [{ message_id: "wc_msg_todo", kind: "todo", status: "resolved", resolved_at: 2, created_at: 1, message: "work" }],
  });
  assert.equal(runtimeCollaborationEditTarget(state), null);
  assert.equal(state.collaboration.messages.length, 1);
  assert.equal(state.collaboration.messages[0].status, "resolved");
  assert.equal(takeRuntimeCollaborationMutationNotice(state), "Message changed while editing; current retained state was refreshed.");
});

test("withdraw and replacement responses merge by message id without duplicate history", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_old", kind: "note", status: "open", created_at: 1, message: "wrong" },
  ]);
  adoptRuntimeCollaborationObservation(state, request, {
    messages: [
      { message_id: "wc_msg_old", kind: "note", status: "resolved", closure_kind: "superseded", superseded_by_message_id: "wc_msg_new", created_at: 1, message: "wrong" },
      { message_id: "wc_msg_new", kind: "note", status: "open", supersedes_message_id: "wc_msg_old", created_at: 2, message: "right" },
    ],
  });
  assert.deepEqual(state.collaboration.messages.map((message) => message.message_id), ["wc_msg_old", "wc_msg_new"]);
  adoptRuntimeCollaborationObservation(state, request, {
    messages: [{ message_id: "wc_msg_new", kind: "note", status: "resolved", closure_kind: "withdrawn", created_at: 2, message: "right" }],
  });
  assert.equal(state.collaboration.messages.length, 2);
  assert.equal(state.collaboration.messages[1].closure_kind, "withdrawn");
});

test("unknown mutation outcome stays fenced until exact replay confirms durability", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_old", kind: "guidance", status: "open", priority: "high", requires_ack: true, created_at: 1, message: "wrong" },
  ]);
  assert.equal(setRuntimeCollaborationEditTarget(state, "wc_msg_old"), true);
  assert.equal(markRuntimeCollaborationMutationUncertain(state, request, { kind: "replace", messageId: "wc_msg_old", message: "right" }), true);
  assert.equal(state.collaboration.messages.length, 1);
  assert.equal(state.collaboration.uncertainMutation.messageId, "wc_msg_old");

  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_old", kind: "guidance", status: "resolved", closure_kind: "superseded", superseded_by_message_id: "wc_msg_new", priority: "high", requires_ack: true, created_at: 1, message: "wrong" },
    { message_id: "wc_msg_new", kind: "guidance", status: "open", supersedes_message_id: "wc_msg_old", priority: "high", requires_ack: true, created_at: 2, message: "right" },
  ]);
  assert.equal(state.collaboration.uncertainMutation.messageId, "wc_msg_old");
  assert.equal(runtimeCollaborationEditTarget(state), null);
  assert.equal(
    takeRuntimeCollaborationMutationNotice(state),
    "Replacement observed after refresh; exact replay required to confirm durability."
  );
  assert.deepEqual(runtimeCollaborationMutationRecovery(state, request), {
    kind: "replace",
    messageId: "wc_msg_old",
    message: "right",
  });
  assert.equal(
    completeRuntimeCollaborationMutationRecovery(
      state, request, "Replacement durably confirmed after exact replay."
    ),
    true
  );
  assert.equal(state.collaboration.uncertainMutation, null);
  assert.equal(
    takeRuntimeCollaborationMutationNotice(state),
    "Replacement durably confirmed after exact replay."
  );
  assert.equal(state.collaboration.messages.length, 2);
});

test("unknown replace outcome remains recoverable when retained source was evicted", () => {
  const state = initialRuntimeConsoleState();
  beginRuntimeCredential(state);
  selectRuntimeProject(state, "runner", "agent:runner:project");
  selectRuntimeWorkflowSession(state, "wc_sess_a");
  const request = runtimeCollaborationRequest(state);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_old", kind: "note", status: "open", created_at: 1, message: "wrong" },
  ]);
  assert.equal(markRuntimeCollaborationMutationUncertain(state, request, { kind: "replace", messageId: "wc_msg_old", message: "right" }), true);
  adoptRuntimeCollaborationList(state, request, [
    { message_id: "wc_msg_new", kind: "note", status: "open", supersedes_message_id: "wc_msg_old", created_at: 2, message: "right" },
  ]);
  assert.equal(state.collaboration.uncertainMutation.messageId, "wc_msg_old");
  assert.equal(
    takeRuntimeCollaborationMutationNotice(state),
    "Replacement observed after refresh; exact replay required to confirm durability."
  );
  assert.deepEqual(runtimeCollaborationMutationRecovery(state, request), {
    kind: "replace",
    messageId: "wc_msg_old",
    message: "right",
  });
});

test("only eligible open Human Join kinds expose mutation actions and ACK is not a lock", () => {
  for (const kind of ["note", "guidance", "question", "todo"]) {
    assert.equal(runtimeCollaborationMessageCanMutate({ kind, status: "open" }), true, kind);
  }
  assert.equal(runtimeCollaborationMessageCanMutate({ kind: "guidance", status: "open", requires_ack: true, first_ack_observed_at: 100 }), true);
  for (const kind of ["answer", "progress", "decision", "risk", "proposal"]) {
    assert.equal(runtimeCollaborationMessageCanMutate({ kind, status: "open" }), false, kind);
  }
  assert.equal(runtimeCollaborationMessageCanMutate({ kind: "note", status: "resolved", closure_kind: "withdrawn" }), false);
  assert.equal(runtimeCollaborationMessageCanMutate({ kind: "todo", status: "resolved", closure_kind: "superseded" }), false);
});

test("conversation presentation never infers authorship from reply topology or message kind", () => {
  const sides = runtimeCollaborationMessageSides([
    { message_id: "user-root", kind: "note", message: "hello" },
    { message_id: "reply-without-provenance", kind: "note", reply_to: "user-root", message: "received" },
    { message_id: "local-reply", kind: "question", reply_to: "reply-without-provenance", message: "why" },
    { message_id: "trusted-agent", kind: "progress", author_session_id: "wc_sess_worker", message: "working" },
    { message_id: "answer-without-provenance", kind: "answer", message: "done" },
    { message_id: "retained-reply", kind: "note", reply_to: "missing", message: "retained" },
  ], new Set(["user-root", "local-reply"]));
  assert.equal(sides.get("user-root"), "outgoing");
  assert.equal(sides.get("reply-without-provenance"), "neutral");
  assert.equal(sides.get("local-reply"), "outgoing");
  assert.equal(sides.get("trusted-agent"), "incoming");
  assert.equal(sides.get("answer-without-provenance"), "neutral");
  assert.equal(sides.get("retained-reply"), "neutral");
});

test("runtime collaboration rendering uses textContent and explicitly reloads on history loss", async () => {
  const source = await readFile(new URL("../src/runtime.ts", import.meta.url), "utf8");
  const html = await readFile(new URL("../src/runtime.html", import.meta.url), "utf8");
  const css = await readFile(new URL("../src/runtime.css", import.meta.url), "utf8");
  assert.equal(html.includes("runtime-project-" + "select"), false);
  assert.match(html, /runtime-project-list/);
  assert.match(html, /runtime-project-search/);
  assert.match(html, /runtime-token-remember/);
  assert.match(html, /data-theme-option="system"/);
  assert.match(html, /data-theme-option="light"/);
  assert.match(html, /data-theme-option="dark"/);
  assert.match(html, /runtime-mobile-nav-toggle/);
  assert.match(html, /runtime-mobile-nav-close/);
  assert.match(html, /runtime-mobile-nav-backdrop/);
  assert.match(html, /runtime-inspector-backdrop/);
  assert.match(html, /data-runtime-view="sessions"/);
  assert.match(html, /data-runtime-view="operations"/);
  assert.match(html, /runtime-operations-stage/);
  assert.match(html, /runtime-operations-overview/);
  assert.match(html, /runtime-operations-runners/);
  assert.match(html, /runtime-operations-agents/);
  assert.match(html, /runtime-session-workspace/);
  assert.match(html, /runtime-workflow-sessions-panel/);
  assert.match(html, /runtime-session-id/);
  assert.match(html, /runtime-session-created/);
  assert.match(html, /runtime-session-updated/);
  assert.match(html, /runtime-session-context-lifecycle/);
  assert.match(html, /runtime-session-context-mode/);
  assert.match(css, /\.session-identity/);
  assert.match(html, /class="context-navigation"/);
  assert.match(html, /data-context-target="runtime-context-activity"/);
  assert.match(html, /workspace path/);
  assert.match(html, /class="recent-panel-title">Recent Sessions<\/span>/);
  assert.match(html, /id="runtime-inspector-close"[^>]*aria-label="Close session context"/);
  assert.match(html, /runtime-recent-session-list/);
  assert.match(html, /Runner Fleet/);
  assert.match(html, /runtime-runner-list/);
  assert.match(html, /All Runners/);
  assert.match(html, /runtime-collaboration-form/);
  assert.match(html, /runtime-collaboration-board[^>]*role="log"/);
  assert.match(html, /runtime-message-announcer[^>]*aria-live="polite"/);
  assert.match(html, /runtime-new-messages/);
  assert.match(html, /id="runtime-chat-scroll" class="chat-scroll"/);
  assert.match(html, /id="runtime-message-body" rows="1" maxlength="4000" enterkeyhint="send"/);
  assert.match(html, /runtime-message-options/);
  assert.match(html, /composer-options-popover/);
  assert.match(html, /runtime-collaboration-empty-title/);
  assert.match(html, /runtime-collaboration-empty-copy/);
  assert.match(html, /runtime-message-requires-ack/);
  assert.match(css, /-webkit-line-clamp:\s*4/);
  assert.match(css, /\.recent-session-row/);
  assert.match(css, /\.fleet-row/);
  assert.match(css, /\.device-group/);
  assert.match(css, /@media \(max-width: 900px\)/);
  assert.match(css, /@media \(min-width: 1280px\)/);
  assert.match(css, /--context-rail-width:\s*clamp\(320px,\s*26vw,\s*420px\)/);
  assert.match(css, /\.runtime-shell\.context-docked\s*\{[^}]*--content-width:\s*760px[^}]*grid-template-columns:\s*var\(--sidebar-width\)\s+minmax\(0,\s*1fr\)\s+var\(--context-rail-width\)/);
  assert.match(css, /translateX\(-102%\)/);
  assert.match(css, /env\(safe-area-inset-bottom\)/);
  assert.match(css, /env\(safe-area-inset-top\)/);
  assert.match(css, /@media \(pointer: coarse\)/);
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
  assert.match(css, /data-resolved-theme="light"/);
  assert.match(css, /backdrop-filter/);
  assert.match(css, /--ambient-three/);
  assert.match(css, /\.workspace-topbar\s*\{[^}]*z-index:\s*40/);
  assert.match(css, /\.runtime-shell\s*\{[^}]*gap:\s*0[^}]*padding:\s*0/);
  assert.match(css, /\.workspace-main\s*\{[^}]*background:\s*var\(--page-surface\)/);
  assert.match(css, /--layout-major:\s*61\.8%/);
  assert.match(css, /--layout-minor:\s*38\.2%/);
  assert.match(css, /--sidebar-width:\s*clamp\(280px,\s*21vw,\s*320px\)/);
  assert.match(css, /--content-width:\s*760px/);
  assert.match(css, /\.message-card\.message-incoming\s*\{[^}]*width:\s*fit-content[^}]*max-width:\s*min\(82%,\s*880px\)/);
  assert.match(css, /\.message-card\.message-neutral\s*\{[^}]*width:\s*fit-content[^}]*max-width:\s*min\(82%,\s*880px\)/);
  assert.match(css, /\.message-card\.message-outgoing\s*\{[^}]*max-width:\s*min\(68%,\s*680px\)[^}]*align-self:\s*flex-end/);
  assert.match(css, /--message-bubble-radius:\s*22px/);
  assert.doesNotMatch(css, /message-avatar/);
  assert.doesNotMatch(css, /--message-bubble-anchor-radius/);
  assert.match(css, /\.message-card\.message-incoming \.message-bubble\s*\{[^}]*border:\s*0[^}]*border-radius:\s*var\(--message-bubble-radius\)/);
  assert.match(css, /\.message-card\.message-outgoing \.message-bubble\s*\{[^}]*border:\s*0[^}]*border-radius:\s*var\(--message-bubble-radius\)/);
  assert.match(css, /\.project-row\.selected\s*\{[^}]*background:\s*var\(--sidebar-selected\)/);
  assert.match(css, /\.device-project-list\s*\{[^}]*border-left:\s*0/);
  assert.match(css, /\.project-row-state/);
  assert.match(css, /--message-incoming-bg:\s*rgba\(255,\s*255,\s*255,\s*\.055\)/);
  assert.match(css, /--message-outgoing-bg:\s*#285487/);
  assert.match(css, /data-resolved-theme="light"[\s\S]*--message-incoming-bg:\s*rgba\(25,\s*32,\s*45,\s*\.055\)/);
  assert.match(css, /data-resolved-theme="light"[\s\S]*--message-outgoing-bg:\s*#2866a8/);
  assert.match(css, /\.message-card\.message-incoming \.message-bubble\s*\{[^}]*background:\s*var\(--message-incoming-bg\)/);
  assert.match(css, /\.message-card\.message-outgoing \.message-bubble\s*\{[^}]*background:\s*var\(--message-outgoing-bg\)/);
  assert.match(css, /\.message-footer\s*\{[^}]*display:\s*flex/);
  assert.match(css, /\.collaboration-composer:focus-within/);
  assert.match(css, /\.composer-options-popover/);
  assert.match(css, /\.message-code-copy/);
  assert.match(css, /\.message-date-separator/);
  assert.match(css, /\.new-messages-button/);
  assert.match(css, /\.operations-stage/);
  assert.match(css, /\.topbar-more-popover/);
  assert.match(css, /@keyframes composer-enter/);
  assert.match(css, /\.message-card \+ \.message-card/);
  assert.match(css, /\.session-card::before/);
  assert.equal(source.includes("innerHTML"), false);
  assert.match(source, /APPEARANCE_STORAGE_KEY/);
  assert.match(source, /applyAppearance/);
  assert.doesNotMatch(source, /api\("runner"/);
  assert.match(source, /selectRuntimeSessionLocation/);
  assert.doesNotMatch(source, /project-row-path/);
  assert.match(source, /runtimeProjectIdentityText\(project\)/);
  assert.match(source, /runtimeCollaborationMessageSides\(messages, locallyAuthoredCollaborationMessageIds\)/);
  assert.match(source, /provenance-unknown/);
  assert.match(source, /sessionListMetaSnapshot = \{\s*total: typeof response\.data\.total === "number" \? Math\.max\(sessionRows\.length, response\.data\.total\) : sessionRows\.length,\s*truncated: !!response\.data\.truncated,\s*\}/);
  assert.match(source, /runtime-session-search"\)\?\.addEventListener\("input", \(\) => renderSessionList\(sessionRows, sessionListMetaSnapshot\)\)/);
  assert.match(source, /rememberLocalCollaborationMessage/);
  assert.match(source, /message-group-continuation/);
  assert.match(source, /syncCollaborationComposerLayout/);
  assert.match(source, /scrollCollaborationToLatest/);
  assert.match(source, /scroll\.scrollTo\(\{ top: scroll\.scrollHeight, behavior \}\)/);
  assert.match(source, /firstRetainedRender \|\| \(hasNewMessages && shouldFollowNewMessages\)/);
  assert.match(source, /collaborationFollowLatest \|\| chatIsNearLatest\(\)/);
  assert.match(source, /collaborationPendingMessages \+= newMessageIds\.length/);
  assert.match(source, /appendRichMessage/);
  assert.match(source, /DRAFT_STORAGE_PREFIX/);
  assert.match(source, /WORKSPACE_VIEW_STORAGE_KEY/);
  assert.doesNotMatch(source, /window\.matchMedia\("\(pointer: fine\)"\)/);
  assert.match(source, /event\.shiftKey \|\| event\.isComposing \|\| event\.keyCode === 229/);
  assert.match(source, /form\.requestSubmit\(\)/);
  assert.match(source, /message-entering/);
  assert.match(source, /Acknowledgement required/);
  assert.doesNotMatch(source, /className = "message-links"/);
  assert.match(source, /renderSessionWorkspaceIdentity\(\)/);
  assert.match(source, /function revealWorkflowSessionDetail[\s\S]*scrollIntoView\(\{ block: "start", inline: "nearest" \}\)/);
  assert.match(source, /setText\("runtime-session-id", String\(detail\.session_id/);
  assert.match(source, /setText\("runtime-session-created", dateTimeLabel\(detail\.created_at\)\)/);
  assert.match(source, /setText\("runtime-session-updated", dateTimeLabel\(detail\.updated_at\)\)/);
  const recentStart = source.indexOf("function renderRecentSessions");
  const recentEnd = source.indexOf("function selectRecentSession", recentStart);
  const recentRender = source.slice(recentStart, recentEnd);
  assert.match(recentRender, /localizedLivenessPresentation\(session\)/);
  assert.match(recentRender, /attentionLabel\(session\.overview\?\.attention\)/);
  assert.match(recentRender, /updatedLabel\(session\.updated_at\)/);
  const recentSelectStart = source.indexOf("function selectRecentSession");
  const recentSelectEnd = source.indexOf("async function fetchSessions", recentSelectStart);
  assert.match(source.slice(recentSelectStart, recentSelectEnd), /revealWorkflowSessionDetail\(\)/);
  assert.doesNotMatch(recentRender, /workflowSessionListOverviewFacts|summary-facts|validation/);
  assert.doesNotMatch(recentRender, /\.sort\(/);
  assert.match(source, /applyRunnerFilter\(select\.value\)/);
  assert.match(source, /void fetchOverview\(refreshRuntimeOverview\(state\)\)/);
  assert.match(source, /const REFRESH_MS = 30000;/);
  assert.doesNotMatch(source, /every 8 seconds|每 8 秒|REFRESH_MS = 8000/);
  const autoRefreshStart = source.indexOf("function refreshAutoSurfaces");
  const autoRefreshEnd = source.indexOf("function connectRuntimeCredential", autoRefreshStart);
  const autoRefresh = source.slice(autoRefreshStart, autoRefreshEnd);
  assert.match(autoRefresh, /document\.hidden[\s\S]*refreshCommunication\(false\)/);
  assert.match(autoRefresh, /refreshCommunication\(workspaceView === "operations"\)/);
  assert.match(autoRefresh, /window\.setInterval\(refreshAutoSurfaces, REFRESH_MS\)/);
  assert.match(source, /appendRichMessage\(bubble, message\?\.message\)/);
  assert.match(source, /action === "reload"[\s\S]*loadRetainedCollaboration/);
  assert.match(source, /action === "drain"/);
  assert.match(source, /abortCollaboration\(\)/);
  assert.match(source, /workflow-session-post-message/);
  assert.match(source, /workflow-session-withdraw-message/);
  assert.match(source, /workflow-session-replace-message/);
  const collaborationRenderStart = source.indexOf("function renderCollaboration");
  const collaborationRenderEnd = source.indexOf("async function confirmCollaborationMutationDurability", collaborationRenderStart);
  const collaborationRender = source.slice(collaborationRenderStart, collaborationRenderEnd);
  assert.doesNotMatch(collaborationRender, /messageSide === "outgoing" && runtimeCollaborationMessageCanMutate/);
  assert.match(source, /sessionCollaborationAuthorityFailure/);
  assert.match(source, /response\?\.status !== 403/);
  assert.match(source, /This credential can still read the Session; add session:collaborate to send, edit, or withdraw messages\./);
  assert.match(source, /show\("runtime-collaboration-form", true\)/);
  assert.match(source, /show\("runtime-collaboration-board", messages\.length > 0\)/);
  assert.match(source, /Conversation access requires runtime:read/);
  assert.match(source, /setHumanJoinSendEnabled\(false\)/);
  assert.match(source, /function setMobileNavigationOpen/);
  assert.match(source, /function syncResponsiveNavigation/);
  assert.match(source, /WIDE_CONTEXT_MEDIA/);
  assert.match(source, /classList\.toggle\("context-docked", resolved\.isDocked\)/);
  assert.match(source, /inspector\.open = resolved\.visible/);
  assert.match(source, /event\.key === "Escape"/);
  assert.match(source, /visibleFocusableElements\(sidebar\)/);
  assert.match(source, /Reply target selected\. Your next message will reply to /);
  assert.match(source, /body\?\.focus\(\)/);
  assert.match(source, /state\.collaboration\.phase === "live" && !state\.collaboration\.uncertainMutation/);
  assert.match(source, /Withdraw this retained message; history is preserved\./);
  assert.match(source, /Replace this retained message while preserving its history\./);
  assert.match(source, /kind\?\.value === "guidance"/);
  assert.match(source, /priority\?\.value !== "high"/);
  assert.match(source, /First ACK observed/);
  assert.doesNotMatch(source, /Delivered|Read by model|Currently acknowledged/);
  assert.match(source, /Refresh failed · showing previous data/);
  assert.match(source, /runtimeCollaborationNeedsRefreshRecovery/);
  assert.match(source, /signature === renderedCollaborationSignature/);
  const communicationRefreshStart = source.indexOf("async function performCommunicationRefresh");
  const communicationRefreshEnd = source.indexOf("async function createCommunicationAgent", communicationRefreshStart);
  const communicationRefresh = source.slice(communicationRefreshStart, communicationRefreshEnd);
  assert.match(communicationRefresh, /!includeData \|\| communicationReadAvailable === false/);
  assert.match(communicationRefresh, /fetchCommunicationAgents\(generation, false\)/);
  assert.match(communicationRefresh, /fetchCommunicationConversations\(generation, false\)/);
  assert.match(communicationRefresh, /fetchCommunicationConversation\(generation, false\)/);
  assert.match(communicationRefresh, /fetchCommunicationInbox\(generation, false\)/);
  assert.match(communicationRefresh, /communicationRefreshCoordinator\.refresh\(includeData\)/);
  assert.doesNotMatch(source, /communicationRefreshInFlight/);
  assert.match(source, /operations && token\) void refreshCommunication\(true\)/);
  assert.match(source, /visibilitychange/);
  const renderProjectsStart = source.indexOf("function renderProjectSelectors");
  const renderProjectsEnd = source.indexOf("function switchProject", renderProjectsStart);
  const renderProjects = source.slice(renderProjectsStart, renderProjectsEnd);
  assert.match(renderProjects, /document\.createElement\("summary"\)/);
  assert.match(renderProjects, /signature === renderedProjectSelectorsSignature/);
  assert.match(renderProjects, /workspace\.addEventListener\("toggle"/);
  assert.doesNotMatch(renderProjects, /addEventListener\("keydown"/);
  assert.match(renderProjects, /all\.textContent = tr\("All Runners"\)/);
  assert.match(renderProjects, /switchProject\(String\(project\.client_id \|\| ""\), String\(project\.id \|\| ""\)\)/);
  assert.match(renderProjects, /project-row-signals/);
  assert.match(renderProjects, /project-row-meta/);
  assert.match(renderProjects, /scan partial/);
  assert.match(renderProjects, /row\.title = \[projectName, projectId/);
  assert.match(renderProjects, /projectsByDevice/);
  assert.match(renderProjects, /projectDeviceFilter/);
  assert.match(renderProjects, /workspace\.appendChild\(sessionsPanel\)/);
  assert.match(renderProjects, /deviceMeta\.textContent = tr\(status\) \+ " · " \+ countLabel\(deviceProjects\.length, "Project"\)/);
  assert.match(source, /appendRichMessage\(bubble, message\?\.message\);\s*content\.appendChild\(bubble\)/);
  assert.match(source, /footer\.appendChild\(actions\);\s*content\.appendChild\(footer\);\s*card\.appendChild\(content\)/);
  assert.doesNotMatch(source, /message-avatar/);
  assert.match(source, /createMessageAction\(tr\("Reply"\), "reply"/);
  assert.match(source, /projectIcon\.appendChild\(runtimeIcon\("folder"\)\)/);
  assert.match(source, /icon\.appendChild\(runtimeIcon\("message"\)\)/);
  const renderRunnersStart = source.indexOf("function renderRunnerFleet");
  const renderRunnersEnd = source.indexOf("function renderRecentSessions", renderRunnersStart);
  const renderRunners = source.slice(renderRunnersStart, renderRunnersEnd);
  assert.match(renderRunners, /projects_scan_partial/);
  assert.match(renderRunners, /Projects scanned/);
  assert.match(renderRunners, /fleet scan partial/);
  assert.match(renderRunners, /Session scan partial/);
  assert.doesNotMatch(renderRunners, /visible_project_count/);
  const fetchProjectsStart = source.indexOf("async function fetchProjects");
  const fetchProjectsEnd = source.indexOf("function effectiveProjects", fetchProjectsStart);
  assert.doesNotMatch(source.slice(fetchProjectsStart, fetchProjectsEnd), /fetchOverview\(/);
  const fetchProjects = source.slice(fetchProjectsStart, fetchProjectsEnd);
  assert.match(fetchProjects, /payload\.client_id = clientId/);
  const fetchSessionsStart = source.indexOf("async function fetchSessions");
  const fetchSessionsEnd = source.indexOf("function updatedLabel", fetchSessionsStart);
  const fetchSessions = source.slice(fetchSessionsStart, fetchSessionsEnd);
  assert.match(fetchSessions, /runtimeWorkflowSessionSummaryChanged\(previousSelected, nextSelected\)/);
  assert.match(fetchSessions, /!state\.workflow\.snapshot \|\| runtimeWorkflowSessionSummaryChanged/);
  assert.match(fetchProjects, /payload\.query = query/);
  assert.match(fetchProjects, /if \(query\) \{[\s\S]*renderProjectSelectors\(projectRows, projectRowsTruncated\);[\s\S]*return true;/);
  assert.match(fetchProjects, /currentProject && projectRowsTruncated/);
  assert.match(fetchProjects, /projectRowsTotal = Math\.max\(projectRows\.length, reportedTotal\)/);
  assert.match(renderProjects, /matching Projects shown/);
  assert.match(renderProjects, /visible Projects shown/);
  const applyRunnerStart = source.indexOf("function applyRunnerFilter");
  const applyRunnerEnd = source.indexOf("function runnerAttentionCount", applyRunnerStart);
  const applyRunner = source.slice(applyRunnerStart, applyRunnerEnd);
  assert.match(applyRunner, /projectDeviceFilter = device/);
  assert.match(applyRunner, /fetchProjects\(refreshRuntimeProjects\(state, projectSearch, projectDeviceFilter\)\)/);
  const searchHandlerStart = source.indexOf('el("runtime-project-search")?.addEventListener("input"');
  const searchHandlerEnd = source.indexOf('el("runtime-message-kind")', searchHandlerStart);
  const searchHandler = source.slice(searchHandlerStart, searchHandlerEnd);
  assert.match(searchHandler, /window\.setTimeout/);
  assert.match(searchHandler, /PROJECT_SEARCH_DEBOUNCE_MS/);
  assert.match(searchHandler, /fetchProjects\(refreshRuntimeProjects\(state, projectSearch, projectDeviceFilter\)\)/);
  const selectStart = source.indexOf("function selectSession");
  const selectEnd = source.indexOf("async function fetchSessionDetail", selectStart);
  assert.match(source.slice(selectStart, selectEnd), /setHumanJoinSendEnabled\(false\)[\s\S]*startCollaboration/);
  assert.match(source.slice(selectStart, selectEnd), /revealWorkflowSessionDetail\(\)/);
  const postStart = source.indexOf("async function postHumanCollaborationMessage");
  const postEnd = source.indexOf("function setRefreshBusy", postStart);
  const post = source.slice(postStart, postEnd);
  assert.match(post, /if \(!isCurrentRuntimeCollaborationRequest\(state, request\)\) return;\s*if \(response\?\.status === 0\)[\s\S]*return;\s*\}[\s\S]*if \(send\) send\.disabled = false;/);
  assert.match(post, /Send outcome unknown\. Refresh and review retained messages before retrying\./);
  assert.match(post, /abortCollaboration\(\)[\s\S]*setRuntimeCollaborationPhase\(state, request, "paused"\)/);
  const refreshStart = source.indexOf("async function refreshAll");
  const refreshEnd = source.indexOf("function refreshAutoSurfaces", refreshStart);
  assert.equal((source.slice(refreshStart, refreshEnd).match(/fetchOverview\(/g) || []).length, 1);
  assert.match(source.slice(refreshStart, refreshEnd), /refreshCommunication\(\)/);
  const runnerFilterStart = source.indexOf('el("runtime-device-select")?.addEventListener("change"');
  const runnerFilterEnd = source.indexOf('el("runtime-project-search")', runnerFilterStart);
  const runnerFilter = source.slice(runnerFilterStart, runnerFilterEnd);
  assert.match(runnerFilter, /applyRunnerFilter\(select\.value\)/);
  assert.doesNotMatch(runnerFilter, /switchProject|fetchRunner/);
  const bootstrapStart = source.indexOf("async function loadRetainedCollaboration");
  const bootstrapEnd = source.indexOf("async function startCollaboration", bootstrapStart);
  const bootstrap = source.slice(bootstrapStart, bootstrapEnd);
  const baselineAt = bootstrap.indexOf('api("workflow-session-observe"');
  const retainedListAt = bootstrap.indexOf('api("workflow-session-messages"');
  assert.ok(baselineAt >= 0 && retainedListAt > baselineAt, "live baseline must precede the retained snapshot to avoid a lost-update gap");
  assert.match(bootstrap, /runtimeCollaborationMutationRecovery\(state, request\)/);
  assert.match(bootstrap, /confirmCollaborationMutationDurability\(request, mutationRecovery, controller\)/);
  assert.match(bootstrap, /confirmCollaborationMutationDurability[\s\S]*setRuntimeCollaborationPhase\(state, request, "live"\);[\s\S]*setHumanJoinSendEnabled\(true\)/);
});

test("runner disclosure honors user collapse over selected project and auto-reveals on navigation", () => {
  assert.equal(resolveRunnerDisclosure(null, true), true);
  assert.equal(resolveRunnerDisclosure(null, false), false);
  assert.equal(resolveRunnerDisclosure(false, true), false);
  assert.equal(resolveRunnerDisclosure(true, false), true);

  let storedRunner1 = true;
  assert.equal(resolveRunnerDisclosure(storedRunner1, false), true);

  storedRunner1 = false;
  assert.equal(resolveRunnerDisclosure(storedRunner1, true), false, "rerender must not override manual collapse");

  storedRunner1 = true;
  assert.equal(resolveRunnerDisclosure(storedRunner1, false), true, "explicit navigation auto-reveals target runner");
});

test("runtime context resolution separates presentation mode from user visibility intent", () => {
  assert.equal(resolveRuntimeContextPresentationMode(true, false), "docked");
  assert.equal(resolveRuntimeContextPresentationMode(false, false), "popover");
  assert.equal(resolveRuntimeContextPresentationMode(false, true), "sheet");
  assert.equal(resolveRuntimeContextPresentationMode(true, true), "sheet");

  const wideDefault = resolveRuntimeContextState({
    userIntent: null,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideDefault.visible, true);
  assert.equal(wideDefault.presentationMode, "docked");
  assert.equal(wideDefault.isDocked, true);

  const normalDefault = resolveRuntimeContextState({
    userIntent: null,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(normalDefault.visible, false);
  assert.equal(normalDefault.presentationMode, "popover");
  assert.equal(normalDefault.isDocked, false);

  const mobileDefault = resolveRuntimeContextState({
    userIntent: null,
    isWideViewport: false,
    isMobileViewport: true,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(mobileDefault.visible, false);
  assert.equal(mobileDefault.presentationMode, "sheet");
  assert.equal(mobileDefault.isDocked, false);

  const wideUserClosed = resolveRuntimeContextState({
    userIntent: false,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideUserClosed.visible, false);
  assert.equal(wideUserClosed.isDocked, false);

  const wideAfterRefresh = resolveRuntimeContextState({
    userIntent: false,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideAfterRefresh.visible, false);
  assert.equal(wideAfterRefresh.isDocked, false);

  const normalUserOpened = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(normalUserOpened.visible, true);
  assert.equal(normalUserOpened.presentationMode, "popover");
  assert.equal(normalUserOpened.isDocked, false);

  const wideAfterResize = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideAfterResize.visible, true);
  assert.equal(wideAfterResize.presentationMode, "docked");
  assert.equal(wideAfterResize.isDocked, true);

  const normalClosedResize = resolveRuntimeContextState({
    userIntent: false,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(normalClosedResize.visible, false);
  const wideClosedResize = resolveRuntimeContextState({
    userIntent: false,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideClosedResize.visible, false);
  assert.equal(wideClosedResize.isDocked, false);

  const operationsView = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "operations",
  });
  assert.equal(operationsView.visible, false);
  assert.equal(operationsView.isDocked, false);

  const windowsView = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "windows",
  });
  assert.equal(windowsView.visible, false);
  assert.equal(windowsView.isDocked, false);

  const backToSessions = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(backToSessions.visible, true);
  assert.equal(backToSessions.isDocked, true);

  const noSession = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: false,
    workspaceView: "sessions",
  });
  assert.equal(noSession.visible, false);
  assert.equal(noSession.isDocked, false);

  const sessionRestored = resolveRuntimeContextState({
    userIntent: true,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(sessionRestored.visible, true);
  assert.equal(sessionRestored.isDocked, true);
});

test("context user intent reducer and lifecycle transitions ensure programmatic projections never pollute intent", () => {
  // Scenario 1: Initial wide viewport defaults to open; programmatic DOM projection does NOT contaminate userIntent.
  let userIntent = null;
  const wideDefault = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(wideDefault.visible, true);
  assert.equal(wideDefault.isDocked, true);

  // Programmatic DOM projection sets inspector.open = true.
  // Critical invariant: userIntent must remain null!
  assert.equal(userIntent, null);

  // Resize to normal viewport: null intent correctly resolves to closed.
  const normalAfterResize = resolveRuntimeContextState({
    userIntent,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(normalAfterResize.visible, false);
  assert.equal(normalAfterResize.isDocked, false);

  // Scenario 2: User explicitly opens context. Switching to Operations hides it, returning to Sessions restores it.
  userIntent = reduceRuntimeContextUserIntent(userIntent, { type: "explicit_open" });
  assert.equal(userIntent, true);

  const opsView = resolveRuntimeContextState({
    userIntent,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "operations",
  });
  assert.equal(opsView.visible, false, "Operations view temporarily hides session context");
  // Switching views or programmatic closing must NOT overwrite userIntent
  assert.equal(userIntent, true, "userIntent remains true during operations view");

  const backToSessions = resolveRuntimeContextState({
    userIntent,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(backToSessions.visible, true, "Context re-opens when returning to Sessions view");

  // Scenario 3: Selected session temporarily unavailable does NOT record as manual collapse.
  const sessionUnavailable = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: false,
    workspaceView: "sessions",
  });
  assert.equal(sessionUnavailable.visible, false);
  assert.equal(userIntent, true, "temporary unavailability does not clear userIntent");

  const sessionAvailableAgain = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(sessionAvailableAgain.visible, true);
  assert.equal(sessionAvailableAgain.isDocked, true);

  // Scenario 4: Explicit user close sets userIntent = false and persists across refresh / resize.
  userIntent = reduceRuntimeContextUserIntent(userIntent, { type: "explicit_close" });
  assert.equal(userIntent, false);

  const closedOnWide = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(closedOnWide.visible, false);
  assert.equal(closedOnWide.isDocked, false);

  // Refresh / resize simulation: userIntent remains false
  const closedAfterRefresh = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(closedAfterRefresh.visible, false);

  // Scenario 5: Trigger toggle action correctly inverts visibility.
  // When visible in DOM, toggle action closes context.
  const toggledClosed = reduceRuntimeContextUserIntent(null, { type: "toggle_trigger", currentVisible: true });
  assert.equal(toggledClosed, false);

  // When hidden in DOM, toggle action opens context.
  const toggledOpen = reduceRuntimeContextUserIntent(null, { type: "toggle_trigger", currentVisible: false });
  assert.equal(toggledOpen, true);

  // 1279 <-> 1280 transitions with explicit open intent preserve intent and only switch presentation mode.
  userIntent = true;
  const at1279 = resolveRuntimeContextState({
    userIntent,
    isWideViewport: false,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(at1279.visible, true);
  assert.equal(at1279.presentationMode, "popover");
  assert.equal(at1279.isDocked, false);

  const at1280 = resolveRuntimeContextState({
    userIntent,
    isWideViewport: true,
    isMobileViewport: false,
    hasSelectedSession: true,
    workspaceView: "sessions",
  });
  assert.equal(at1280.visible, true);
  assert.equal(at1280.presentationMode, "docked");
  assert.equal(at1280.isDocked, true);
});

test("context focus transition preserves accessible focus across breakpoint and close actions", () => {
  // P2 Case 1: Popover is open and trigger is focused. Viewport resizes 1279 -> 1280.
  // Trigger will be hidden by CSS (display: none), so focus MUST transfer to #runtime-inspector-close.
  assert.equal(
    resolveRuntimeContextFocusTransition({
      wasDocked: false,
      nextDocked: true,
      isTriggerFocused: true,
    }),
    "inspector_close"
  );

  // P2 Case 2: Popover is open but focus was elsewhere (e.g. inside chat or timeline).
  // Resize to 1280 must NOT steal focus!
  assert.equal(
    resolveRuntimeContextFocusTransition({
      wasDocked: false,
      nextDocked: true,
      isTriggerFocused: false,
    }),
    "none"
  );

  // P2 Case 3: Context is closed and user resizes across 1279 <-> 1280.
  // nextDocked is false because closed context never docks; focus must never be stolen.
  assert.equal(
    resolveRuntimeContextFocusTransition({
      wasDocked: false,
      nextDocked: false,
      isTriggerFocused: true,
    }),
    "none"
  );

  // P2 Case 4: Already docked; resize within >=1280 range does not transfer focus.
  assert.equal(
    resolveRuntimeContextFocusTransition({
      wasDocked: true,
      nextDocked: true,
      isTriggerFocused: false,
    }),
    "none"
  );

  // P2 Case 5: Reverse transition from docked (1280) to popover (1279).
  // Close button remains visible in popover header; no focus jump needed.
  assert.equal(
    resolveRuntimeContextFocusTransition({
      wasDocked: true,
      nextDocked: false,
      isTriggerFocused: false,
    }),
    "none"
  );
});

test("navigation and inspector source contracts maintain disclosure hierarchy and accessibility", async () => {
  const [html, css, source] = await Promise.all([
    readFile(new URL("../src/runtime.html", import.meta.url), "utf8"),
    readFile(new URL("../src/runtime.css", import.meta.url), "utf8"),
    readFile(new URL("../src/runtime.ts", import.meta.url), "utf8"),
  ]);

  // P3: Recent Sessions component semantics - clean class, no legacy sidebar-details overrides
  assert.match(html, /<details id="runtime-recent-panel" class="recent-panel" open>/);
  assert.match(html, /<span class="recent-panel-title">Recent Sessions<\/span>/);
  assert.doesNotMatch(html, /class="[^"]*sidebar-details/);
  assert.doesNotMatch(css, /\.sidebar-details/);
  assert.doesNotMatch(css, /summary::before\s*\{\s*content:\s*"Show more"/);
  assert.doesNotMatch(css, /summary\[open\]::before\s*\{\s*content:\s*"Recent Sessions"/);
  assert.match(css, /\.recent-panel\s*\{[^}]*border-top:/);

  // P1 & P2: Inspector triggers and close controls with deterministic user intent handling
  assert.match(html, /id="runtime-inspector-close"[^>]*aria-label="Close session context"/);
  assert.match(html, /id="runtime-inspector-backdrop"[^>]*aria-label="Close session context"/);
  assert.match(source, /"Close session context": "关闭会话上下文"/);
  assert.match(source, /el\("runtime-inspector-close"\)\?\.addEventListener\("click", \(\) => closeRuntimeInspector\(true, true\)\)/);
  assert.match(source, /document\.querySelector\("\.context-trigger"\)\?\.addEventListener\("click",/);
  assert.match(source, /reduceRuntimeContextUserIntent/);
  assert.match(source, /resolveRuntimeContextFocusTransition/);
  assert.doesNotMatch(source, /syncingContextDom/);
  assert.doesNotMatch(source, /contextUserIntent\s*=\s*inspector\.open/);
  assert.match(source, /function lock[\s\S]*closeRuntimeInspector\(false, true\);[\s\S]*contextUserIntent = null;/);

  assert.match(source, /function isContextDocked/);
  assert.match(source, /function syncContextUi/);
  assert.match(source, /function revealRunner/);
  assert.match(source, /function switchProject[\s\S]*if \(device\) revealRunner\(device\)/);
  assert.match(source, /function selectRecentSession[\s\S]*if \(clientId\) revealRunner\(clientId\)/);
  assert.match(source, /group\.open = resolveRunnerDisclosure\(storedDisclosure, defaultOpen\)/);
});
