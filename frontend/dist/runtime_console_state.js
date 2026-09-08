import { initialWorkflowSessionState, selectWorkflowSession, refreshWorkflowSessionDetail, clearWorkflowSessionSelection, isCurrentWorkflowSessionDetailRequest, adoptWorkflowSessionDetail, } from "./workflow_session_state.js";
function compareText(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
export class RuntimeCommunicationRefreshCoordinator {
    constructor(runRefresh) {
        this.runRefresh = runRefresh;
        this.generation = 0;
        this.inFlight = null;
    }
    refresh(includeData = true) {
        const generation = this.generation;
        const current = this.inFlight;
        if (current && current.generation === generation) {
            if (!includeData || current.includeData)
                return current.promise;
            return current.promise.then(() => this.generation === generation ? this.refresh(true) : false, () => this.generation === generation ? this.refresh(true) : false);
        }
        const promise = Promise.resolve().then(() => this.runRefresh(includeData));
        const started = { includeData, generation, promise };
        this.inFlight = started;
        const clear = () => {
            if (this.inFlight === started)
                this.inFlight = null;
        };
        void promise.then(clear, clear);
        return promise;
    }
    reset() {
        this.generation += 1;
        this.inFlight = null;
    }
}
export function runtimeCommunicationTranscriptAfterSeq(lastSeq, limit = 100) {
    const normalizedLastSeq = typeof lastSeq === "number" && Number.isSafeInteger(lastSeq)
        ? Math.max(0, lastSeq)
        : 0;
    const normalizedLimit = Number.isSafeInteger(limit) && limit > 0 ? limit : 100;
    return Math.max(0, normalizedLastSeq - normalizedLimit);
}
export function runtimeWorkflowSessionSummaryRevision(session) {
    if (!session)
        return "";
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
export function runtimeWorkflowSessionSummaryChanged(previous, next) {
    return runtimeWorkflowSessionSummaryRevision(previous) !== runtimeWorkflowSessionSummaryRevision(next);
}
function emptyCollaborationState() {
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
function messageCreatedAt(message) {
    return typeof message?.created_at === "number" ? message.created_at : 0;
}
const RUNTIME_COLLABORATION_MUTABLE_KINDS = new Set(["note", "guidance", "question", "todo"]);
export function runtimeCollaborationMessageCanMutate(message) {
    return !!message && message.status === "open" && RUNTIME_COLLABORATION_MUTABLE_KINDS.has(String(message.kind || ""));
}
export function runtimeCollaborationMessageSides(messages, locallyAuthoredMessageIds = new Set()) {
    const sides = new Map();
    for (const message of Array.isArray(messages) ? messages : []) {
        const id = typeof message?.message_id === "string" ? message.message_id : "";
        if (!id)
            continue;
        const side = message?.author_session_id
            ? "incoming"
            : locallyAuthoredMessageIds.has(id)
                ? "outgoing"
                : "neutral";
        sides.set(id, side);
    }
    return sides;
}
function collaborationMessageById(state, messageId) {
    return (Array.isArray(state?.collaboration?.messages) ? state.collaboration.messages : [])
        .find((message) => String(message?.message_id || "") === messageId) || null;
}
function reconcileRuntimeCollaborationMutationState(state, authoritativeRefresh = false) {
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
                ? collaboration.messages.find((message) => message?.supersedes_message_id === uncertain.messageId && message?.message === uncertain.message)
                : null);
            confirmedReplace = !!retainedReplacement
                && retainedReplacement?.supersedes_message_id === uncertain.messageId
                && retainedReplacement?.message === uncertain.message;
        }
        if (confirmedWithdraw || confirmedReplace) {
            collaboration.mutationNotice = confirmedWithdraw
                ? "Withdraw observed after refresh; exact replay required to confirm durability."
                : "Replacement observed after refresh; exact replay required to confirm durability.";
        }
        else if (authoritativeRefresh) {
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
export function mergeRuntimeCollaborationMessages(current, updates) {
    const byId = new Map();
    for (const message of Array.isArray(current) ? current : []) {
        const id = typeof message?.message_id === "string" ? message.message_id : "";
        if (id)
            byId.set(id, message);
    }
    for (const message of Array.isArray(updates) ? updates : []) {
        const id = typeof message?.message_id === "string" ? message.message_id : "";
        if (id)
            byId.set(id, message);
    }
    return Array.from(byId.values()).sort((left, right) => messageCreatedAt(left) - messageCreatedAt(right) ||
        compareText(String(left?.message_id || ""), String(right?.message_id || "")));
}
export function runtimeCollaborationObservationAction(payload) {
    if (payload?.history_lost)
        return "reload";
    if (payload?.has_more)
        return "drain";
    return "wait";
}
export function runtimeDeviceIds(projects) {
    const devices = new Set();
    for (const project of Array.isArray(projects) ? projects : []) {
        const clientId = typeof project?.client_id === "string" ? project.client_id : "";
        if (clientId)
            devices.add(clientId);
    }
    return Array.from(devices).sort(compareText);
}
export function runtimeProjectsForDevice(projects, clientId) {
    return (Array.isArray(projects) ? projects : [])
        .filter((project) => project && (!clientId || project.client_id === clientId) && typeof project.id === "string" && project.id)
        .slice()
        .sort((left, right) => {
        const leftName = typeof left.name === "string" && left.name ? left.name : left.id;
        const rightName = typeof right.name === "string" && right.name ? right.name : right.id;
        return compareText(leftName, rightName) || compareText(left.id, right.id);
    });
}
function projectAttentionCount(project) {
    const attention = project?.sessions?.attention;
    return ["open_guidance", "open_questions", "open_risks", "open_todos"]
        .reduce((total, key) => total + (typeof attention?.[key] === "number" ? Math.max(0, attention[key]) : 0), 0);
}
export function filterAndSortRuntimeProjects(projects, clientId, query) {
    const needle = String(query || "").trim().toLocaleLowerCase();
    return runtimeProjectsForDevice(projects, clientId)
        .filter((project) => {
        if (!needle)
            return true;
        return [project?.name, project?.id, project?.client_id, project?.path]
            .filter((value) => typeof value === "string")
            .some((value) => String(value).toLocaleLowerCase().includes(needle));
    })
        .sort((left, right) => {
        const leftRunning = typeof left?.sessions?.running_sessions === "number" ? left.sessions.running_sessions : 0;
        const rightRunning = typeof right?.sessions?.running_sessions === "number" ? right.sessions.running_sessions : 0;
        if (!!rightRunning !== !!leftRunning)
            return rightRunning ? 1 : -1;
        const leftAttention = projectAttentionCount(left);
        const rightAttention = projectAttentionCount(right);
        if (!!rightAttention !== !!leftAttention)
            return rightAttention ? 1 : -1;
        const leftUpdated = typeof left?.sessions?.latest_updated_at === "number" ? left.sessions.latest_updated_at : 0;
        const rightUpdated = typeof right?.sessions?.latest_updated_at === "number" ? right.sessions.latest_updated_at : 0;
        if (leftUpdated !== rightUpdated)
            return rightUpdated - leftUpdated;
        const leftName = typeof left?.name === "string" && left.name ? left.name : left.id;
        const rightName = typeof right?.name === "string" && right.name ? right.name : right.id;
        return compareText(String(leftName || ""), String(rightName || "")) || compareText(String(left?.id || ""), String(right?.id || ""));
    });
}
export function runtimeProjectIdentityText(project) {
    if (!project || typeof project.id !== "string" || !project.id)
        return "No project selected";
    const runner = typeof project.client_id === "string" && project.client_id ? project.client_id : "unknown";
    const path = typeof project.path === "string" && project.path ? project.path : "unavailable";
    return "Runner: " + runner + " · Project: " + project.id + " · Workspace: " + path;
}
export function preferredRuntimeProjectSelection(projects, selectedDevice, selectedProject) {
    const rows = Array.isArray(projects) ? projects : [];
    if (selectedProject) {
        const retained = rows.find((project) => project && project.id === selectedProject && typeof project.client_id === "string" && project.client_id);
        if (retained)
            return { device: retained.client_id, project: retained.id };
    }
    const devices = runtimeDeviceIds(rows);
    const device = devices.includes(selectedDevice) ? selectedDevice : "";
    return { device, project: "" };
}
export function initialRuntimeConsoleState() {
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
export function invalidateRuntimeCredential(state) {
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
export function beginRuntimeCredential(state) {
    invalidateRuntimeCredential(state);
    return refreshRuntimeProjects(state);
}
export function refreshRuntimeOverview(state) {
    state.overviewGeneration += 1;
    return { credentialGeneration: state.credentialGeneration, generation: state.overviewGeneration };
}
export function isCurrentRuntimeOverviewRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration && request.generation === state.overviewGeneration;
}
export function refreshRuntimeProjects(state, query = "", clientId = state.selectedDevice) {
    state.projectsGeneration += 1;
    return {
        credentialGeneration: state.credentialGeneration,
        projectGeneration: state.projectGeneration,
        generation: state.projectsGeneration,
        clientId: String(clientId || ""),
        query: String(query || "").trim(),
    };
}
export function isCurrentRuntimeProjectsRequest(state, request) {
    return !!request &&
        request.credentialGeneration === state.credentialGeneration &&
        request.projectGeneration === state.projectGeneration &&
        request.generation === state.projectsGeneration;
}
export function refreshRuntimeRunner(state) {
    if (!state.selectedDevice)
        return null;
    state.runnerGeneration += 1;
    return { credentialGeneration: state.credentialGeneration, device: state.selectedDevice, generation: state.runnerGeneration };
}
export function isCurrentRuntimeRunnerRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.device === state.selectedDevice && request.generation === state.runnerGeneration;
}
export function selectRuntimeRunnerFilter(state, device) {
    selectRuntimeProject(state, device, "");
}
export function selectRuntimeProject(state, device, project) {
    if (state.selectedDevice !== device)
        state.runnerGeneration += 1;
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
export function refreshRuntimeSessionList(state) {
    if (!state.selectedProject)
        return null;
    state.sessionListGeneration += 1;
    return {
        credentialGeneration: state.credentialGeneration,
        project: state.selectedProject,
        projectGeneration: state.projectGeneration,
        generation: state.sessionListGeneration,
    };
}
export function isCurrentRuntimeSessionListRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
        request.generation === state.sessionListGeneration;
}
function wrapWorkflowRequest(state, request) {
    if (!request || !state.selectedProject)
        return null;
    return {
        credentialGeneration: state.credentialGeneration,
        project: state.selectedProject,
        projectGeneration: state.projectGeneration,
        sessionId: request.sessionId,
        generation: request.generation,
    };
}
export function selectRuntimeWorkflowSession(state, sessionId) {
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
export function selectRuntimeSessionLocation(state, device, project, sessionId) {
    const sessionListRequest = selectRuntimeProject(state, device, project);
    const detailRequest = selectRuntimeWorkflowSession(state, sessionId);
    return { sessionListRequest, detailRequest };
}
export function refreshRuntimeWorkflowSession(state) {
    return wrapWorkflowRequest(state, refreshWorkflowSessionDetail(state.workflow));
}
export function clearRuntimeWorkflowSession(state) {
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
export function runtimeCollaborationRequest(state) {
    if (!state.selectedProject || !state.collaboration.sessionId)
        return null;
    return {
        credentialGeneration: state.credentialGeneration,
        project: state.selectedProject,
        projectGeneration: state.projectGeneration,
        sessionId: state.collaboration.sessionId,
        generation: state.collaboration.generation,
    };
}
export function isCurrentRuntimeCollaborationRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
        request.sessionId === state.collaboration.sessionId && request.generation === state.collaboration.generation;
}
export function setRuntimeCollaborationReplyTarget(state, messageId) {
    state.collaboration.replyTargetId = String(messageId || "");
    if (state.collaboration.replyTargetId)
        state.collaboration.editTargetId = "";
}
export function setRuntimeCollaborationEditTarget(state, messageId) {
    const id = String(messageId || "");
    const message = collaborationMessageById(state, id);
    if (!id || !runtimeCollaborationMessageCanMutate(message))
        return false;
    state.collaboration.editTargetId = id;
    state.collaboration.replyTargetId = "";
    state.collaboration.mutationNotice = "";
    return true;
}
export function clearRuntimeCollaborationEditTarget(state) {
    state.collaboration.editTargetId = "";
}
export function runtimeCollaborationEditTarget(state) {
    const id = String(state?.collaboration?.editTargetId || "");
    return id ? collaborationMessageById(state, id) : null;
}
export function markRuntimeCollaborationMutationUncertain(state, request, mutation) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.uncertainMutation = {
        kind: mutation?.kind === "replace" ? "replace" : "withdraw",
        messageId: String(mutation?.messageId || ""),
        ...(mutation?.kind === "replace" ? { message: String(mutation?.message || "") } : {}),
    };
    state.collaboration.mutationNotice = "Outcome unknown; refresh retained messages before retrying.";
    return true;
}
export function runtimeCollaborationMutationRecovery(state, request) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return null;
    const mutation = state?.collaboration?.uncertainMutation;
    const messageId = String(mutation?.messageId || "");
    if (!mutation || !messageId)
        return null;
    return {
        kind: mutation.kind === "replace" ? "replace" : "withdraw",
        messageId,
        ...(mutation.kind === "replace" ? { message: String(mutation.message || "") } : {}),
    };
}
export function completeRuntimeCollaborationMutationRecovery(state, request, notice) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.uncertainMutation = null;
    state.collaboration.mutationNotice = String(notice || "");
    return true;
}
export function takeRuntimeCollaborationMutationNotice(state) {
    const notice = String(state?.collaboration?.mutationNotice || "");
    state.collaboration.mutationNotice = "";
    return notice;
}
export function adoptRuntimeCollaborationList(state, request, messages) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.messages = mergeRuntimeCollaborationMessages([], messages);
    reconcileRuntimeCollaborationMutationState(state, true);
    return true;
}
export function adoptRuntimeCollaborationObservation(state, request, payload) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.messages = mergeRuntimeCollaborationMessages(state.collaboration.messages, Array.isArray(payload?.messages) ? payload.messages : []);
    if (typeof payload?.observation_token === "string")
        state.collaboration.observationToken = payload.observation_token;
    reconcileRuntimeCollaborationMutationState(state, false);
    return true;
}
export function setRuntimeCollaborationAvailable(state, request, available) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.available = available;
    if (!available) {
        state.collaboration.editTargetId = "";
        state.collaboration.replyTargetId = "";
    }
    return true;
}
export function setRuntimeCollaborationPhase(state, request, phase) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.phase = phase;
    return true;
}
export function runtimeCollaborationNeedsRefreshRecovery(state) {
    return state?.collaboration?.phase === "paused";
}
export function isCurrentRuntimeWorkflowSessionRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
        isCurrentWorkflowSessionDetailRequest(state.workflow, { sessionId: request.sessionId, generation: request.generation });
}
export function adoptRuntimeWorkflowSessionDetail(state, request, detail) {
    if (!isCurrentRuntimeWorkflowSessionRequest(state, request))
        return false;
    return adoptWorkflowSessionDetail(state.workflow, { sessionId: request.sessionId, generation: request.generation }, detail);
}
export function resolveRunnerDisclosure(storedDisclosure, defaultOpen) {
    return storedDisclosure === null ? defaultOpen : storedDisclosure;
}
export function runtimeWindowShortKey(value) {
    const key = String(value || "");
    if (key.length <= 14)
        return key;
    return key.slice(0, 8) + "…" + key.slice(-4);
}
export function runtimeWindowActivityLabel(timestampMs, nowMs) {
    const value = Number(timestampMs);
    if (!Number.isFinite(value) || value <= 0)
        return "No WebCodex activity";
    const elapsed = Math.max(0, nowMs - value);
    if (elapsed < 1000)
        return "just now";
    if (elapsed < 60000)
        return Math.floor(elapsed / 1000) + "s ago";
    if (elapsed < 3600000)
        return Math.floor(elapsed / 60000) + "m ago";
    if (elapsed < 86400000)
        return Math.floor(elapsed / 3600000) + "h ago";
    return Math.floor(elapsed / 86400000) + "d ago";
}
export function resolveRuntimeContextPresentationMode(isWideViewport, isMobileViewport) {
    if (isMobileViewport)
        return "sheet";
    if (isWideViewport)
        return "docked";
    return "popover";
}
export function resolveRuntimeContextState(options) {
    const presentationMode = resolveRuntimeContextPresentationMode(options.isWideViewport, options.isMobileViewport);
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
export function reduceRuntimeContextUserIntent(_previousIntent, action) {
    switch (action.type) {
        case "toggle_trigger":
            return !action.currentVisible;
        case "explicit_open":
            return true;
        case "explicit_close":
            return false;
    }
}
export function resolveRuntimeContextFocusTransition(options) {
    if (!options.wasDocked && options.nextDocked && options.isTriggerFocused) {
        return "inspector_close";
    }
    return "none";
}
