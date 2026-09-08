import {
  initialWorkflowSessionState,
  selectWorkflowSession,
  refreshWorkflowSessionDetail,
  clearWorkflowSessionSelection,
  isCurrentWorkflowSessionDetailRequest,
  adoptWorkflowSessionDetail,
} from "./workflow_session_state.js";

export {};

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

export class RuntimeCommunicationRefreshCoordinator {
  private generation = 0;
  private inFlight: { includeData: boolean; generation: number; promise: Promise<boolean> } | null = null;

  constructor(private readonly runRefresh: (includeData: boolean) => Promise<boolean>) {}

  refresh(includeData = true): Promise<boolean> {
    const generation = this.generation;
    const current = this.inFlight;
    if (current && current.generation === generation) {
      if (!includeData || current.includeData) return current.promise;
      return current.promise.then(
        () => this.generation === generation ? this.refresh(true) : false,
        () => this.generation === generation ? this.refresh(true) : false,
      );
    }

    const promise = Promise.resolve().then(() => this.runRefresh(includeData));
    const started = { includeData, generation, promise };
    this.inFlight = started;
    const clear = () => {
      if (this.inFlight === started) this.inFlight = null;
    };
    void promise.then(clear, clear);
    return promise;
  }

  reset(): void {
    this.generation += 1;
    this.inFlight = null;
  }
}

export function runtimeCommunicationTranscriptAfterSeq(lastSeq: unknown, limit = 100): number {
  const normalizedLastSeq = typeof lastSeq === "number" && Number.isSafeInteger(lastSeq)
    ? Math.max(0, lastSeq)
    : 0;
  const normalizedLimit = Number.isSafeInteger(limit) && limit > 0 ? limit : 100;
  return Math.max(0, normalizedLastSeq - normalizedLimit);
}

export function runtimeWorkflowSessionSummaryRevision(session: any): string {
  if (!session) return "";
  return JSON.stringify([
    String(session.session_id || ""),
    String(session.title || ""),
    String(session.lifecycle || ""),
    String(session.mode || ""),
    typeof session.updated_at === "number" ? session.updated_at : null,
    !!session.running_call,
    typeof session.running_jobs === "number" ? session.running_jobs : null,
    session.running_jobs_complete === true,
    session.current_activity ?? null,
    session.last_activity ?? null,
    session.overview ?? null,
  ]);
}

export function runtimeWorkflowSessionSummaryChanged(previous: any, next: any): boolean {
  return runtimeWorkflowSessionSummaryRevision(previous) !== runtimeWorkflowSessionSummaryRevision(next);
}

function emptyCollaborationState(): any {
  return {
    generation: 0,
    sessionId: "",
    messages: [],
    observationToken: "",
    available: true,
    phase: "idle",
    replyTargetId: "",
    editTargetId: "",
    uncertainMutation: null,
    mutationNotice: "",
  };
}

function messageCreatedAt(message: any): number {
  return typeof message?.created_at === "number" ? message.created_at : 0;
}

const RUNTIME_COLLABORATION_MUTABLE_KINDS = new Set(["note", "guidance", "question", "todo"]);

export function runtimeCollaborationMessageCanMutate(message: any): boolean {
  return !!message && message.status === "open" && RUNTIME_COLLABORATION_MUTABLE_KINDS.has(String(message.kind || ""));
}

export type RuntimeCollaborationMessageSide = "incoming" | "outgoing" | "neutral";

export function runtimeCollaborationMessageSides(
  messages: any[],
  locallyAuthoredMessageIds: ReadonlySet<string> = new Set(),
): Map<string, RuntimeCollaborationMessageSide> {
  const sides = new Map<string, RuntimeCollaborationMessageSide>();
  for (const message of Array.isArray(messages) ? messages : []) {
    const id = typeof message?.message_id === "string" ? message.message_id : "";
    if (!id) continue;
    const side: RuntimeCollaborationMessageSide = message?.author_session_id
      ? "incoming"
      : locallyAuthoredMessageIds.has(id)
        ? "outgoing"
        : "neutral";
    sides.set(id, side);
  }
  return sides;
}

function collaborationMessageById(state: any, messageId: string): any | null {
  return (Array.isArray(state?.collaboration?.messages) ? state.collaboration.messages : [])
    .find((message: any) => String(message?.message_id || "") === messageId) || null;
}

function reconcileRuntimeCollaborationMutationState(state: any, authoritativeRefresh = false): void {
  const collaboration = state.collaboration;
  const uncertain = collaboration.uncertainMutation;
  if (uncertain) {
    const original = collaborationMessageById(state, String(uncertain.messageId || ""));
    const confirmedWithdraw = uncertain.kind === "withdraw" && original?.closure_kind === "withdrawn";
    let confirmedReplace = false;
    if (uncertain.kind === "replace") {
      const replacementId = original?.closure_kind === "superseded"
        ? String(original?.superseded_by_message_id || "")
        : "";
      const linkedReplacement = replacementId ? collaborationMessageById(state, replacementId) : null;
      const retainedReplacement = linkedReplacement || (Array.isArray(collaboration.messages)
        ? collaboration.messages.find((message: any) =>
            message?.supersedes_message_id === uncertain.messageId && message?.message === uncertain.message
          )
        : null);
      confirmedReplace = !!retainedReplacement
        && retainedReplacement?.supersedes_message_id === uncertain.messageId
        && retainedReplacement?.message === uncertain.message;
    }
    if (confirmedWithdraw || confirmedReplace) {
      collaboration.mutationNotice = confirmedWithdraw
        ? "Withdraw observed after refresh; exact replay required to confirm durability."
        : "Replacement observed after refresh; exact replay required to confirm durability.";
    } else if (authoritativeRefresh) {
      collaboration.mutationNotice = "Outcome not observed in retained messages; exact replay required before live observation resumes.";
    }
  }

  if (collaboration.editTargetId) {
    const target = collaborationMessageById(state, collaboration.editTargetId);
    if (!runtimeCollaborationMessageCanMutate(target)) {
      collaboration.editTargetId = "";
      if (!collaboration.mutationNotice) {
        collaboration.mutationNotice = "Message changed while editing; current retained state was refreshed.";
      }
    }
  }
}

export function mergeRuntimeCollaborationMessages(current: any[], updates: any[]): any[] {
  const byId = new Map<string, any>();
  for (const message of Array.isArray(current) ? current : []) {
    const id = typeof message?.message_id === "string" ? message.message_id : "";
    if (id) byId.set(id, message);
  }
  for (const message of Array.isArray(updates) ? updates : []) {
    const id = typeof message?.message_id === "string" ? message.message_id : "";
    if (id) byId.set(id, message);
  }
  return Array.from(byId.values()).sort((left, right) =>
    messageCreatedAt(left) - messageCreatedAt(right) ||
    compareText(String(left?.message_id || ""), String(right?.message_id || ""))
  );
}

export function runtimeCollaborationObservationAction(payload: any): "reload" | "drain" | "wait" {
  if (payload?.history_lost) return "reload";
  if (payload?.has_more) return "drain";
  return "wait";
}

export function runtimeDeviceIds(projects: any[]): string[] {
  const devices = new Set<string>();
  for (const project of Array.isArray(projects) ? projects : []) {
    const clientId = typeof project?.client_id === "string" ? project.client_id : "";
    if (clientId) devices.add(clientId);
  }
  return Array.from(devices).sort(compareText);
}

export function runtimeProjectsForDevice(projects: any[], clientId: string): any[] {
  return (Array.isArray(projects) ? projects : [])
    .filter((project) => project && (!clientId || project.client_id === clientId) && typeof project.id === "string" && project.id)
    .slice()
    .sort((left, right) => {
      const leftName = typeof left.name === "string" && left.name ? left.name : left.id;
      const rightName = typeof right.name === "string" && right.name ? right.name : right.id;
      return compareText(leftName, rightName) || compareText(left.id, right.id);
    });
}

function projectAttentionCount(project: any): number {
  const attention = project?.sessions?.attention;
  return ["open_guidance", "open_questions", "open_risks", "open_todos"]
    .reduce((total, key) => total + (typeof attention?.[key] === "number" ? Math.max(0, attention[key]) : 0), 0);
}

export function filterAndSortRuntimeProjects(projects: any[], clientId: string, query: string): any[] {
  const needle = String(query || "").trim().toLocaleLowerCase();
  return runtimeProjectsForDevice(projects, clientId)
    .filter((project) => {
      if (!needle) return true;
      return [project?.name, project?.id, project?.client_id, project?.path]
        .filter((value) => typeof value === "string")
        .some((value) => String(value).toLocaleLowerCase().includes(needle));
    })
    .sort((left, right) => {
      const leftRunning = typeof left?.sessions?.running_sessions === "number" ? left.sessions.running_sessions : 0;
      const rightRunning = typeof right?.sessions?.running_sessions === "number" ? right.sessions.running_sessions : 0;
      if (!!rightRunning !== !!leftRunning) return rightRunning ? 1 : -1;
      const leftAttention = projectAttentionCount(left);
      const rightAttention = projectAttentionCount(right);
      if (!!rightAttention !== !!leftAttention) return rightAttention ? 1 : -1;
      const leftUpdated = typeof left?.sessions?.latest_updated_at === "number" ? left.sessions.latest_updated_at : 0;
      const rightUpdated = typeof right?.sessions?.latest_updated_at === "number" ? right.sessions.latest_updated_at : 0;
      if (leftUpdated !== rightUpdated) return rightUpdated - leftUpdated;
      const leftName = typeof left?.name === "string" && left.name ? left.name : left.id;
      const rightName = typeof right?.name === "string" && right.name ? right.name : right.id;
      return compareText(String(leftName || ""), String(rightName || "")) || compareText(String(left?.id || ""), String(right?.id || ""));
    });
}

export function runtimeProjectIdentityText(project: any): string {
  if (!project || typeof project.id !== "string" || !project.id) return "No project selected";
  const runner = typeof project.client_id === "string" && project.client_id ? project.client_id : "unknown";
  const path = typeof project.path === "string" && project.path ? project.path : "unavailable";
  return "Runner: " + runner + " · Project: " + project.id + " · Workspace: " + path;
}

export function preferredRuntimeProjectSelection(
  projects: any[],
  selectedDevice: string,
  selectedProject: string
): any {
  const rows = Array.isArray(projects) ? projects : [];
  if (selectedProject) {
    const retained = rows.find(
      (project) => project && project.id === selectedProject && typeof project.client_id === "string" && project.client_id
    );
    if (retained) return { device: retained.client_id, project: retained.id };
  }
  const devices = runtimeDeviceIds(rows);
  const device = devices.includes(selectedDevice) ? selectedDevice : "";
  return { device, project: "" };
}

export function initialRuntimeConsoleState(): any {
  return {
    credentialGeneration: 0,
    overviewGeneration: 0,
    projectsGeneration: 0,
    runnerGeneration: 0,
    selectedDevice: "",
    selectedProject: "",
    projectGeneration: 0,
    sessionListGeneration: 0,
    workflow: initialWorkflowSessionState(),
    collaboration: emptyCollaborationState(),
  };
}

export function invalidateRuntimeCredential(state: any): void {
  state.credentialGeneration += 1;
  state.overviewGeneration += 1;
  state.projectsGeneration += 1;
  state.runnerGeneration += 1;
  state.selectedDevice = "";
  state.selectedProject = "";
  state.projectGeneration += 1;
  state.sessionListGeneration += 1;
  clearWorkflowSessionSelection(state.workflow);
  state.collaboration.generation += 1;
  state.collaboration.sessionId = "";
  state.collaboration.messages = [];
  state.collaboration.observationToken = "";
  state.collaboration.available = true;
  state.collaboration.phase = "idle";
  state.collaboration.replyTargetId = "";
  state.collaboration.editTargetId = "";
  state.collaboration.uncertainMutation = null;
  state.collaboration.mutationNotice = "";
}

export function beginRuntimeCredential(state: any): any {
  invalidateRuntimeCredential(state);
  return refreshRuntimeProjects(state);
}

export function refreshRuntimeOverview(state: any): any {
  state.overviewGeneration += 1;
  return { credentialGeneration: state.credentialGeneration, generation: state.overviewGeneration };
}

export function isCurrentRuntimeOverviewRequest(state: any, request: any): boolean {
  return !!request && request.credentialGeneration === state.credentialGeneration && request.generation === state.overviewGeneration;
}

export function refreshRuntimeProjects(state: any, query = "", clientId = state.selectedDevice): any {
  state.projectsGeneration += 1;
  return {
    credentialGeneration: state.credentialGeneration,
    projectGeneration: state.projectGeneration,
    generation: state.projectsGeneration,
    clientId: String(clientId || ""),
    query: String(query || "").trim(),
  };
}

export function isCurrentRuntimeProjectsRequest(state: any, request: any): boolean {
  return !!request &&
    request.credentialGeneration === state.credentialGeneration &&
    request.projectGeneration === state.projectGeneration &&
    request.generation === state.projectsGeneration;
}

export function refreshRuntimeRunner(state: any): any {
  if (!state.selectedDevice) return null;
  state.runnerGeneration += 1;
  return { credentialGeneration: state.credentialGeneration, device: state.selectedDevice, generation: state.runnerGeneration };
}

export function isCurrentRuntimeRunnerRequest(state: any, request: any): boolean {
  return !!request && request.credentialGeneration === state.credentialGeneration &&
    request.device === state.selectedDevice && request.generation === state.runnerGeneration;
}

export function selectRuntimeRunnerFilter(state: any, device: string): void {
  selectRuntimeProject(state, device, "");
}

export function selectRuntimeProject(state: any, device: string, project: string): any {
  if (state.selectedDevice !== device) state.runnerGeneration += 1;
  state.selectedDevice = device;
  state.selectedProject = project;
  state.projectGeneration += 1;
  state.sessionListGeneration += 1;
  clearWorkflowSessionSelection(state.workflow);
  state.collaboration.generation += 1;
  state.collaboration.sessionId = "";
  state.collaboration.messages = [];
  state.collaboration.observationToken = "";
  state.collaboration.available = true;
  state.collaboration.phase = "idle";
  state.collaboration.replyTargetId = "";
  state.collaboration.editTargetId = "";
  state.collaboration.uncertainMutation = null;
  state.collaboration.mutationNotice = "";
  return refreshRuntimeSessionList(state);
}

export function refreshRuntimeSessionList(state: any): any {
  if (!state.selectedProject) return null;
  state.sessionListGeneration += 1;
  return {
    credentialGeneration: state.credentialGeneration,
    project: state.selectedProject,
    projectGeneration: state.projectGeneration,
    generation: state.sessionListGeneration,
  };
}

export function isCurrentRuntimeSessionListRequest(state: any, request: any): boolean {
  return !!request && request.credentialGeneration === state.credentialGeneration &&
    request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
    request.generation === state.sessionListGeneration;
}

function wrapWorkflowRequest(state: any, request: any): any {
  if (!request || !state.selectedProject) return null;
  return {
    credentialGeneration: state.credentialGeneration,
    project: state.selectedProject,
    projectGeneration: state.projectGeneration,
    sessionId: request.sessionId,
    generation: request.generation,
  };
}

export function selectRuntimeWorkflowSession(state: any, sessionId: string): any {
  state.collaboration.generation += 1;
  state.collaboration.sessionId = sessionId;
  state.collaboration.messages = [];
  state.collaboration.observationToken = "";
  state.collaboration.available = true;
  state.collaboration.phase = "idle";
  state.collaboration.replyTargetId = "";
  state.collaboration.editTargetId = "";
  state.collaboration.uncertainMutation = null;
  state.collaboration.mutationNotice = "";
  return wrapWorkflowRequest(state, selectWorkflowSession(state.workflow, sessionId));
}

export function selectRuntimeSessionLocation(
  state: any,
  device: string,
  project: string,
  sessionId: string
): any {
  const sessionListRequest = selectRuntimeProject(state, device, project);
  const detailRequest = selectRuntimeWorkflowSession(state, sessionId);
  return { sessionListRequest, detailRequest };
}

export function refreshRuntimeWorkflowSession(state: any): any {
  return wrapWorkflowRequest(state, refreshWorkflowSessionDetail(state.workflow));
}

export function clearRuntimeWorkflowSession(state: any): void {
  clearWorkflowSessionSelection(state.workflow);
  state.collaboration.generation += 1;
  state.collaboration.sessionId = "";
  state.collaboration.messages = [];
  state.collaboration.observationToken = "";
  state.collaboration.replyTargetId = "";
  state.collaboration.editTargetId = "";
  state.collaboration.uncertainMutation = null;
  state.collaboration.mutationNotice = "";
}

export function runtimeCollaborationRequest(state: any): any {
  if (!state.selectedProject || !state.collaboration.sessionId) return null;
  return {
    credentialGeneration: state.credentialGeneration,
    project: state.selectedProject,
    projectGeneration: state.projectGeneration,
    sessionId: state.collaboration.sessionId,
    generation: state.collaboration.generation,
  };
}

export function isCurrentRuntimeCollaborationRequest(state: any, request: any): boolean {
  return !!request && request.credentialGeneration === state.credentialGeneration &&
    request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
    request.sessionId === state.collaboration.sessionId && request.generation === state.collaboration.generation;
}

export function setRuntimeCollaborationReplyTarget(state: any, messageId: string): void {
  state.collaboration.replyTargetId = String(messageId || "");
  if (state.collaboration.replyTargetId) state.collaboration.editTargetId = "";
}

export function setRuntimeCollaborationEditTarget(state: any, messageId: string): boolean {
  const id = String(messageId || "");
  const message = collaborationMessageById(state, id);
  if (!id || !runtimeCollaborationMessageCanMutate(message)) return false;
  state.collaboration.editTargetId = id;
  state.collaboration.replyTargetId = "";
  state.collaboration.mutationNotice = "";
  return true;
}

export function clearRuntimeCollaborationEditTarget(state: any): void {
  state.collaboration.editTargetId = "";
}

export function runtimeCollaborationEditTarget(state: any): any | null {
  const id = String(state?.collaboration?.editTargetId || "");
  return id ? collaborationMessageById(state, id) : null;
}

export function markRuntimeCollaborationMutationUncertain(state: any, request: any, mutation: any): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.uncertainMutation = {
    kind: mutation?.kind === "replace" ? "replace" : "withdraw",
    messageId: String(mutation?.messageId || ""),
    ...(mutation?.kind === "replace" ? { message: String(mutation?.message || "") } : {}),
  };
  state.collaboration.mutationNotice = "Outcome unknown; refresh retained messages before retrying.";
  return true;
}

export function runtimeCollaborationMutationRecovery(state: any, request: any): any | null {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return null;
  const mutation = state?.collaboration?.uncertainMutation;
  const messageId = String(mutation?.messageId || "");
  if (!mutation || !messageId) return null;
  return {
    kind: mutation.kind === "replace" ? "replace" : "withdraw",
    messageId,
    ...(mutation.kind === "replace" ? { message: String(mutation.message || "") } : {}),
  };
}

export function completeRuntimeCollaborationMutationRecovery(
  state: any,
  request: any,
  notice: string
): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.uncertainMutation = null;
  state.collaboration.mutationNotice = String(notice || "");
  return true;
}

export function takeRuntimeCollaborationMutationNotice(state: any): string {
  const notice = String(state?.collaboration?.mutationNotice || "");
  state.collaboration.mutationNotice = "";
  return notice;
}

export function adoptRuntimeCollaborationList(state: any, request: any, messages: any[]): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.messages = mergeRuntimeCollaborationMessages([], messages);
  reconcileRuntimeCollaborationMutationState(state, true);
  return true;
}

export function adoptRuntimeCollaborationObservation(state: any, request: any, payload: any): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.messages = mergeRuntimeCollaborationMessages(
    state.collaboration.messages,
    Array.isArray(payload?.messages) ? payload.messages : []
  );
  if (typeof payload?.observation_token === "string") state.collaboration.observationToken = payload.observation_token;
  reconcileRuntimeCollaborationMutationState(state, false);
  return true;
}

export function setRuntimeCollaborationAvailable(state: any, request: any, available: boolean): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.available = available;
  if (!available) {
    state.collaboration.editTargetId = "";
    state.collaboration.replyTargetId = "";
  }
  return true;
}

export function setRuntimeCollaborationPhase(
  state: any,
  request: any,
  phase: "idle" | "reconnecting" | "live" | "paused"
): boolean {
  if (!isCurrentRuntimeCollaborationRequest(state, request)) return false;
  state.collaboration.phase = phase;
  return true;
}

export function runtimeCollaborationNeedsRefreshRecovery(state: any): boolean {
  return state?.collaboration?.phase === "paused";
}

export function isCurrentRuntimeWorkflowSessionRequest(state: any, request: any): boolean {
  return !!request && request.credentialGeneration === state.credentialGeneration &&
    request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
    isCurrentWorkflowSessionDetailRequest(state.workflow, { sessionId: request.sessionId, generation: request.generation });
}

export function adoptRuntimeWorkflowSessionDetail(state: any, request: any, detail: any): boolean {
  if (!isCurrentRuntimeWorkflowSessionRequest(state, request)) return false;
  return adoptWorkflowSessionDetail(
    state.workflow,
    { sessionId: request.sessionId, generation: request.generation },
    detail
  );
}

export function resolveRunnerDisclosure(storedDisclosure: boolean | null, defaultOpen: boolean): boolean {
  return storedDisclosure === null ? defaultOpen : storedDisclosure;
}

export function runtimeWindowShortKey(value: unknown): string {
  const key = String(value || "");
  if (key.length <= 14) return key;
  return key.slice(0, 8) + "…" + key.slice(-4);
}

export function runtimeWindowActivityLabel(timestampMs: unknown, nowMs: number): string {
  const value = Number(timestampMs);
  if (!Number.isFinite(value) || value <= 0) return "No WebCodex activity";
  const elapsed = Math.max(0, nowMs - value);
  if (elapsed < 1000) return "just now";
  if (elapsed < 60_000) return Math.floor(elapsed / 1000) + "s ago";
  if (elapsed < 3_600_000) return Math.floor(elapsed / 60_000) + "m ago";
  if (elapsed < 86_400_000) return Math.floor(elapsed / 3_600_000) + "h ago";
  return Math.floor(elapsed / 86_400_000) + "d ago";
}

export type RuntimeContextPresentationMode = "docked" | "popover" | "sheet";

export interface RuntimeContextLayoutOptions {
  userIntent: boolean | null;
  isWideViewport: boolean;
  isMobileViewport: boolean;
  hasSelectedSession: boolean;
  workspaceView: "sessions" | "operations" | "windows";
}

export interface RuntimeContextResolvedState {
  visible: boolean;
  presentationMode: RuntimeContextPresentationMode;
  isDocked: boolean;
}

export function resolveRuntimeContextPresentationMode(
  isWideViewport: boolean,
  isMobileViewport: boolean
): RuntimeContextPresentationMode {
  if (isMobileViewport) return "sheet";
  if (isWideViewport) return "docked";
  return "popover";
}

export function resolveRuntimeContextState(
  options: RuntimeContextLayoutOptions
): RuntimeContextResolvedState {
  const presentationMode = resolveRuntimeContextPresentationMode(
    options.isWideViewport,
    options.isMobileViewport
  );
  const sessionAvailable = options.hasSelectedSession && options.workspaceView === "sessions";
  if (!sessionAvailable) {
    return {
      visible: false,
      presentationMode,
      isDocked: false,
    };
  }
  const visible = options.userIntent !== null
    ? options.userIntent
    : options.isWideViewport;
  const isDocked = visible && presentationMode === "docked";
  return {
    visible,
    presentationMode,
    isDocked,
  };
}

export type RuntimeContextUserAction =
  | { type: "toggle_trigger"; currentVisible: boolean }
  | { type: "explicit_open" }
  | { type: "explicit_close" };

export function reduceRuntimeContextUserIntent(
  _previousIntent: boolean | null,
  action: RuntimeContextUserAction
): boolean {
  switch (action.type) {
    case "toggle_trigger":
      return !action.currentVisible;
    case "explicit_open":
      return true;
    case "explicit_close":
      return false;
  }
}

export interface RuntimeContextFocusTransitionOptions {
  wasDocked: boolean;
  nextDocked: boolean;
  isTriggerFocused: boolean;
}

export type RuntimeContextFocusTarget = "inspector_close" | "none";

export function resolveRuntimeContextFocusTransition(
  options: RuntimeContextFocusTransitionOptions
): RuntimeContextFocusTarget {
  if (!options.wasDocked && options.nextDocked && options.isTriggerFocused) {
    return "inspector_close";
  }
  return "none";
}
