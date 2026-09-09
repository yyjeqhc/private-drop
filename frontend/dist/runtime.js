// DOM-free selection, response fencing, timeline-follow state, and narrow
// human-facing overview formatting for Workflow Sessions.
const FOLLOW_BOTTOM_THRESHOLD_PX = 24;
function overviewCount(value) {
    return typeof value === "number" && Number.isFinite(value) && value > 0
        ? Math.floor(value)
        : 0;
}
function countLabel(count, singular, plural = singular + "s") {
    return count + " " + (count === 1 ? singular : plural);
}
function validationOverviewFact(validation) {
    const state = String((validation && validation.state) || "unavailable");
    const retained = !!(validation && validation.history_truncated);
    const unresolved = overviewCount(validation && validation.unresolved_failure_count);
    if (state === "failed") {
        return {
            text: unresolved
                ? (retained ? "Retained: " : "") + countLabel(unresolved, "unresolved validation failure")
                : retained
                    ? "Latest retained validation failed"
                    : "Latest validation failed",
            tone: "fail",
        };
    }
    if (state === "passed") {
        return {
            text: retained ? "Latest retained validation passed" : "Latest validation passed",
            tone: "pass",
        };
    }
    if (state === "not_run") {
        return { text: "Validation not run", tone: "muted" };
    }
    return {
        text: retained
            ? "Retained terminal validation evidence unavailable"
            : "Terminal validation evidence unavailable",
        tone: "muted",
    };
}
function attentionOverviewParts(attention) {
    const parts = [];
    for (const [key, singular] of [
        ["open_risks", "risk"],
        ["open_todos", "todo"],
        ["open_questions", "question"],
        ["open_guidance", "guidance"],
    ]) {
        const count = overviewCount(attention && attention[key]);
        if (count) {
            parts.push(countLabel(count, singular));
        }
    }
    return parts;
}
function workOverviewParts(work, limit = 5) {
    const parts = [];
    for (const [key, singular, plural] of [
        ["edits", "edit", "edits"],
        ["validations", "validation", "validations"],
        ["exploration", "exploration", "exploration"],
        ["reviews", "review", "reviews"],
        ["runs", "run", "runs"],
    ]) {
        const count = overviewCount(work && work[key]);
        if (count) {
            parts.push(countLabel(count, singular, plural));
        }
        if (parts.length >= limit) {
            break;
        }
    }
    return parts;
}
function workflowSessionListOverviewFacts(overview) {
    if (!overview || typeof overview !== "object") {
        return [];
    }
    const facts = [validationOverviewFact(overview.validation)];
    const attention = attentionOverviewParts(overview.attention);
    if (attention.length) {
        facts.push({
            text: "Retained: " + attention.slice(0, 2).join(" · "),
            tone: overviewCount(overview.attention && overview.attention.open_risks) ? "fail" : "warn",
        });
    }
    const work = workOverviewParts(overview.work, 2);
    if (work.length) {
        facts.push({
            text: (overview.work && overview.work.history_truncated ? "Recent " : "") + work.join(" · "),
            tone: "runtime",
        });
    }
    return facts.slice(0, 3);
}
function workflowSessionOverviewPresentation(overview) {
    const value = overview && typeof overview === "object" ? overview : {};
    const work = workOverviewParts(value.work);
    const workText = work.length
        ? (value.work && value.work.history_truncated ? "Recent observed work: " : "Observed work: ") +
            work.join(" · ")
        : value.work && value.work.history_truncated
            ? "No work observations in retained events."
            : "No tool activity observed.";
    const validationFact = validationOverviewFact(value.validation);
    const validationParts = [validationFact.text];
    if (value.validation && value.validation.latest_kind) {
        validationParts.push("latest " + String(value.validation.latest_kind));
    }
    const testsRun = overviewCount(value.validation && value.validation.tests_run_count);
    if (testsRun || (value.validation && value.validation.tests_run_count === 0)) {
        validationParts.push(countLabel(testsRun, "test"));
    }
    const unresolved = overviewCount(value.validation && value.validation.unresolved_failure_count);
    if (unresolved && !validationFact.text.includes("unresolved validation failure")) {
        validationParts.push(countLabel(unresolved, "unresolved failure"));
    }
    const attention = attentionOverviewParts(value.attention);
    const attentionText = attention.length
        ? "Retained open messages: " + attention.join(" · ")
        : "No retained open guidance, questions, risks, or todos.";
    const progress = value.reported_progress && typeof value.reported_progress === "object"
        ? value.reported_progress
        : null;
    return {
        workText,
        validationText: validationParts.join(" · "),
        validationTone: validationFact.tone,
        validationAt: value.validation && typeof value.validation.latest_at === "number"
            ? value.validation.latest_at
            : null,
        attentionText,
        attentionTone: overviewCount(value.attention && value.attention.open_risks)
            ? "fail"
            : attention.length
                ? "warn"
                : "muted",
        progressText: progress && progress.text ? String(progress.text) : "No retained model-reported progress.",
        progressAt: progress && typeof progress.reported_at === "number" ? progress.reported_at : null,
    };
}
function hasPendingAttention(overview) {
    const attention = overview && typeof overview === "object" ? overview.attention : null;
    return ["open_guidance", "open_questions", "open_risks", "open_todos"]
        .some((key) => overviewCount(attention && attention[key]) > 0);
}
function idleAgeLabel(ageSeconds) {
    if (ageSeconds < 60)
        return "<1m";
    const minutes = Math.floor(ageSeconds / 60);
    if (minutes < 60)
        return minutes + "m";
    const hours = Math.floor(minutes / 60);
    if (hours < 24)
        return hours + "h";
    return Math.floor(hours / 24) + "d";
}
function workflowSessionLivenessPresentation(session, nowSeconds = Date.now() / 1000) {
    const runningCall = !!session?.running_call;
    const runningJobs = typeof session?.running_jobs === "number" ? Math.max(0, session.running_jobs) : 0;
    const tooltip = "WebCodex activity only; host/model state is unknown.";
    if (runningCall || runningJobs > 0) {
        return { state: "working", label: "working", tooltip };
    }
    const updatedAt = typeof session?.updated_at === "number" ? session.updated_at : 0;
    const ageSeconds = updatedAt > 0 ? Math.max(0, nowSeconds - updatedAt) : Number.POSITIVE_INFINITY;
    if (ageSeconds <= 120) {
        return { state: "recent", label: "recently active", tooltip };
    }
    if (hasPendingAttention(session?.overview)) {
        return { state: "attention", label: "idle · pending attention", tooltip };
    }
    return {
        state: "idle",
        label: Number.isFinite(ageSeconds) ? "idle · " + idleAgeLabel(ageSeconds) : "idle",
        tooltip,
    };
}
function workflowSessionIdleAttentionLabel(runningCall, overview) {
    return workflowSessionLivenessPresentation({ running_call: runningCall, overview, updated_at: 0 }, 0).label;
}
function initialWorkflowSessionState() {
    return {
        selectedSessionId: "",
        detailGeneration: 0,
        snapshot: null,
        followLatest: true,
    };
}
function selectWorkflowSession(state, sessionId) {
    state.selectedSessionId = sessionId;
    state.detailGeneration += 1;
    state.snapshot = null;
    state.followLatest = true;
    return workflowSessionDetailRequest(state);
}
function refreshWorkflowSessionDetail(state) {
    if (!state.selectedSessionId) {
        return null;
    }
    state.detailGeneration += 1;
    return workflowSessionDetailRequest(state);
}
function clearWorkflowSessionSelection(state) {
    state.selectedSessionId = "";
    state.detailGeneration += 1;
    state.snapshot = null;
    state.followLatest = true;
}
function workflowSessionDetailRequest(state) {
    if (!state.selectedSessionId) {
        return null;
    }
    return {
        sessionId: state.selectedSessionId,
        generation: state.detailGeneration,
    };
}
function isCurrentWorkflowSessionDetailRequest(state, request) {
    return !!request &&
        request.sessionId === state.selectedSessionId &&
        request.generation === state.detailGeneration;
}
function adoptWorkflowSessionDetail(state, request, detail) {
    if (!isCurrentWorkflowSessionDetailRequest(state, request)) {
        return false;
    }
    state.snapshot = detail;
    return true;
}
function updateWorkflowSessionFollowFromScroll(state, scrollTop, clientHeight, scrollHeight) {
    const distanceFromBottom = Math.max(0, scrollHeight - scrollTop - clientHeight);
    state.followLatest = distanceFromBottom <= FOLLOW_BOTTOM_THRESHOLD_PX;
    return state.followLatest;
}
function workflowSessionScrollTopAfterRender(state, previousScrollTop, clientHeight, scrollHeight) {
    if (shouldFollowWorkflowSessionLatest(state)) {
        return Math.max(0, scrollHeight - clientHeight);
    }
    return Math.min(Math.max(0, previousScrollTop), Math.max(0, scrollHeight - clientHeight));
}
function jumpWorkflowSessionToLatest(state) {
    state.followLatest = true;
}
function shouldFollowWorkflowSessionLatest(state) {
    return state.followLatest !== false;
}

function compareText(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
class RuntimeCommunicationRefreshCoordinator {
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
function runtimeCommunicationTranscriptAfterSeq(lastSeq, limit = 100) {
    const normalizedLastSeq = typeof lastSeq === "number" && Number.isSafeInteger(lastSeq)
        ? Math.max(0, lastSeq)
        : 0;
    const normalizedLimit = Number.isSafeInteger(limit) && limit > 0 ? limit : 100;
    return Math.max(0, normalizedLastSeq - normalizedLimit);
}
function runtimeWorkflowSessionSummaryRevision(session) {
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
function runtimeWorkflowSessionSummaryChanged(previous, next) {
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
function runtimeCollaborationMessageCanMutate(message) {
    return !!message && message.status === "open" && RUNTIME_COLLABORATION_MUTABLE_KINDS.has(String(message.kind || ""));
}
function runtimeCollaborationMessageSides(messages, locallyAuthoredMessageIds = new Set()) {
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
function mergeRuntimeCollaborationMessages(current, updates) {
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
function runtimeCollaborationObservationAction(payload) {
    if (payload?.history_lost)
        return "reload";
    if (payload?.has_more)
        return "drain";
    return "wait";
}
function runtimeDeviceIds(projects) {
    const devices = new Set();
    for (const project of Array.isArray(projects) ? projects : []) {
        const clientId = typeof project?.client_id === "string" ? project.client_id : "";
        if (clientId)
            devices.add(clientId);
    }
    return Array.from(devices).sort(compareText);
}
function runtimeProjectsForDevice(projects, clientId) {
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
function filterAndSortRuntimeProjects(projects, clientId, query) {
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
function runtimeProjectIdentityText(project) {
    if (!project || typeof project.id !== "string" || !project.id)
        return "No project selected";
    const runner = typeof project.client_id === "string" && project.client_id ? project.client_id : "unknown";
    const path = typeof project.path === "string" && project.path ? project.path : "unavailable";
    return "Runner: " + runner + " · Project: " + project.id + " · Workspace: " + path;
}
function preferredRuntimeProjectSelection(projects, selectedDevice, selectedProject) {
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
function initialRuntimeConsoleState() {
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
function invalidateRuntimeCredential(state) {
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
function beginRuntimeCredential(state) {
    invalidateRuntimeCredential(state);
    return refreshRuntimeProjects(state);
}
function refreshRuntimeOverview(state) {
    state.overviewGeneration += 1;
    return { credentialGeneration: state.credentialGeneration, generation: state.overviewGeneration };
}
function isCurrentRuntimeOverviewRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration && request.generation === state.overviewGeneration;
}
function refreshRuntimeProjects(state, query = "", clientId = state.selectedDevice) {
    state.projectsGeneration += 1;
    return {
        credentialGeneration: state.credentialGeneration,
        projectGeneration: state.projectGeneration,
        generation: state.projectsGeneration,
        clientId: String(clientId || ""),
        query: String(query || "").trim(),
    };
}
function isCurrentRuntimeProjectsRequest(state, request) {
    return !!request &&
        request.credentialGeneration === state.credentialGeneration &&
        request.projectGeneration === state.projectGeneration &&
        request.generation === state.projectsGeneration;
}
function refreshRuntimeRunner(state) {
    if (!state.selectedDevice)
        return null;
    state.runnerGeneration += 1;
    return { credentialGeneration: state.credentialGeneration, device: state.selectedDevice, generation: state.runnerGeneration };
}
function isCurrentRuntimeRunnerRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.device === state.selectedDevice && request.generation === state.runnerGeneration;
}
function selectRuntimeRunnerFilter(state, device) {
    selectRuntimeProject(state, device, "");
}
function selectRuntimeProject(state, device, project) {
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
function refreshRuntimeSessionList(state) {
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
function isCurrentRuntimeSessionListRequest(state, request) {
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
function selectRuntimeWorkflowSession(state, sessionId) {
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
function selectRuntimeSessionLocation(state, device, project, sessionId) {
    const sessionListRequest = selectRuntimeProject(state, device, project);
    const detailRequest = selectRuntimeWorkflowSession(state, sessionId);
    return { sessionListRequest, detailRequest };
}
function refreshRuntimeWorkflowSession(state) {
    return wrapWorkflowRequest(state, refreshWorkflowSessionDetail(state.workflow));
}
function clearRuntimeWorkflowSession(state) {
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
function runtimeCollaborationRequest(state) {
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
function isCurrentRuntimeCollaborationRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
        request.sessionId === state.collaboration.sessionId && request.generation === state.collaboration.generation;
}
function setRuntimeCollaborationReplyTarget(state, messageId) {
    state.collaboration.replyTargetId = String(messageId || "");
    if (state.collaboration.replyTargetId)
        state.collaboration.editTargetId = "";
}
function setRuntimeCollaborationEditTarget(state, messageId) {
    const id = String(messageId || "");
    const message = collaborationMessageById(state, id);
    if (!id || !runtimeCollaborationMessageCanMutate(message))
        return false;
    state.collaboration.editTargetId = id;
    state.collaboration.replyTargetId = "";
    state.collaboration.mutationNotice = "";
    return true;
}
function clearRuntimeCollaborationEditTarget(state) {
    state.collaboration.editTargetId = "";
}
function runtimeCollaborationEditTarget(state) {
    const id = String(state?.collaboration?.editTargetId || "");
    return id ? collaborationMessageById(state, id) : null;
}
function markRuntimeCollaborationMutationUncertain(state, request, mutation) {
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
function runtimeCollaborationMutationRecovery(state, request) {
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
function completeRuntimeCollaborationMutationRecovery(state, request, notice) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.uncertainMutation = null;
    state.collaboration.mutationNotice = String(notice || "");
    return true;
}
function takeRuntimeCollaborationMutationNotice(state) {
    const notice = String(state?.collaboration?.mutationNotice || "");
    state.collaboration.mutationNotice = "";
    return notice;
}
function adoptRuntimeCollaborationList(state, request, messages) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.messages = mergeRuntimeCollaborationMessages([], messages);
    reconcileRuntimeCollaborationMutationState(state, true);
    return true;
}
function adoptRuntimeCollaborationObservation(state, request, payload) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.messages = mergeRuntimeCollaborationMessages(state.collaboration.messages, Array.isArray(payload?.messages) ? payload.messages : []);
    if (typeof payload?.observation_token === "string")
        state.collaboration.observationToken = payload.observation_token;
    reconcileRuntimeCollaborationMutationState(state, false);
    return true;
}
function setRuntimeCollaborationAvailable(state, request, available) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.available = available;
    if (!available) {
        state.collaboration.editTargetId = "";
        state.collaboration.replyTargetId = "";
    }
    return true;
}
function setRuntimeCollaborationPhase(state, request, phase) {
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    state.collaboration.phase = phase;
    return true;
}
function runtimeCollaborationNeedsRefreshRecovery(state) {
    return state?.collaboration?.phase === "paused";
}
function isCurrentRuntimeWorkflowSessionRequest(state, request) {
    return !!request && request.credentialGeneration === state.credentialGeneration &&
        request.project === state.selectedProject && request.projectGeneration === state.projectGeneration &&
        isCurrentWorkflowSessionDetailRequest(state.workflow, { sessionId: request.sessionId, generation: request.generation });
}
function adoptRuntimeWorkflowSessionDetail(state, request, detail) {
    if (!isCurrentRuntimeWorkflowSessionRequest(state, request))
        return false;
    return adoptWorkflowSessionDetail(state.workflow, { sessionId: request.sessionId, generation: request.generation }, detail);
}
function resolveRunnerDisclosure(storedDisclosure, defaultOpen) {
    return storedDisclosure === null ? defaultOpen : storedDisclosure;
}
function runtimeWindowShortKey(value) {
    const key = String(value || "");
    if (key.length <= 14)
        return key;
    return key.slice(0, 8) + "…" + key.slice(-4);
}
function runtimeWindowActivityLabel(timestampMs, nowMs) {
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
function resolveRuntimeContextPresentationMode(isWideViewport, isMobileViewport) {
    if (isMobileViewport)
        return "sheet";
    if (isWideViewport)
        return "docked";
    return "popover";
}
function resolveRuntimeContextState(options) {
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
function reduceRuntimeContextUserIntent(_previousIntent, action) {
    switch (action.type) {
        case "toggle_trigger":
            return !action.currentVisible;
        case "explicit_open":
            return true;
        case "explicit_close":
            return false;
    }
}
function resolveRuntimeContextFocusTransition(options) {
    if (!options.wasDocked && options.nextDocked && options.isTriggerFocused) {
        return "inspector_close";
    }
    return "none";
}

const API_BASE = "/api/runtime-console/";
const REFRESH_MS = 30000;
const WINDOW_REFRESH_MS = 3000;
const COLLABORATION_WAIT_SECS = 25;
const PROJECT_SEARCH_DEBOUNCE_MS = 200;
const RUNTIME_CREDENTIAL_SESSION_KEY = "webcodex.runtime.credential.v1";
const APPEARANCE_STORAGE_KEY = "webcodex.runtime.appearance.v1";
const LANGUAGE_STORAGE_KEY = "webcodex.runtime.language.v1";
const WORKSPACE_VIEW_STORAGE_KEY = "webcodex.runtime.workspace-view.v1";
const DRAFT_STORAGE_PREFIX = "webcodex.runtime.draft.v1.";
const DEVICE_DISCLOSURE_STORAGE_PREFIX = "webcodex.runtime.runner-open.v1.";
const APPEARANCE_MEDIA_QUERY = "(prefers-color-scheme: light)";
const MOBILE_NAVIGATION_MEDIA = "(max-width: 900px)";
const WIDE_CONTEXT_MEDIA = "(min-width: 1280px)";
let contextUserIntent = null;
const RUNTIME_ZH_TEXT = {
    "Your workspace": "你的工作空间",
    "Pick up where work happens": "从这里继续工作",
    "Find a project": "查找项目",
    "Find a project…": "查找项目…",
    "Runtime overview": "运行概览",
    "WebCodex — Runtime Console": "WebCodex — 运行控制台",
    "WebCodex Runtime Console": "WebCodex 运行控制台",
    "A local workspace for Projects, Sessions, and collaboration": "用于管理项目、会话与协作的本地工作空间",
    "Appearance": "外观",
    "Choose appearance": "选择外观",
    "Color mode": "颜色模式",
    "System": "跟随系统",
    "Light": "浅色",
    "Dark": "深色",
    "Local runtime": "本地运行时",
    "Connect to your workspace": "连接到你的工作空间",
    "Enter an existing runtime Bearer credential. Project and Workflow Session views require their existing scopes; durable Agent Chat separately requires communication:read and communication:manage.": "输入已有的运行时 Bearer 凭证。项目和工作流会话视图需要相应权限；持久 Agent 对话还需要 communication:read 和 communication:manage。",
    "Runtime Bearer credential": "运行时 Bearer 凭证",
    "Connect": "连接",
    "Keep me signed in for this tab (survives refresh, clears on Lock or tab close)": "在此标签页保持登录（刷新后仍有效，锁定或关闭标签页时清除）",
    "Project and Session navigation": "项目与会话导航",
    "Local": "本地",
    "Close project navigation": "关闭项目导航",
    "Projects & Sessions": "项目与会话",
    "Workspace views": "工作空间视图",
    "Connected": "已连接",
    "Runtime & Agents": "运行时与 Agent",
    "Runtime workspace": "运行时工作区",
    "Inspect infrastructure and manage durable Agents without mixing administration into the current Session.": "检查基础设施并管理持久 Agent，同时避免将管理操作混入当前会话。",
    "Overview": "概览",
    "Details": "详情",
    "Session context views": "会话上下文视图",
    "Server health and fleet capacity": "服务器健康状态与设备群容量",
    "Runner fleet": "运行器设备群",
    "Devices, builds and current load": "设备、构建与当前负载",
    "Agents, inboxes and conversations": "Agent、收件箱与对话",
    "Local control plane": "本地控制平面",
    "Server health, Runner capacity, durable Agent identity, inboxes, and conversations.": "查看服务器健康状态、运行器容量、持久 Agent 标识、收件箱与对话。",
    "Live updates": "实时更新",
    "Infrastructure": "基础设施",
    "Devices": "设备",
    "Durable communication": "持久通信",
    "Agents & Conversations": "Agent 与对话",
    "Session conversation": "会话对话",
    "New messages": "新消息",
    "new message": "条新消息",
    "new messages": "条新消息",
    "Show full message": "展开完整消息",
    "Collapse message": "收起消息",
    "Copy code": "复制代码",
    "Code copied": "代码已复制",
    "Unable to copy code": "无法复制代码",
    "connected": "已连接",
    "Projects": "项目",
    "Runner": "运行器",
    "Latest Agent message": "最近的 Agent 留言",
    "Search loaded Sessions": "搜索已加载会话",
    "Title, id, or lifecycle": "标题、ID 或生命周期",
    "Search retained messages": "搜索已保留消息",
    "Message, resolution, or id": "消息内容、处理说明或 ID",
    "This board shows retained Session messages. ACK is not a reply or completion. Host chat replies appear here only when explicitly posted to this Session.": "这里展示会话中保留的协作消息。ACK 不代表回复或完成；宿主聊天中的回复只有明确发布到此会话后才会显示。",
    "All Runners": "全部运行器",
    "Filter by Project name, id, Runner, or workspace path": "按项目名称、ID、运行器或工作空间路径筛选",
    "No project selected": "尚未选择项目",
    "No Projects match this filter.": "没有符合当前筛选条件的项目。",
    "Sessions": "会话",
    "Workflow Sessions": "工作流会话",
    "No retained Workflow Sessions for this project.": "此项目没有保留的工作流会话。",
    "Working & Recently Updated Sessions": "正在工作与最近更新的会话",
    "Working and recently updated Workflow Sessions": "正在工作与最近更新的工作流会话",
    "No recent Workflow Sessions are visible.": "当前没有可见的最近工作流会话。",
    "Fleet-wide recent Sessions require runtime:read. Project-scoped Session access remains available.": "查看整个设备群的最近会话需要 runtime:read；仍可访问项目范围内的会话。",
    "Local Runtime": "本地运行时",
    "Credential stays in this tab only": "凭证仅保留在此标签页",
    "Open project navigation": "打开项目导航",
    "Current location": "当前位置",
    "Fleet": "设备群",
    "Select a Session": "选择一个会话",
    "Refresh runtime": "刷新运行时",
    "Lock": "锁定",
    "Choose a project and Workflow Session from the sidebar to inspect its context and continue the collaboration.": "从侧边栏选择项目和工作流会话，以查看上下文并继续协作。",
    "Conversation": "对话",
    "Collaboration messages require runtime:read. Existing project/session observability remains available.": "协作消息需要 runtime:read；现有的项目和会话观察能力仍可使用。",
    "Start this Session conversation": "开始此会话的对话",
    "Messages posted here are retained on the Session collaboration board.": "此处发送的消息会保留在会话协作板中。",
    "Retained board only; this is not a permanent or complete chat history. ACK observed is server-side evidence of an explicit echo, not a delivery or read receipt.": "这里只展示保留的协作板内容，并非永久或完整的聊天记录。已观察到 ACK 仅表示服务端收到明确回显，不代表送达或已读。",
    "Clear reply": "清除回复",
    "Cancel Edit": "取消编辑",
    "Message this Session…": "给此会话发送消息…",
    "Options": "选项",
    "Message options": "消息选项",
    "Applied to this message": "应用于本条消息",
    "Kind": "类型",
    "Note": "备注",
    "Guidance": "指导",
    "Question": "问题",
    "Todo": "待办",
    "Priority": "优先级",
    "Low": "低",
    "Normal": "普通",
    "High": "高",
    "Require acknowledgement": "需要确认",
    "Send message": "发送消息",
    "More actions": "更多操作",
    "Language": "语言",
    "Refresh": "刷新",
    "Runtime details": "运行时详情",
    "Session context": "会话上下文",
    "Close runtime details": "关闭运行时详情",
    "Close session context": "关闭会话上下文",
    "Context": "上下文",
    "Live": "实时",
    "Session": "会话",
    "Selected context": "已选上下文",
    "Workflow Session identity": "工作流会话标识",
    "Session ID": "会话 ID",
    "Lifecycle": "生命周期",
    "Mode": "模式",
    "Created": "创建时间",
    "Updated": "更新时间",
    "Workflow Session overview": "工作流会话概览",
    "Details & activity": "详情与活动",
    "IDs, validation, timeline": "标识、验证与时间线",
    "Work": "工作",
    "Validation": "验证",
    "Attention": "待处理",
    "Reported progress": "已报告进度",
    "Model-reported; informational only.": "由模型报告，仅供参考。",
    "Activity": "活动",
    "Jump to latest": "跳到最新",
    "No bounded activity is available.": "没有可用的有界活动记录。",
    "Server overview": "服务器概览",
    "Server": "服务器",
    "Runners": "运行器",
    "Collaboration attention": "协作待处理项",
    "Runtime-wide overview is unavailable to this credential; project-scoped Console access remains available.": "此凭证无法查看运行时全局概览；仍可使用项目范围的控制台访问。",
    "Runner Fleet": "运行器设备群",
    "No caller-visible Runners.": "没有调用方可见的运行器。",
    "Runtime-wide Runner facts require runtime:read.": "运行时全局运行器信息需要 runtime:read。",
    "Durable Agent Chat": "持久 Agent 对话",
    "Durable Conversation transcript, recipient-specific Inbox, and coalesced Wake Intent state. While Runtime & Agents is visible, the Console refreshes communication every 30 seconds; attached Endpoint leases are renewed every 30 seconds even outside that view. Polling, renewal, refresh, or unload cleanup does not invoke or wake a model.": "展示持久对话记录、收件人专属收件箱与合并后的唤醒意图状态。显示“运行时与 Agent”时，控制台每 30 秒刷新通信数据；即使离开该视图，已附加端点的租约仍每 30 秒续期。轮询、续期、刷新或卸载清理都不会调用或唤醒模型。",
    "Choose “Continue as this Agent” to bind this browser window to one durable Agent. The Console can poll and renew that bounded Endpoint lease, but it has no production model-resume adapter: pending Wake Intents remain durable until an explicit Host/model activation.": "选择“以此 Agent 继续”可将当前浏览器窗口绑定到一个持久 Agent。控制台可以轮询并续期该有界端点租约，但没有生产模型恢复适配器；待处理的唤醒意图会一直持久保留，直到主机或模型被明确激活。",
    "Durable Agent Chat requires communication:read. Project and Workflow Session access remain independent.": "持久 Agent 对话需要 communication:read；项目与工作流会话访问彼此独立。",
    "Agents": "Agent",
    "Handle": "标识名",
    "Display name": "显示名称",
    "Description": "描述",
    "What this Agent mainly does": "此 Agent 的主要职责",
    "Specialty labels": "专长标签",
    "Create Agent": "创建 Agent",
    "Durable Agents": "持久 Agent",
    "No durable Agents are owned by this communication principal.": "此通信主体尚未拥有持久 Agent。",
    "Agent Card": "Agent 卡片",
    "Update Agent Card": "更新 Agent 卡片",
    "No browser Endpoint attached.": "尚未附加浏览器端点。",
    "Attach this browser": "附加此浏览器",
    "Continue as this Agent": "以此 Agent 继续",
    "Detach": "分离",
    "Selected Agent Inbox": "所选 Agent 收件箱",
    "Consume visible": "消费可见项",
    "Select and attach an Agent to inspect recipient-specific queued deliveries.": "选择并附加一个 Agent，以查看收件人专属的排队投递。",
    "Conversations": "对话",
    "Title": "标题",
    "Agent IDs": "Agent ID",
    "Select an Agent or enter comma-separated wc_dagent_* ids": "选择 Agent，或输入以逗号分隔的 wc_dagent_* ID",
    "Create Conversation": "创建对话",
    "Durable Conversations": "持久对话",
    "No Conversations are visible to this Human principal.": "此人工主体目前没有可见对话。",
    "No messages yet.": "暂无消息。",
    "Inbox recipients": "收件箱接收方",
    "Blank = all Agent participants; empty delivery can be sent with [] through the API": "留空表示所有 Agent 参与者；可通过 API 使用 [] 发送不投递到收件箱的消息",
    "Send a Human-authored durable message…": "发送一条由人工撰写的持久消息…",
    "Send as the selected Agent through its exact attached Endpoint": "通过精确附加的端点，以所选 Agent 身份发送",
    "Send a durable message…": "发送一条持久消息…",
    "Send durable message": "发送持久消息",
    "Select or create a Conversation.": "选择或创建一个对话。",
    "Show more": "展开更多",
    "Recent Sessions": "最近会话",
    "Switch to Chinese": "切换到中文",
    "System appearance": "跟随系统外观",
    "Light appearance": "浅色外观",
    "Dark appearance": "深色外观",
    "No retained pending attention": "没有保留的待处理项",
    "No visible Projects": "没有可见项目",
    "Conversation access unavailable": "对话访问不可用",
    "This credential can inspect the Project and Session, but retained messages require runtime:read.": "此凭证可以查看项目和会话，但查看保留消息需要 runtime:read。",
    "Conversation access requires runtime:read": "对话访问需要 runtime:read",
    "Replace message": "替换消息",
    "Reply": "回复",
    "Replying to": "回复",
    "Original message unavailable": "原消息不可用",
    "You": "你",
    "Agent": "Agent",
    "Retained message": "保留消息",
    "Author provenance unavailable": "作者来源不可用",
    "Edit": "编辑",
    "Delete": "删除",
    "Consume": "消费",
    "Untitled Conversation": "未命名对话",
    "No description.": "暂无描述。",
    "time unavailable": "时间不可用",
    "working": "工作中",
    "recently active": "最近活跃",
    "idle · pending attention": "空闲 · 有待处理项",
    "idle": "空闲",
    "WebCodex activity only; host/model state is unknown.": "仅反映 WebCodex 活动；主机与模型状态未知。",
    "Now": "当前",
    "Last": "上次",
    "Reconnecting": "正在重连",
    "Paused": "已暂停",
    "Idle": "空闲",
    "OFFLINE": "离线",
    "online": "在线",
    "offline": "离线",
    "stale": "状态过期",
    "unknown": "未知",
    "note": "备注",
    "guidance": "指导",
    "question": "问题",
    "todo": "待办",
    "low": "低",
    "normal": "普通",
    "high": "高",
    "open": "开放",
    "resolved": "已解决",
    "Acknowledgement required": "需要确认",
    "Acknowledged": "已确认",
    "Withdrawn": "已撤回",
    "Replaced": "已替换",
    "Resolved": "已解决",
    "active": "活跃",
    "completed": "已完成",
    "none": "无",
    "attached": "已附加",
    "detached": "已分离",
    "expired": "已过期",
    "queued": "排队中",
    "consumed": "已消费",
    "passed": "已通过",
    "failed": "失败",
    "runtime:read unavailable": "runtime:read 不可用",
    "refresh unavailable": "刷新不可用",
    "project:read unavailable": "project:read 不可用",
    "build unavailable": "构建信息不可用",
    "Credential rejected.": "凭证已被拒绝。",
    "Credential does not have Runtime Console project access.": "此凭证没有运行控制台的项目访问权限。",
    "Runtime Console is unavailable.": "运行控制台当前不可用。",
    "Could not refresh projects.": "无法刷新项目。",
    "Selected project is no longer available.": "所选项目已不可用。",
    "Could not refresh Workflow Sessions.": "无法刷新工作流会话。",
    "Could not refresh Workflow Session detail.": "无法刷新工作流会话详情。",
    "Enter a runtime Bearer credential.": "请输入运行时 Bearer 凭证。",
    "Searching…": "正在搜索…",
    "Refreshing…": "正在刷新…",
    "Refreshed": "已刷新",
    "Refresh failed · showing previous data": "刷新失败 · 正在显示之前的数据",
    "Refreshing runtime": "正在刷新运行时",
    "Restoring this tab…": "正在恢复此标签页…",
    "Reply target cleared.": "已清除回复目标。",
    "Edit cancelled.": "已取消编辑。",
    "Enter a message.": "请输入消息。",
    "Sending…": "正在发送…",
    "Sent.": "已发送。",
    "Send failed.": "发送失败。",
    "Delete failed.": "删除失败。",
    "Replace failed.": "替换失败。",
    "Withdrawing retained message…": "正在撤回保留消息…",
    "Message changed before Delete. Refresh retained messages before retrying.": "删除前消息已发生变化。请刷新保留消息后再重试。",
    "Retained message withdrawn.": "保留消息已撤回。",
    "Replacing retained message…": "正在替换保留消息…",
    "Message changed before Replace. Refresh retained messages before retrying.": "替换前消息已发生变化。请刷新保留消息后再重试。",
    "Send outcome unknown. Refresh and review retained messages before retrying.": "发送结果未知。请先刷新并检查保留消息，再决定是否重试。",
    "Refreshing durable communication…": "正在刷新持久通信…",
    "Handle and display name are required.": "标识名和显示名称不能为空。",
    "Creating durable Agent…": "正在创建持久 Agent…",
    "Updating Agent Card…": "正在更新 Agent 卡片…",
    "Outcome uncertain. Refresh the Card before deciding whether to retry.": "操作结果不确定。请刷新 Agent 卡片后再决定是否重试。",
    "Agent Card update failed; refresh before retrying a stale revision.": "Agent 卡片更新失败；请刷新后再重试，避免使用过期版本。",
    "Agent Card updated.": "Agent 卡片已更新。",
    "Releasing this window’s previous Agent Endpoint…": "正在释放此窗口之前的 Agent 端点…",
    "Previous Endpoint detach is uncertain. Refresh before switching this window to another Agent.": "之前端点的分离结果不确定。请刷新后再将此窗口切换到其他 Agent。",
    "Could not release the previous Agent Endpoint.": "无法释放之前的 Agent 端点。",
    "The exact Attach replay was already replaced. Choose “Continue as this Agent” again to create a fresh Endpoint generation.": "这次精确附加重放已被替代。请再次选择“以此 Agent 继续”，创建新的端点代数。",
    "communication:manage required.": "需要 communication:manage 权限。",
    "Outcome uncertain. Keep inputs unchanged and retry to replay the same idempotency key, or refresh before deciding.": "操作结果不确定。请保持输入不变并重试以复用同一幂等键，或先刷新再决定。",
    "Attaching browser Endpoint…": "正在附加浏览器端点…",
    "Outcome uncertain. Retry Attach to replay the same idempotency key; do not create a new attachment.": "附加结果不确定。请重试附加以复用同一幂等键，不要创建新的附加记录。",
    "Detaching browser Endpoint…": "正在分离浏览器端点…",
    "Detach outcome uncertain. Refresh before retry; the durable Agent and Inbox are unaffected.": "分离结果不确定。请刷新后再重试；持久 Agent 和收件箱不受影响。",
    "At least one Agent id is required.": "至少需要一个 Agent ID。",
    "Creating durable Conversation…": "正在创建持久对话…",
    "Outcome uncertain. Keep inputs unchanged and retry to replay the same idempotency key.": "操作结果不确定。请保持输入不变并重试以复用同一幂等键。",
    "Select a Conversation and enter a message.": "请选择一个对话并输入消息。",
    "Select an Agent and choose “Continue as this Agent” before sending as it.": "请先选择一个 Agent 并点击“以此 Agent 继续”，然后再以其身份发送。",
    "Appending Message and Agent deliveries atomically…": "正在以原子方式写入消息和 Agent 投递…",
    "Outcome uncertain. Keep the message unchanged and retry only to replay the same idempotency key, or refresh the transcript first.": "操作结果不确定。请保持消息不变，仅在复用同一幂等键时重试，或先刷新对话记录。",
    "Consuming recipient state…": "正在消费接收方状态…",
    "communication:manage required to consume deliveries.": "消费投递需要 communication:manage 权限。",
    "Consume outcome uncertain. Refresh before retry; desired-state replay is safe.": "消费结果不确定。请刷新后再重试；目标状态重放是安全的。",
    "Delivery consume failed.": "消费投递失败。",
    "Existing idempotent Agent replayed.": "已重放现有的幂等 Agent。",
    "Agent created.": "Agent 已创建。",
    "Existing idempotent Conversation replayed.": "已重放现有的幂等对话。",
    "Conversation created.": "对话已创建。",
    "Existing Message replayed without duplicate delivery.": "已重放现有消息，未产生重复投递。",
    "Durable Message sent.": "持久消息已发送。",
    "Confirming replacement durability…": "正在确认替换操作的持久性…",
    "Confirming withdrawal durability…": "正在确认撤回操作的持久性…",
    "Replacement already retained.": "替换消息已保留。",
    "Message replaced.": "消息已替换。",
    "Withdraw observed after refresh; exact replay required to confirm durability.": "刷新后已观察到撤回结果；仍需精确重放以确认持久性。",
    "Replacement observed after refresh; exact replay required to confirm durability.": "刷新后已观察到替换结果；仍需精确重放以确认持久性。",
    "Outcome not observed in retained messages; exact replay required before live observation resumes.": "保留消息中未观察到操作结果；恢复实时观察前需要精确重放。",
    "Message changed while editing; current retained state was refreshed.": "编辑期间消息已变化；当前保留状态已刷新。",
    "Outcome unknown; refresh retained messages before retrying.": "操作结果未知；请刷新保留消息后再重试。",
    "Replacement durably confirmed after exact replay.": "精确重放后已确认替换操作持久保存。",
    "Withdraw durably confirmed after exact replay.": "精确重放后已确认撤回操作持久保存。",
    "durability confirmation still uncertain · refresh before retry": "持久性确认仍不确定 · 请刷新后再重试",
    "message changed during durability confirmation · refresh retained state": "持久性确认期间消息已变化 · 请刷新保留状态",
    "durability confirmation failed · refresh before retry": "持久性确认失败 · 请刷新后再重试",
    "establishing retained baseline": "正在建立保留消息基线",
    "Session unavailable": "会话不可用",
    "observation unavailable": "观察接口不可用",
    "retained snapshot failed": "保留消息快照获取失败",
    "bounded long-poll": "有界长轮询",
    "request failed": "请求失败",
    "retention changed · reloading": "保留窗口已变化 · 正在重新加载",
    "delta drain failed": "增量排空失败",
    "withdraw outcome unknown · refresh before retry": "撤回结果未知 · 请刷新后再重试",
    "message changed · refresh retained state": "消息已变化 · 请刷新保留状态",
    "replace outcome unknown · refresh before retry": "替换结果未知 · 请刷新后再重试",
    "send outcome unknown · refresh before retry": "发送结果未知 · 请刷新后再重试",
    "RUNNING": "运行中",
    "ATTENTION": "待处理",
    "STALE": "状态过期",
    "SOURCE DIFFERENT": "源码不一致",
    "BUILD DIFFERENT": "构建不一致",
    "DIRTY": "有未提交更改",
    "SESSION SCAN PARTIAL": "会话扫描不完整",
};
const ZH_COUNT_LABELS = {
    "Runner": "台运行器",
    "authorized Runner": "台已授权运行器",
    "Project": "个项目",
    "visible Project": "个可见项目",
    "matching Project": "个匹配项目",
    "Session": "个会话",
    "retained Session": "个保留会话",
    "active Session": "个活跃会话",
    "running Session": "个运行中会话",
    "Agent": "个 Agent",
    "active Endpoint": "个活跃端点",
    "active Job": "个活跃任务",
    "running Job": "个运行中任务",
    "queued Job": "个排队任务",
    "retained message": "条保留消息",
    "message": "条消息",
    "participant": "位参与者",
    "queued delivery": "条排队投递",
    "queued": "条排队项",
    "unresolved Wake": "个未解决唤醒",
    "risk": "个风险",
    "todo": "个待办",
    "question": "个问题",
    "guidance": "条指导",
    "online": "台在线",
    "stale": "台状态过期",
    "unavailable": "台不可用",
    "RUNNING": "个运行中",
};
const appearanceMedia = window.matchMedia(APPEARANCE_MEDIA_QUERY);
let runtimeLanguage = languagePreference(document.documentElement.dataset.language);
const staticTextSources = [];
const staticAttributeSources = [];
let token = "";
let rememberCredentialForTab = true;
let timer = 0;
let overviewAbort = null;
let projectsAbort = null;
let sessionsAbort = null;
let detailAbort = null;
let collaborationAbort = null;
let windowsAbort = null;
let windowDetailAbort = null;
let windowTimer = 0;
let windowRows = [];
let selectedWindowKey = "";
let selectedWindowDetail = null;
let projectRows = [];
let homeProjectRows = [];
let runnerRows = [];
let recentSessionRows = [];
let runtimeOverviewSnapshot = null;
let recentSessionMetaSnapshot = null;
let projectSearch = "";
let projectDeviceFilter = "";
let projectSearchTimer = 0;
let collaborationReplyTo = "";
let renderedCollaborationMessageIds = new Set();
let locallyAuthoredCollaborationMessageIds = new Set();
let workspaceView = "sessions";
let collaborationFollowLatest = true;
let collaborationPendingMessages = 0;
let refreshInFlight = false;
let projectRowsTotal = 0;
let projectRowsTruncated = false;
let knownProjectDevices = [];
let selectedProjectSnapshot = null;
let sessionRows = [];
let sessionListMetaSnapshot = { total: 0, truncated: false };
const state = initialRuntimeConsoleState();
let renderedProjectSelectorsSignature = "";
let renderedRunnerFleetSignature = "";
let renderedRecentSessionsSignature = "";
let renderedSessionListSignature = "";
let renderedCollaborationSignature = "";
let renderedCommunicationSurfaceSignature = "";
let communicationAgents = [];
let communicationConversations = [];
let communicationDetail = null;
let communicationInbox = [];
let selectedCommunicationAgentId = "";
let selectedCommunicationConversationId = "";
let communicationReadAvailable = null;
let communicationManageAvailable = null;
let communicationGeneration = 0;
const communicationRefreshCoordinator = new RuntimeCommunicationRefreshCoordinator(performCommunicationRefresh);
const communicationEndpoints = new Map();
const pendingEndpointAttach = new Map();
let pendingAgentCreate = null;
let pendingConversationCreate = null;
let pendingConversationMessage = null;
const pageAttachmentId = "runtime-console-" + operationKey("page");
function el(id) {
    return document.getElementById(id);
}
function setText(id, value) {
    const node = el(id);
    if (node)
        node.textContent = value === null || value === undefined ? "—" : String(value);
}
function show(id, visible) {
    const node = el(id);
    if (node)
        node.hidden = !visible;
}
function languagePreference(value) {
    return value === "zh-CN" ? "zh-CN" : "en";
}
function loadLanguagePreference() {
    try {
        const stored = window.localStorage.getItem(LANGUAGE_STORAGE_KEY);
        if (stored === "en" || stored === "zh-CN")
            return stored;
    }
    catch { /* Fall through to the browser language. */ }
    return navigator.language && navigator.language.toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}
function tr(source) {
    return runtimeLanguage === "zh-CN" ? (RUNTIME_ZH_TEXT[source] || source) : source;
}
function translatedStaticNodeValue(source) {
    const match = /^(\s*)([\s\S]*?)(\s*)$/.exec(source);
    if (!match)
        return source;
    return match[1] + tr(match[2]) + match[3];
}
function captureStaticUiSources() {
    if (staticTextSources.length || staticAttributeSources.length)
        return;
    const root = el("runtime-page");
    if (!root)
        return;
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let node = walker.nextNode();
    while (node) {
        const textNode = node;
        const source = textNode.nodeValue || "";
        if (source.trim())
            staticTextSources.push({ node: textNode, source });
        node = walker.nextNode();
    }
    for (const element of Array.from(root.querySelectorAll("*"))) {
        for (const name of ["placeholder", "title", "aria-label"]) {
            const source = element.getAttribute(name);
            if (source)
                staticAttributeSources.push({ node: element, name, source });
        }
    }
}
function renderLanguageSensitiveUi() {
    renderRuntimeOverviewMetrics(runtimeOverviewSnapshot);
    if (projectRows.length || knownProjectDevices.length || runnerRows.length) {
        renderProjectSelectors(projectRows, projectRowsTruncated);
    }
    else {
        renderSelectedProjectIdentity();
    }
    renderRunnerFleet(runnerRows);
    renderRecentSessions(recentSessionRows, recentSessionMetaSnapshot);
    if (state.selectedProject)
        renderSessionList(sessionRows, sessionListMetaSnapshot);
    const snapshot = state.workflow?.snapshot;
    if (snapshot)
        renderDetail(snapshot, false);
    else if (!state.workflow?.selectedSessionId)
        hideDetail();
    if (!snapshot)
        renderCollaboration(undefined, false);
    renderCommunicationSurface();
    syncCollaborationComposer();
    renderWorkspaceHeading();
    setRuntimeConnectionState(token ? "connected" : "disconnected");
}
function applyLanguage(language, persist = true, rerender = true) {
    runtimeLanguage = languagePreference(language);
    document.documentElement.lang = runtimeLanguage;
    document.documentElement.dataset.language = runtimeLanguage;
    document.title = tr("WebCodex — Runtime Console");
    for (const source of staticTextSources)
        source.node.nodeValue = translatedStaticNodeValue(source.source);
    for (const source of staticAttributeSources)
        source.node.setAttribute(source.name, tr(source.source));
    const nextLanguageLabel = runtimeLanguage === "zh-CN" ? "EN" : "中";
    const nextLanguageTitle = runtimeLanguage === "zh-CN" ? "切换到英文" : "Switch to Chinese";
    document.querySelectorAll("[data-language-toggle-label]").forEach((label) => {
        label.textContent = nextLanguageLabel;
    });
    document.querySelectorAll("[data-language-toggle]").forEach((button) => {
        button.title = nextLanguageTitle;
        button.setAttribute("aria-label", nextLanguageTitle);
    });
    applyAppearance(appearancePreference(document.documentElement.dataset.theme), false);
    if (persist) {
        try {
            window.localStorage.setItem(LANGUAGE_STORAGE_KEY, runtimeLanguage);
        }
        catch { /* Language remains active when storage is unavailable. */ }
    }
    if (rerender)
        renderLanguageSensitiveUi();
}
function appearancePreference(value) {
    return value === "light" || value === "dark" || value === "system" ? value : "system";
}
function loadAppearancePreference() {
    try {
        return appearancePreference(window.localStorage.getItem(APPEARANCE_STORAGE_KEY));
    }
    catch {
        return "system";
    }
}
function resolvedAppearance(preference) {
    if (preference !== "system")
        return preference;
    return appearanceMedia.matches ? "light" : "dark";
}
function applyAppearance(preference, persist = true) {
    const resolved = resolvedAppearance(preference);
    document.documentElement.dataset.theme = preference;
    document.documentElement.dataset.resolvedTheme = resolved;
    document.querySelector('meta[name="theme-color"]')?.setAttribute("content", resolved === "light" ? "#f4f4f1" : "#090a0d");
    document.querySelectorAll("[data-theme-option]").forEach((button) => {
        button.setAttribute("aria-pressed", button.dataset.themeOption === preference ? "true" : "false");
    });
    const label = preference === "system" ? tr("System appearance") : preference === "light" ? tr("Light appearance") : tr("Dark appearance");
    document.querySelectorAll(".theme-trigger").forEach((trigger) => {
        trigger.title = label;
        trigger.setAttribute("aria-label", runtimeLanguage === "zh-CN" ? label + "。" + tr("Choose appearance") : label + ". " + tr("Choose appearance"));
    });
    if (!persist)
        return;
    try {
        window.localStorage.setItem(APPEARANCE_STORAGE_KEY, preference);
    }
    catch { /* Appearance remains active when storage is unavailable. */ }
}
function workspaceViewPreference(value) {
    return value === "operations" || value === "windows" ? value : "sessions";
}
function loadWorkspaceViewPreference() {
    try {
        return workspaceViewPreference(window.localStorage.getItem(WORKSPACE_VIEW_STORAGE_KEY));
    }
    catch {
        return "sessions";
    }
}
function renderWorkspaceHeading() {
    if (workspaceView === "operations") {
        setText("runtime-breadcrumb-runner", tr("Runtime workspace"));
        setText("runtime-breadcrumb-project", tr("Local control plane"));
        setText("runtime-session-title", tr("Runtime & Agents"));
        return;
    }
    if (workspaceView === "windows") {
        setText("runtime-breadcrumb-runner", tr("Runtime workspace"));
        setText("runtime-breadcrumb-project", tr("Windows"));
        setText("runtime-session-title", selectedWindowKey ? "Window " + runtimeWindowShortKey(selectedWindowKey) : tr("Window activity"));
        return;
    }
    renderWorkspaceBreadcrumb();
    const snapshot = state.workflow?.snapshot;
    setText("runtime-session-title", snapshot?.title ? String(snapshot.title) : tr("Select a Session"));
}
function applyWorkspaceView(view, persist = true) {
    workspaceView = workspaceViewPreference(view);
    const operations = workspaceView === "operations";
    const windows = workspaceView === "windows";
    const sessions = workspaceView === "sessions";
    const shell = el("runtime-console");
    if (shell)
        shell.dataset.workspaceView = workspaceView;
    document.body.classList.toggle("runtime-operations-view", operations);
    document.body.classList.toggle("runtime-windows-view", windows);
    show("runtime-navigation-sessions", sessions);
    show("runtime-navigation-operations", operations);
    show("runtime-navigation-windows", windows);
    show("runtime-conversation-stage", sessions);
    show("runtime-operations-stage", operations);
    show("runtime-windows-stage", windows);
    document.querySelectorAll("[data-runtime-view]").forEach((button) => {
        const selected = button.dataset.runtimeView === workspaceView;
        button.classList.toggle("selected", selected);
        if (selected)
            button.setAttribute("aria-current", "page");
        else
            button.removeAttribute("aria-current");
    });
    if (operations && token)
        void refreshCommunication(true);
    if (windows && token) {
        void refreshWindows(true);
        startWindowAuto();
    }
    else {
        stopWindowAuto();
    }
    renderWorkspaceHeading();
    syncResponsiveNavigation();
    setMobileNavigationOpen(false, false);
    if (persist) {
        try {
            window.localStorage.setItem(WORKSPACE_VIEW_STORAGE_KEY, workspaceView);
        }
        catch { /* The selected view remains active when storage is unavailable. */ }
    }
}
function revealOperationsSection(targetId) {
    applyWorkspaceView("operations");
    const buttons = document.querySelectorAll("[data-operations-target]");
    if (!Array.from(buttons).some((button) => button.dataset.operationsTarget === targetId))
        return;
    buttons.forEach((button) => {
        const selected = button.dataset.operationsTarget === targetId;
        button.classList.toggle("selected", selected);
        if (selected)
            button.setAttribute("aria-current", "page");
        else
            button.removeAttribute("aria-current");
        show(String(button.dataset.operationsTarget), selected);
    });
    const scroll = document.querySelector(".operations-scroll");
    if (scroll)
        scroll.scrollTop = 0;
    el(targetId)?.focus({ preventScroll: true });
}
function setRuntimeConnectionState(connection) {
    const label = connection === "connected"
        ? tr("Connected")
        : connection === "connecting"
            ? tr("Reconnecting")
            : connection === "stale"
                ? tr("STALE")
                : tr("offline");
    document.querySelectorAll(".sidebar-runtime-status, .sidebar-connection, .operations-live").forEach((node) => {
        node.dataset.connection = connection;
    });
    const connectionLabel = document.querySelector(".sidebar-connection > span");
    if (connectionLabel)
        connectionLabel.textContent = label;
    setText("runtime-navigation-health", label);
}
function closeAppearanceMenus(restoreFocus = false, except = null) {
    let closed = false;
    document.querySelectorAll("details.theme-menu[open]").forEach((menu) => {
        if (menu === except)
            return;
        menu.open = false;
        closed = true;
        if (restoreFocus)
            menu.querySelector("summary")?.focus();
    });
    return closed;
}
function closeTopbarMore(restoreFocus = false) {
    const menu = el("runtime-topbar-more");
    if (!menu?.open)
        return false;
    menu.open = false;
    if (restoreFocus)
        menu.querySelector(":scope > summary")?.focus();
    return true;
}
function closeComposerOptions(restoreFocus = false) {
    const options = el("runtime-message-options");
    if (!options?.open)
        return false;
    options.open = false;
    if (restoreFocus)
        options.querySelector("summary")?.focus();
    return true;
}
function mobileNavigationViewport() {
    return window.matchMedia(MOBILE_NAVIGATION_MEDIA).matches;
}
function isContextDocked() {
    return !!el("runtime-console")?.classList.contains("context-docked");
}
function syncContextUi(restoreFocus = false) {
    const shell = el("runtime-console");
    const inspector = document.querySelector(".runtime-inspector");
    const trigger = inspector?.querySelector(".context-trigger");
    const wasDocked = !!shell?.classList.contains("context-docked");
    const isTriggerFocused = document.activeElement === trigger;
    const resolved = resolveRuntimeContextState({
        userIntent: contextUserIntent,
        isWideViewport: window.matchMedia(WIDE_CONTEXT_MEDIA).matches,
        isMobileViewport: mobileNavigationViewport(),
        hasSelectedSession: !!state.workflow?.selectedSessionId,
        workspaceView,
    });
    shell?.classList.toggle("context-docked", resolved.isDocked);
    if (inspector && inspector.open !== resolved.visible) {
        inspector.open = resolved.visible;
    }
    const focusTarget = resolveRuntimeContextFocusTransition({
        wasDocked,
        nextDocked: resolved.isDocked,
        isTriggerFocused,
    });
    if (focusTarget === "inspector_close") {
        el("runtime-inspector-close")?.focus();
    }
    else if (restoreFocus && !resolved.visible && trigger) {
        trigger.focus();
    }
}
function closeRuntimeInspector(restoreFocus = false, forceDocked = false) {
    if (isContextDocked() && !forceDocked)
        return false;
    const inspector = document.querySelector(".runtime-inspector");
    if (!inspector?.open && !isContextDocked())
        return false;
    contextUserIntent = reduceRuntimeContextUserIntent(contextUserIntent, { type: "explicit_close" });
    syncContextUi(restoreFocus);
    return true;
}
function setMobileNavigationOpen(open, restoreFocus = false, focusTarget = "runtime-mobile-nav-close") {
    const shell = el("runtime-console");
    const sidebar = el("runtime-sidebar");
    const toggle = el("runtime-mobile-nav-toggle");
    const mobile = mobileNavigationViewport();
    const nextOpen = mobile && open;
    shell?.classList.toggle("mobile-nav-open", nextOpen);
    toggle?.setAttribute("aria-expanded", nextOpen ? "true" : "false");
    if (sidebar) {
        if (mobile)
            sidebar.setAttribute("aria-hidden", nextOpen ? "false" : "true");
        else
            sidebar.removeAttribute("aria-hidden");
    }
    if (nextOpen) {
        closeAppearanceMenus(false);
        closeTopbarMore(false);
        closeRuntimeInspector(false);
        window.setTimeout(() => {
            if (mobileNavigationViewport() && shell?.classList.contains("mobile-nav-open"))
                el(focusTarget)?.focus();
        }, 260);
    }
    else if (restoreFocus && mobile) {
        window.setTimeout(() => toggle?.focus(), 0);
    }
}
function focusProjectNavigation() {
    applyWorkspaceView("sessions");
    if (mobileNavigationViewport())
        setMobileNavigationOpen(true, false, "runtime-project-search");
    else
        el("runtime-project-search")?.focus();
}
function syncResponsiveNavigation() {
    const shell = el("runtime-console");
    const sidebar = el("runtime-sidebar");
    const toggle = el("runtime-mobile-nav-toggle");
    syncContextUi();
    if (!mobileNavigationViewport()) {
        shell?.classList.remove("mobile-nav-open");
        sidebar?.removeAttribute("aria-hidden");
        toggle?.setAttribute("aria-expanded", "false");
        return;
    }
    const open = !!shell?.classList.contains("mobile-nav-open");
    sidebar?.setAttribute("aria-hidden", open ? "false" : "true");
    if (!open && sidebar?.contains(document.activeElement))
        toggle?.focus();
}
function visibleFocusableElements(container) {
    return Array.from(container.querySelectorAll('button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), summary, [href], [tabindex]:not([tabindex="-1"])')).filter((node) => node.offsetParent !== null);
}
function clearNode(node) {
    while (node && node.firstChild)
        node.removeChild(node.firstChild);
}
function renderFingerprint(value) {
    try {
        return JSON.stringify(value) || "";
    }
    catch {
        return "";
    }
}
const RUNTIME_ICON_PATHS = {
    folder: ["M3 6h7l2 2h9v10H3V6Z"],
    monitor: ["M4 5h16v12H4V5Z", "M8 21h8", "M12 17v4"],
    message: ["M5 5h14v12H9l-4 3V5Z", "M9 9h6", "M9 13h4"],
    reply: ["m10 8-5 4 5 4", "M5 12h7a6 6 0 0 1 6 6"],
    edit: ["m4 16-.5 4.5L8 20l10.5-10.5-4-4L4 16Z", "m12.5 7.5 4 4"],
    trash: ["M4 7h16", "M9 7V4h6v3", "m7 7 1 13h8l1-13", "M10 11v5", "M14 11v5"],
    copy: ["M8 8h11v11H8V8Z", "M5 16H4V4h12v1"],
};
function runtimeIcon(name, className = "") {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    if (className)
        svg.setAttribute("class", className);
    for (const pathData of RUNTIME_ICON_PATHS[name]) {
        const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
        path.setAttribute("d", pathData);
        svg.appendChild(path);
    }
    return svg;
}
function createMessageAction(label, iconName, action, danger = false) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "message-action" + (danger ? " danger" : "");
    button.title = label;
    button.setAttribute("aria-label", label);
    button.appendChild(runtimeIcon(iconName));
    button.addEventListener("click", action);
    return button;
}
function syncNewMessageIndicator() {
    const visible = collaborationPendingMessages > 0 && !collaborationFollowLatest;
    show("runtime-new-messages", visible);
    if (!visible)
        return;
    const label = runtimeLanguage === "zh-CN"
        ? String(collaborationPendingMessages) + " 条新消息"
        : String(collaborationPendingMessages) + " " + (collaborationPendingMessages === 1 ? "new message" : "new messages");
    setText("runtime-new-messages-label", label);
}
function chatIsNearLatest() {
    const scroll = el("runtime-chat-scroll");
    if (!scroll)
        return true;
    return scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight <= 96;
}
function updateCollaborationFollowFromScroll() {
    collaborationFollowLatest = chatIsNearLatest();
    if (collaborationFollowLatest)
        collaborationPendingMessages = 0;
    syncNewMessageIndicator();
}
function announceNewCollaborationMessages(count) {
    if (count <= 0)
        return;
    const label = runtimeLanguage === "zh-CN"
        ? String(count) + " 条新消息"
        : String(count) + " " + (count === 1 ? "new message" : "new messages");
    setText("runtime-message-announcer", label);
}
function appendLinkifiedText(parent, text) {
    const pattern = /https?:\/\/[^\s<>{}\[\]]+/g;
    let cursor = 0;
    for (const match of text.matchAll(pattern)) {
        const index = match.index || 0;
        if (index > cursor)
            parent.appendChild(document.createTextNode(text.slice(cursor, index)));
        let href = match[0];
        let trailing = "";
        while (/[.,;:!?)]$/.test(href)) {
            trailing = href.slice(-1) + trailing;
            href = href.slice(0, -1);
        }
        const link = document.createElement("a");
        link.href = href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.textContent = href;
        parent.appendChild(link);
        if (trailing)
            parent.appendChild(document.createTextNode(trailing));
        cursor = index + match[0].length;
    }
    if (cursor < text.length)
        parent.appendChild(document.createTextNode(text.slice(cursor)));
}
function messageLineStartsBlock(line) {
    return /^```/.test(line)
        || /^#{1,3}\s+/.test(line)
        || /^>\s?/.test(line)
        || /^\s*[-*+]\s+/.test(line)
        || /^\s*\d+[.)]\s+/.test(line);
}
function appendMessageParagraph(parent, lines) {
    if (!lines.length)
        return;
    const paragraph = document.createElement("p");
    paragraph.className = "message-paragraph";
    lines.forEach((line, index) => {
        if (index)
            paragraph.appendChild(document.createElement("br"));
        appendLinkifiedText(paragraph, line);
    });
    parent.appendChild(paragraph);
}
function appendMessageCode(parent, language, codeText) {
    const block = document.createElement("section");
    block.className = "message-code";
    const header = document.createElement("header");
    const label = document.createElement("span");
    label.textContent = language || "code";
    const copy = document.createElement("button");
    copy.type = "button";
    copy.className = "message-code-copy";
    copy.appendChild(runtimeIcon("copy"));
    const copyLabel = document.createElement("span");
    copyLabel.textContent = tr("Copy code");
    copy.appendChild(copyLabel);
    copy.title = tr("Copy code");
    copy.setAttribute("aria-label", tr("Copy code"));
    copy.addEventListener("click", async () => {
        try {
            await navigator.clipboard.writeText(codeText);
            copyLabel.textContent = tr("Code copied");
            setText("runtime-message-announcer", tr("Code copied"));
            window.setTimeout(() => { copyLabel.textContent = tr("Copy code"); }, 1400);
        }
        catch {
            copyLabel.textContent = tr("Unable to copy code");
            setText("runtime-message-announcer", tr("Unable to copy code"));
            window.setTimeout(() => { copyLabel.textContent = tr("Copy code"); }, 1800);
        }
    });
    header.appendChild(label);
    header.appendChild(copy);
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    if (language)
        code.dataset.language = language;
    code.textContent = codeText;
    pre.appendChild(code);
    block.appendChild(header);
    block.appendChild(pre);
    parent.appendChild(block);
}
function appendRichMessage(bubble, sourceValue) {
    const source = String(sourceValue || "").replace(/\r\n?/g, "\n");
    const lines = source.split("\n");
    const body = document.createElement("div");
    body.className = "message-body";
    let index = 0;
    while (index < lines.length) {
        const line = lines[index];
        if (!line.trim()) {
            index += 1;
            continue;
        }
        const fence = /^```\s*([^\s`]*)/.exec(line);
        if (fence) {
            const codeLines = [];
            index += 1;
            while (index < lines.length && !/^```\s*$/.test(lines[index])) {
                codeLines.push(lines[index]);
                index += 1;
            }
            if (index < lines.length)
                index += 1;
            appendMessageCode(body, fence[1] || "", codeLines.join("\n"));
            continue;
        }
        const heading = /^(#{1,3})\s+(.+)$/.exec(line);
        if (heading) {
            const title = document.createElement(heading[1].length === 1 ? "h3" : heading[1].length === 2 ? "h4" : "h5");
            title.className = "message-heading";
            appendLinkifiedText(title, heading[2]);
            body.appendChild(title);
            index += 1;
            continue;
        }
        if (/^>\s?/.test(line)) {
            const quote = document.createElement("blockquote");
            const quoteLines = [];
            while (index < lines.length && /^>\s?/.test(lines[index])) {
                quoteLines.push(lines[index].replace(/^>\s?/, ""));
                index += 1;
            }
            appendMessageParagraph(quote, quoteLines);
            body.appendChild(quote);
            continue;
        }
        const unordered = /^\s*[-*+]\s+/.test(line);
        const ordered = /^\s*\d+[.)]\s+/.test(line);
        if (unordered || ordered) {
            const list = document.createElement(ordered ? "ol" : "ul");
            const pattern = ordered ? /^\s*\d+[.)]\s+/ : /^\s*[-*+]\s+/;
            while (index < lines.length && pattern.test(lines[index])) {
                const item = document.createElement("li");
                appendLinkifiedText(item, lines[index].replace(pattern, ""));
                list.appendChild(item);
                index += 1;
            }
            body.appendChild(list);
            continue;
        }
        const paragraphLines = [];
        while (index < lines.length && lines[index].trim() && !messageLineStartsBlock(lines[index])) {
            paragraphLines.push(lines[index]);
            index += 1;
        }
        if (!paragraphLines.length) {
            paragraphLines.push(line);
            index += 1;
        }
        appendMessageParagraph(body, paragraphLines);
    }
    bubble.appendChild(body);
    if (source.length <= 2200 && lines.length <= 36)
        return;
    body.classList.add("is-collapsed");
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "message-expand";
    toggle.textContent = tr("Show full message");
    toggle.setAttribute("aria-expanded", "false");
    toggle.addEventListener("click", () => {
        const expanded = body.classList.toggle("is-expanded");
        body.classList.toggle("is-collapsed", !expanded);
        toggle.textContent = tr(expanded ? "Collapse message" : "Show full message");
        toggle.setAttribute("aria-expanded", expanded ? "true" : "false");
    });
    bubble.appendChild(toggle);
}
function loadRememberedRuntimeCredential() {
    try {
        return window.sessionStorage.getItem(RUNTIME_CREDENTIAL_SESSION_KEY)?.trim() || "";
    }
    catch {
        return "";
    }
}
function persistRuntimeCredentialForTab() {
    try {
        if (rememberCredentialForTab && token)
            window.sessionStorage.setItem(RUNTIME_CREDENTIAL_SESSION_KEY, token);
        else
            window.sessionStorage.removeItem(RUNTIME_CREDENTIAL_SESSION_KEY);
    }
    catch { /* Storage can be unavailable in hardened browser contexts. */ }
}
function clearRememberedRuntimeCredential() {
    try {
        window.sessionStorage.removeItem(RUNTIME_CREDENTIAL_SESSION_KEY);
    }
    catch { /* Storage can be unavailable in hardened browser contexts. */ }
}
function currentDraftStorageKey(project = state.selectedProject, sessionId = state.workflow?.selectedSessionId) {
    const projectId = String(project || "");
    const workflowSessionId = String(sessionId || "");
    return projectId && workflowSessionId
        ? DRAFT_STORAGE_PREFIX + encodeURIComponent(projectId) + "." + encodeURIComponent(workflowSessionId)
        : "";
}
function saveCurrentDraft() {
    const key = currentDraftStorageKey();
    const body = el("runtime-message-body");
    if (!key || !body)
        return;
    try {
        if (body.value)
            window.sessionStorage.setItem(key, body.value);
        else
            window.sessionStorage.removeItem(key);
    }
    catch { /* Draft remains available in the current input when storage is unavailable. */ }
}
function restoreCurrentDraft() {
    const key = currentDraftStorageKey();
    const body = el("runtime-message-body");
    if (!key || !body)
        return;
    try {
        body.value = window.sessionStorage.getItem(key) || "";
    }
    catch {
        body.value = "";
    }
    syncCollaborationComposerLayout();
}
function clearCurrentDraft() {
    const key = currentDraftStorageKey();
    if (!key)
        return;
    try {
        window.sessionStorage.removeItem(key);
    }
    catch { /* No-op in hardened browser contexts. */ }
}
function clearRuntimeDrafts() {
    try {
        const keys = [];
        for (let index = 0; index < window.sessionStorage.length; index += 1) {
            const key = window.sessionStorage.key(index);
            if (key?.startsWith(DRAFT_STORAGE_PREFIX))
                keys.push(key);
        }
        for (const key of keys)
            window.sessionStorage.removeItem(key);
    }
    catch { /* No-op in hardened browser contexts. */ }
}
function rememberLocalCollaborationMessage(messageId) {
    const id = typeof messageId === "string" ? messageId : "";
    if (!/^wc_msg_[A-Za-z0-9_]+$/.test(id))
        return;
    locallyAuthoredCollaborationMessageIds.add(id);
}
function deviceDisclosureStorageKey(clientId) {
    return DEVICE_DISCLOSURE_STORAGE_PREFIX + encodeURIComponent(clientId);
}
function storedDeviceDisclosure(clientId) {
    try {
        const value = window.localStorage.getItem(deviceDisclosureStorageKey(clientId));
        return value === "open" ? true : value === "closed" ? false : null;
    }
    catch {
        return null;
    }
}
function persistDeviceDisclosure(clientId, open) {
    try {
        window.localStorage.setItem(deviceDisclosureStorageKey(clientId), open ? "open" : "closed");
    }
    catch { /* Disclosure remains active for the current render. */ }
}
function revealRunner(clientId) {
    if (!clientId)
        return;
    persistDeviceDisclosure(clientId, true);
    const group = document.querySelector(`.device-group[data-runner-id="${CSS.escape(clientId)}"]`);
    if (group)
        group.open = true;
}
function appendChip(parent, text, extraClass = "") {
    const chip = document.createElement("span");
    chip.className = "chip" + (extraClass ? " " + extraClass : "");
    chip.textContent = text;
    parent.appendChild(chip);
    return chip;
}
function abort(controller) {
    if (controller)
        controller.abort();
}
function abortCollaboration() {
    abort(collaborationAbort);
    collaborationAbort = null;
}
function abortProjectWork() {
    abort(sessionsAbort);
    abort(detailAbort);
    abortCollaboration();
    sessionsAbort = null;
    detailAbort = null;
}
function stopProjectSearchTimer() {
    if (projectSearchTimer)
        window.clearTimeout(projectSearchTimer);
    projectSearchTimer = 0;
}
function abortAll() {
    abort(overviewAbort);
    abort(projectsAbort);
    overviewAbort = null;
    projectsAbort = null;
    stopProjectSearchTimer();
    abortProjectWork();
    abort(windowsAbort);
    abort(windowDetailAbort);
    windowsAbort = null;
    windowDetailAbort = null;
}
async function api(path, payload, signal) {
    try {
        const response = await fetch(API_BASE + path, {
            method: "POST",
            headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" },
            body: JSON.stringify(payload),
            signal,
        });
        let data = null;
        try {
            data = await response.json();
        }
        catch {
            data = null;
        }
        return { ok: response.ok, status: response.status, data };
    }
    catch (error) {
        if (error instanceof DOMException && error.name === "AbortError")
            return null;
        return { ok: false, status: 0, data: null };
    }
}
function windowDateTimeLabel(timestampMs) {
    const value = Number(timestampMs);
    if (!Number.isFinite(value) || value <= 0)
        return tr("time unavailable");
    return new Date(value).toLocaleString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en");
}
function windowAgeLabel(timestampMs) {
    return runtimeWindowActivityLabel(timestampMs, Date.now());
}
function runtimeProjectClientId(project) {
    const value = String(project || "");
    const parts = value.split(":");
    return parts.length >= 3 && parts[0] === "agent" ? parts[1] : "";
}
async function copyRuntimeValue(value, statusId) {
    if (!value)
        return;
    try {
        await navigator.clipboard.writeText(value);
        if (statusId)
            setText(statusId, tr("Copied"));
    }
    catch {
        if (statusId)
            setText(statusId, tr("Unable to copy"));
    }
}
function openWindowLinkedSession(session) {
    const project = String(session?.project || "");
    const sessionId = String(session?.workflow_session_id || "");
    const clientId = runtimeProjectClientId(project);
    if (!project || !sessionId || !clientId)
        return;
    applyWorkspaceView("sessions");
    selectRecentSession({ client_id: clientId, project_id: project, session_id: sessionId });
}
function renderWindowActivityRows(node, activities, compact = false) {
    clearNode(node);
    if (!node)
        return;
    for (const activity of activities) {
        const item = document.createElement("article");
        item.className = "window-activity-item" + (compact ? " compact" : "")
            + (activity?.recorder_gap_session_id ? " recorder-gap" : "");
        const head = document.createElement("div");
        head.className = "window-activity-head";
        const title = document.createElement("strong");
        title.textContent = String(activity?.tool_name || activity?.method || "WebCodex call");
        const time = document.createElement("span");
        time.className = "muted small";
        time.textContent = windowDateTimeLabel(activity?.started_at_ms);
        head.appendChild(title);
        head.appendChild(time);
        item.appendChild(head);
        const facts = document.createElement("div");
        facts.className = "chips window-activity-facts";
        appendChip(facts, String(activity?.status || "unknown"));
        if (activity?.project)
            appendChip(facts, String(activity.project));
        if (activity?.meaningful)
            appendChip(facts, "meaningful", "tone-runtime");
        if (activity?.recorder_gap_session_id)
            appendChip(facts, "recorder gap", "tone-warn");
        if (activity?.response_streaming === true) {
            appendChip(facts, "streaming timing unavailable", "tone-warn");
        }
        else if (typeof activity?.service_ms === "number") {
            appendChip(facts, "service " + String(activity.service_ms) + " ms");
        }
        else if (activity?.meaningful) {
            appendChip(facts, "service unavailable");
        }
        if (activity?.meaningful) {
            if (typeof activity?.next_call_gap_ms === "number") {
                appendChip(facts, "next gap " + String(activity.next_call_gap_ms) + " ms");
            }
            else {
                appendChip(facts, "next gap unavailable");
            }
            if (typeof activity?.cycle_ms === "number") {
                appendChip(facts, "cycle " + String(activity.cycle_ms) + " ms");
            }
        }
        if (activity?.window_transition_kind === "overlap") {
            appendChip(facts, "overlap from previous", "tone-warn");
        }
        item.appendChild(facts);
        const links = Array.isArray(activity?.workflow_sessions) ? activity.workflow_sessions : [];
        if (links.length) {
            const relation = document.createElement("div");
            relation.className = "muted small";
            relation.textContent = links
                .map((link) => String(link.workflow_session_id || "") + " · " + String(link.relation || "linked"))
                .join(" · ");
            item.appendChild(relation);
        }
        if (activity?.recorder_gap_session_id) {
            const gap = document.createElement("div");
            gap.className = "window-gap-note small";
            gap.textContent = "Recording was not continued for " + String(activity.recorder_gap_session_id) + ".";
            item.appendChild(gap);
        }
        if (activity?.server_trace_id) {
            const trace = document.createElement("button");
            trace.type = "button";
            trace.className = "window-trace-copy";
            trace.textContent = "trace " + String(activity.server_trace_id);
            trace.title = tr("Copy trace id");
            trace.addEventListener("click", () => void copyRuntimeValue(String(activity.server_trace_id)));
            item.appendChild(trace);
        }
        node.appendChild(item);
    }
}
function renderWindowList() {
    const node = el("runtime-window-list");
    clearNode(node);
    setText("runtime-window-list-count", String(windowRows.length));
    show("runtime-window-list-empty", windowRows.length === 0);
    setText("runtime-window-list-status", windowRows.length ? countLabel(windowRows.length, "Window") : tr("No WebCodex activity"));
    if (!node)
        return;
    for (const row of windowRows) {
        const key = String(row?.client_window_key || "");
        if (!key)
            continue;
        const button = document.createElement("button");
        button.type = "button";
        button.className = "runtime-window-card" + (key === selectedWindowKey ? " selected" : "");
        if (key === selectedWindowKey)
            button.setAttribute("aria-current", "true");
        const head = document.createElement("div");
        head.className = "runtime-window-card-head";
        const title = document.createElement("strong");
        title.textContent = "Window " + runtimeWindowShortKey(key);
        const active = document.createElement("span");
        active.className = "chip" + (Number(row?.active_count || 0) > 0 ? " tone-runtime" : "");
        active.textContent = Number(row?.active_count || 0) > 0
            ? String(row.active_count) + " active"
            : String(row?.source || "window");
        head.appendChild(title);
        head.appendChild(active);
        button.appendChild(head);
        const call = document.createElement("span");
        call.className = "muted small";
        call.textContent = row?.last_tool_call_at_ms
            ? "Last WebCodex call " + windowAgeLabel(row.last_tool_call_at_ms)
            : "Last WebCodex activity " + windowAgeLabel(row?.last_seen_at_ms);
        button.appendChild(call);
        const meaningful = document.createElement("span");
        meaningful.className = "muted small";
        meaningful.textContent = row?.last_meaningful_activity_at_ms
            ? "Last meaningful work " + windowAgeLabel(row.last_meaningful_activity_at_ms)
            : "No meaningful WebCodex work recorded";
        button.appendChild(meaningful);
        const links = document.createElement("span");
        links.className = "muted small";
        links.textContent = countLabel(Number(row?.linked_session_count || 0), "linked Session")
            + (Number(row?.recorder_gap_count || 0) ? " · " + String(row.recorder_gap_count) + " recorder gap" : "");
        button.appendChild(links);
        button.addEventListener("click", () => void selectWindow(key));
        node.appendChild(button);
    }
}
function renderWindowDetail(detail) {
    selectedWindowDetail = detail;
    const present = !!detail;
    show("runtime-window-detail-empty", !present);
    show("runtime-window-detail", present);
    if (!detail)
        return;
    const key = String(detail.client_window_key || selectedWindowKey || "");
    setText("runtime-window-title", "Window " + runtimeWindowShortKey(key));
    setText("runtime-window-key", key || "—");
    setText("runtime-window-source", String(detail.source || "—"));
    setText("runtime-window-active-count", String(Number(detail.active_count || 0)));
    setText("runtime-window-last-call", detail.last_tool_call_at_ms ? windowAgeLabel(detail.last_tool_call_at_ms) : "No completed tools/call activity");
    setText("runtime-window-last-meaningful", detail.last_meaningful_activity_at_ms
        ? windowAgeLabel(detail.last_meaningful_activity_at_ms)
        : "No meaningful WebCodex work recorded");
    setText("runtime-window-active-status", Number(detail.active_count || 0) ? "Active request" : "No active request");
    setText("runtime-window-linked-status", countLabel(Number(detail.sessions_returned || 0), "Session") + (detail.sessions_truncated ? " · bounded" : ""));
    setText("runtime-window-activity-status", countLabel(Number(detail.activity_returned || 0), "event") + (detail.activity_truncated ? " · bounded" : ""));
    const activeNode = el("runtime-window-active-requests");
    clearNode(activeNode);
    const activeRequests = Array.isArray(detail.active_requests) ? detail.active_requests : [];
    if (activeNode && !activeRequests.length) {
        const empty = document.createElement("p");
        empty.className = "muted small";
        empty.textContent = "No WebCodex request is currently active.";
        activeNode.appendChild(empty);
    }
    for (const request of activeRequests) {
        const item = document.createElement("article");
        item.className = "window-request-item";
        const title = document.createElement("strong");
        title.textContent = String(request?.tool_name || request?.method || "WebCodex request");
        item.appendChild(title);
        const meta = document.createElement("div");
        meta.className = "muted small";
        const facts = [
            request?.project,
            request?.started_at_ms ? "started " + windowAgeLabel(request.started_at_ms) : null,
            typeof request?.elapsed_ms === "number" ? String(request.elapsed_ms) + " ms elapsed" : null,
        ].filter(Boolean).map(String);
        meta.textContent = facts.join(" · ");
        item.appendChild(meta);
        if (request?.server_trace_id) {
            const trace = document.createElement("button");
            trace.type = "button";
            trace.className = "window-trace-copy";
            trace.textContent = "trace " + String(request.server_trace_id);
            trace.addEventListener("click", () => void copyRuntimeValue(String(request.server_trace_id)));
            item.appendChild(trace);
        }
        activeNode?.appendChild(item);
    }
    const sessionsNode = el("runtime-window-linked-sessions");
    clearNode(sessionsNode);
    for (const session of Array.isArray(detail.linked_sessions) ? detail.linked_sessions : []) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "window-session-card";
        const title = document.createElement("strong");
        title.textContent = String(session?.title || session?.workflow_session_id || "Workflow Session");
        const meta = document.createElement("span");
        meta.className = "muted small";
        meta.textContent = [
            session?.workflow_session_id,
            session?.lifecycle,
            session?.project,
            ...(Array.isArray(session?.relations) ? session.relations : []),
        ].filter(Boolean).map(String).join(" · ");
        button.appendChild(title);
        button.appendChild(meta);
        button.addEventListener("click", () => openWindowLinkedSession(session));
        sessionsNode?.appendChild(button);
    }
    if (sessionsNode && !sessionsNode.childElementCount) {
        const empty = document.createElement("p");
        empty.className = "muted small";
        empty.textContent = "No authorized Workflow Session links.";
        sessionsNode.appendChild(empty);
    }
    renderWindowActivityRows(el("runtime-window-activity"), Array.isArray(detail.activity) ? detail.activity : []);
    renderWorkspaceHeading();
}
async function refreshWindowDetail() {
    if (!token || !selectedWindowKey)
        return renderWindowDetail(null);
    abort(windowDetailAbort);
    const controller = new AbortController();
    windowDetailAbort = controller;
    const key = selectedWindowKey;
    const response = await api("window", { client_window_key: key, activity_limit: 100, session_limit: 50 }, controller.signal);
    if (windowDetailAbort === controller)
        windowDetailAbort = null;
    if (!response || key !== selectedWindowKey)
        return;
    if (response.status === 401)
        return lock("Credential rejected.");
    if (response.status === 404) {
        selectedWindowKey = "";
        renderWindowList();
        renderWindowDetail(null);
        return;
    }
    if (!response.ok || !response.data) {
        setText("runtime-window-list-status", response.status === 403 ? "runtime:read required" : "Could not refresh Window detail.");
        return;
    }
    renderWindowDetail(response.data);
}
async function selectWindow(key) {
    if (!/^[0-9a-fA-F]{64}$/.test(key))
        return;
    if (key !== selectedWindowKey)
        renderWindowDetail(null);
    selectedWindowKey = key;
    renderWindowList();
    renderWorkspaceHeading();
    setMobileNavigationOpen(false, true);
    await refreshWindowDetail();
}
async function refreshWindows(refreshSelected = true) {
    if (!token || workspaceView !== "windows")
        return;
    abort(windowsAbort);
    const controller = new AbortController();
    windowsAbort = controller;
    const response = await api("windows", { limit: 100 }, controller.signal);
    if (windowsAbort === controller)
        windowsAbort = null;
    if (!response)
        return;
    if (response.status === 401)
        return lock("Credential rejected.");
    if (response.status === 403) {
        windowRows = [];
        selectedWindowKey = "";
        renderWindowList();
        renderWindowDetail(null);
        setText("runtime-window-list-status", "runtime:read required");
        return;
    }
    if (!response.ok || !response.data) {
        setText("runtime-window-list-status", "Could not refresh Window activity.");
        return;
    }
    windowRows = Array.isArray(response.data.windows) ? response.data.windows : [];
    if (!selectedWindowKey && windowRows.length)
        selectedWindowKey = String(windowRows[0]?.client_window_key || "");
    renderWindowList();
    if (refreshSelected && selectedWindowKey) {
        await refreshWindowDetail();
    }
    else if (!selectedWindowKey) {
        renderWindowDetail(null);
    }
}
function renderSessionWindowCorrelation(detail) {
    const available = detail?.window_activity_available === true;
    show("runtime-linked-windows-unavailable", !available);
    const linkedNode = el("runtime-linked-windows");
    clearNode(linkedNode);
    const links = available && Array.isArray(detail?.linked_windows) ? detail.linked_windows : [];
    setText("runtime-linked-windows-status", available ? countLabel(links.length, "Window") : "runtime:read unavailable");
    for (const link of links) {
        const key = String(link?.client_window_key || "");
        if (!key)
            continue;
        const button = document.createElement("button");
        button.type = "button";
        button.className = "window-session-card" + (Number(link?.recorder_gap_count || 0) ? " recorder-gap" : "");
        const title = document.createElement("strong");
        title.textContent = "Window " + runtimeWindowShortKey(key);
        const meta = document.createElement("span");
        meta.className = "muted small";
        meta.textContent = [
            link?.source,
            link?.last_seen_at_ms ? "last WebCodex activity " + windowAgeLabel(link.last_seen_at_ms) : null,
            Number(link?.recorder_gap_count || 0) ? String(link.recorder_gap_count) + " recorder gap" : null,
        ].filter(Boolean).map(String).join(" · ");
        button.appendChild(title);
        button.appendChild(meta);
        button.addEventListener("click", () => {
            selectedWindowKey = key;
            applyWorkspaceView("windows");
            renderWindowList();
            void refreshWindowDetail();
        });
        linkedNode?.appendChild(button);
    }
    if (available && linkedNode && !links.length) {
        const empty = document.createElement("p");
        empty.className = "muted small";
        empty.textContent = "No linked Window evidence.";
        linkedNode.appendChild(empty);
    }
    const gaps = available && Array.isArray(detail?.window_activity_after_last_session_record)
        ? detail.window_activity_after_last_session_record
        : [];
    show("runtime-recorder-gap-panel", gaps.length > 0);
    renderWindowActivityRows(el("runtime-recorder-gap-activity"), gaps, true);
}
function hideDetail() {
    const messageSearch = el("runtime-message-search");
    if (messageSearch)
        messageSearch.value = "";
    setText("runtime-message-search-status", "");
    document.body.classList.remove("runtime-has-session");
    renderWorkspaceHeading();
    show("runtime-session-detail", false);
    show("runtime-session-context", false);
    show("runtime-session-detail-empty", true);
    show("runtime-jump-latest", false);
    setText("runtime-session-workspace", "");
    clearNode(el("runtime-collaboration-board"));
    renderedCollaborationMessageIds = new Set();
    renderedCollaborationSignature = "";
    collaborationFollowLatest = true;
    collaborationPendingMessages = 0;
    syncNewMessageIndicator();
    syncResponsiveNavigation();
}
function clearSessionSurface() {
    saveCurrentDraft();
    sessionRows = [];
    sessionListMetaSnapshot = { total: 0, truncated: false };
    const sessionSearch = el("runtime-session-search");
    if (sessionSearch)
        sessionSearch.value = "";
    renderedSessionListSignature = "";
    renderedCollaborationSignature = "";
    clearNode(el("runtime-session-list"));
    show("runtime-sessions-empty", false);
    clearRuntimeWorkflowSession(state);
    locallyAuthoredCollaborationMessageIds = new Set();
    abortCollaboration();
    hideDetail();
    resetCollaborationComposerUi();
}
function lock(message = "", clearRemembered = true) {
    setMobileNavigationOpen(false, false);
    closeRuntimeInspector(false, true);
    contextUserIntent = null;
    detachCommunicationEndpointsBestEffort();
    token = "";
    if (clearRemembered) {
        clearRememberedRuntimeCredential();
        clearRuntimeDrafts();
    }
    abortAll();
    invalidateRuntimeCredential(state);
    projectRows = [];
    homeProjectRows = [];
    runnerRows = [];
    recentSessionRows = [];
    runtimeOverviewSnapshot = null;
    recentSessionMetaSnapshot = null;
    projectRowsTotal = 0;
    projectRowsTruncated = false;
    knownProjectDevices = [];
    selectedProjectSnapshot = null;
    projectSearch = "";
    projectDeviceFilter = "";
    collaborationReplyTo = "";
    collaborationFollowLatest = true;
    collaborationPendingMessages = 0;
    clearSessionSurface();
    windowRows = [];
    selectedWindowKey = "";
    selectedWindowDetail = null;
    stopWindowAuto();
    renderWindowList();
    renderWindowDetail(null);
    resetCommunicationSurface();
    const projectList = el("runtime-project-list");
    const sessionsPanel = el("runtime-workflow-sessions-panel");
    renderedProjectSelectorsSignature = "";
    renderedRunnerFleetSignature = "";
    renderedRecentSessionsSignature = "";
    sessionsPanel?.remove();
    clearNode(projectList);
    if (projectList && sessionsPanel) {
        sessionsPanel.hidden = true;
        projectList.appendChild(sessionsPanel);
    }
    clearNode(el("runtime-recent-session-list"));
    clearNode(el("runtime-runner-list"));
    document.body.classList.remove("runtime-connected");
    const messageSearch = el("runtime-message-search");
    if (messageSearch)
        messageSearch.value = "";
    setText("runtime-message-search-status", "");
    document.body.classList.remove("runtime-has-session");
    show("runtime-token-gate", true);
    show("runtime-console", false);
    show("runtime-topbar-controls", false);
    stopAuto();
    setRuntimeConnectionState("disconnected");
    setText("runtime-token-error", message ? tr(message) : "");
    setText("runtime-refresh-status", "");
    const input = el("runtime-token-input");
    if (input) {
        input.value = "";
        input.focus();
    }
    const search = el("runtime-project-search");
    if (search)
        search.value = "";
}
function unlockUi() {
    persistRuntimeCredentialForTab();
    document.body.classList.add("runtime-connected");
    show("runtime-token-gate", false);
    show("runtime-console", true);
    show("runtime-topbar-controls", true);
    setText("runtime-token-error", "");
    setRuntimeConnectionState("connected");
    applyWorkspaceView(workspaceView, false);
    syncResponsiveNavigation();
    startAuto();
}
function showError(message) {
    setText("runtime-error", message ? tr(message) : "");
    show("runtime-error", !!message);
}
function countLabel(value, singular, plural = singular + "s") {
    const count = typeof value === "number" && Number.isFinite(value) ? Math.max(0, Math.floor(value)) : 0;
    if (runtimeLanguage === "zh-CN")
        return count + " " + (ZH_COUNT_LABELS[singular] || RUNTIME_ZH_TEXT[singular] || singular);
    return count + " " + (count === 1 ? singular : plural);
}
function pendingAttentionCount(attention) {
    return ["open_risks", "open_todos", "open_questions", "open_guidance"]
        .reduce((total, key) => total + (typeof attention?.[key] === "number" ? Math.max(0, Math.floor(attention[key])) : 0), 0);
}
function attentionLabel(attention) {
    const parts = [];
    for (const [key, singular] of [["open_risks", "risk"], ["open_todos", "todo"], ["open_questions", "question"], ["open_guidance", "guidance"]]) {
        const count = typeof attention?.[key] === "number" ? attention[key] : 0;
        if (count)
            parts.push(countLabel(count, singular));
    }
    return parts.length ? parts.join(" · ") : tr("No retained pending attention");
}
function renderRuntimeOverviewMetrics(data) {
    if (!data)
        return;
    setText("runtime-server-identity", [data.service, data.version].filter(Boolean).join(" · "));
    setText("runtime-server-build", data.build_git_commit
        ? (runtimeLanguage === "zh-CN" ? "构建 " : "build ") + data.build_git_commit + (data.build_git_dirty ? (runtimeLanguage === "zh-CN" ? " · 有未提交更改" : " · dirty") : "")
        : tr("build unavailable"));
    setText("runtime-server-runners", countLabel(data.runner_count, "Runner"));
    setText("runtime-server-alignment", countLabel(data.runners_online, "online") + " · " + countLabel(data.runners_stale, "stale") + " · " + countLabel(data.runners_unavailable, "unavailable"));
    setText("runtime-server-projects", data.projects_available ? countLabel(data.visible_projects, "visible Project") + (data.projects_truncated ? (runtimeLanguage === "zh-CN" ? " · 不完整" : " · partial") : "") : tr("project:read unavailable"));
    setText("runtime-server-jobs", countLabel(data.active_jobs, "active Job") + (data.mixed_builds_present ? (runtimeLanguage === "zh-CN" ? " · 存在混合构建" : " · mixed builds") : ""));
    setText("runtime-server-attention", attentionLabel(data.workflow_sessions));
    setText("runtime-server-sessions", countLabel(data.workflow_sessions?.active, "active Session") + " · " + countLabel(data.workflow_sessions?.running, "running Session") + (data.workflow_sessions?.truncated ? (runtimeLanguage === "zh-CN" ? " · 有界汇总" : " · bounded aggregate") : ""));
    const recentMeta = data.recent_sessions || {};
    setText("runtime-recent-status", countLabel(recentMeta.returned, "Session") +
        (recentMeta.truncated ? (runtimeLanguage === "zh-CN" ? " · 前 " : " · top ") + String(recentMeta.returned || 0) : "") +
        (recentMeta.scan_truncated ? (runtimeLanguage === "zh-CN" ? " · 扫描不完整" : " · partial scan") : ""));
}
async function fetchOverview(request) {
    abort(overviewAbort);
    const controller = new AbortController();
    overviewAbort = controller;
    const response = await api("overview", {}, controller.signal);
    if (overviewAbort === controller)
        overviewAbort = null;
    if (!response || !isCurrentRuntimeOverviewRequest(state, request))
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 403) {
        homeProjectRows = [];
        runnerRows = [];
        recentSessionRows = [];
        runtimeOverviewSnapshot = null;
        recentSessionMetaSnapshot = null;
        show("runtime-overview-unavailable", true);
        show("runtime-runner-unavailable", true);
        show("runtime-recent-unavailable", true);
        setText("runtime-overview-access", tr("runtime:read unavailable"));
        setText("runtime-runner-access", tr("runtime:read unavailable"));
        setText("runtime-recent-status", tr("runtime:read unavailable"));
        renderRunnerFleet([]);
        renderRecentSessions([], null);
        renderProjectSelectors(projectRows, projectRowsTruncated);
        setRuntimeConnectionState("connected");
        return true;
    }
    if (!response.ok || !response.data) {
        setText("runtime-overview-access", tr("refresh unavailable"));
        setText("runtime-runner-access", tr("refresh unavailable"));
        setText("runtime-recent-status", tr("refresh unavailable"));
        setRuntimeConnectionState("stale");
        return false;
    }
    show("runtime-overview-unavailable", false);
    show("runtime-runner-unavailable", false);
    show("runtime-recent-unavailable", false);
    setText("runtime-overview-access", "runtime:read");
    setText("runtime-runner-access", "runtime:read");
    const data = response.data;
    runtimeOverviewSnapshot = data;
    homeProjectRows = Array.isArray(data.projects) ? data.projects : [];
    runnerRows = Array.isArray(data.runners) ? data.runners : [];
    recentSessionRows = Array.isArray(data.recent_sessions?.sessions) ? data.recent_sessions.sessions : [];
    const recentMeta = data.recent_sessions || {};
    recentSessionMetaSnapshot = recentMeta;
    renderRuntimeOverviewMetrics(data);
    renderRecentSessions(recentSessionRows, recentMeta);
    renderRunnerFleet(runnerRows);
    renderProjectSelectors(projectRows, projectRowsTruncated);
    setRuntimeConnectionState("connected");
    return true;
}
function projectLabel(project) {
    const name = project && project.name ? String(project.name) : "";
    const id = project && project.id ? String(project.id) : "";
    const identity = name && name !== id ? name + " — " + id : id;
    const status = project && project.connected ? String(project.agent_status || "online") : "offline";
    return identity + " · " + status;
}
async function fetchProjects(request, unlocking = false) {
    const priorSelectedProject = selectedProjectRow();
    abort(projectsAbort);
    const controller = new AbortController();
    projectsAbort = controller;
    const payload = { limit: 100 };
    const clientId = String(request?.clientId || "");
    const query = String(request?.query || "").trim();
    if (clientId)
        payload.client_id = clientId;
    if (query)
        payload.query = query;
    const response = await api("projects", payload, controller.signal);
    if (projectsAbort === controller)
        projectsAbort = null;
    if (!response || !isCurrentRuntimeProjectsRequest(state, request))
        return false;
    if (response.status === 401 || response.status === 403) {
        lock("Credential does not have Runtime Console project access.");
        return false;
    }
    if (!response.ok || !response.data) {
        if (unlocking)
            lock("Runtime Console is unavailable.");
        else {
            showError("Could not refresh projects.");
            setRuntimeConnectionState("stale");
        }
        return false;
    }
    projectRows = Array.isArray(response.data.projects) ? response.data.projects : [];
    const reportedTotal = typeof response.data.total === "number" && Number.isFinite(response.data.total)
        ? Math.max(0, Math.floor(response.data.total))
        : projectRows.length;
    projectRowsTotal = Math.max(projectRows.length, reportedTotal);
    projectRowsTruncated = !!response.data.truncated;
    const known = new Set(knownProjectDevices);
    for (const device of runtimeDeviceIds(projectRows))
        known.add(device);
    knownProjectDevices = Array.from(known).sort((left, right) => left.localeCompare(right));
    if (priorSelectedProject && String(priorSelectedProject.id || "") === String(state.selectedProject || "")) {
        selectedProjectSnapshot = priorSelectedProject;
    }
    const refreshedSelected = effectiveProjects(projectRows).find((project) => String(project?.id || "") === String(state.selectedProject || ""));
    if (refreshedSelected)
        selectedProjectSnapshot = refreshedSelected;
    unlockUi();
    setRuntimeConnectionState("connected");
    showError("");
    const currentDevice = String(state.selectedDevice || "");
    const currentProject = String(state.selectedProject || "");
    if (query) {
        renderProjectSelectors(projectRows, projectRowsTruncated);
        renderSelectedProjectIdentity();
        return true;
    }
    const selection = preferredRuntimeProjectSelection(projectRows, currentDevice, currentProject);
    if (!selection.project) {
        if (currentProject && projectRowsTruncated) {
            renderProjectSelectors(projectRows, projectRowsTruncated);
            renderSelectedProjectIdentity();
            return true;
        }
        if (currentProject || selection.device !== currentDevice) {
            abortProjectWork();
            selectRuntimeRunnerFilter(state, selection.device || "");
            selectedProjectSnapshot = null;
            collaborationReplyTo = "";
            clearSessionSurface();
        }
        renderProjectSelectors(projectRows, projectRowsTruncated);
        renderSelectedProjectIdentity();
        return true;
    }
    if (selection.device !== currentDevice || selection.project !== currentProject) {
        switchProject(selection.device, selection.project);
    }
    else {
        renderProjectSelectors(projectRows, projectRowsTruncated);
        const listRequest = refreshRuntimeSessionList(state);
        if (listRequest)
            void fetchSessions(listRequest);
    }
    return true;
}
function effectiveProjects(projects) {
    const aggregates = new Map();
    for (const row of homeProjectRows) {
        if (row && typeof row.id === "string")
            aggregates.set(row.id, row);
    }
    return (Array.isArray(projects) ? projects : []).map((project) => {
        const aggregate = aggregates.get(String(project?.id || ""));
        return aggregate ? { ...project, sessions: aggregate.sessions } : project;
    });
}
function projectSelectorDevices(projects) {
    const devices = new Set(knownProjectDevices);
    for (const device of runtimeDeviceIds(projects))
        devices.add(device);
    for (const runner of runnerRows) {
        const clientId = typeof runner?.client_id === "string" ? runner.client_id : "";
        if (clientId)
            devices.add(clientId);
    }
    const selectedDevice = String(state.selectedDevice || "");
    if (selectedDevice)
        devices.add(selectedDevice);
    return Array.from(devices).sort((left, right) => left.localeCompare(right));
}
function selectedProjectRow() {
    const selected = String(state.selectedProject || "");
    if (!selected)
        return null;
    const current = effectiveProjects(projectRows).find((project) => String(project?.id || "") === selected);
    if (current)
        return current;
    return selectedProjectSnapshot && String(selectedProjectSnapshot.id || "") === selected
        ? selectedProjectSnapshot
        : null;
}
function renderWorkspaceBreadcrumb() {
    const project = selectedProjectRow();
    setText("runtime-breadcrumb-runner", project?.client_id ? String(project.client_id) : tr("Fleet"));
    setText("runtime-breadcrumb-project", project ? String(project.name || project.id || tr("Projects")) : tr("Projects"));
}
function renderSelectedProjectIdentity() {
    const project = selectedProjectRow();
    renderWorkspaceBreadcrumb();
    if (runtimeLanguage !== "zh-CN") {
        setText("runtime-selected-project", runtimeProjectIdentityText(project));
        return;
    }
    if (!project || typeof project.id !== "string" || !project.id) {
        setText("runtime-selected-project", tr("No project selected"));
        return;
    }
    const runner = typeof project.client_id === "string" && project.client_id ? project.client_id : tr("unknown");
    const path = typeof project.path === "string" && project.path ? project.path : "不可用";
    setText("runtime-selected-project", "运行器：" + runner + " · 项目：" + project.id + " · 工作空间：" + path);
}
function renderSessionWorkspaceIdentity() {
    const project = selectedProjectRow();
    if (runtimeLanguage !== "zh-CN")
        setText("runtime-session-workspace", runtimeProjectIdentityText(project));
    else if (!project || typeof project.id !== "string" || !project.id)
        setText("runtime-session-workspace", tr("No project selected"));
    else {
        const runner = typeof project.client_id === "string" && project.client_id ? project.client_id : tr("unknown");
        const path = typeof project.path === "string" && project.path ? project.path : "不可用";
        setText("runtime-session-workspace", "运行器：" + runner + " · 项目：" + project.id + " · 工作空间：" + path);
    }
}
function revealWorkflowSessionDetail() {
    const panel = el("runtime-workflow-sessions-panel");
    const workspace = panel?.closest("details.workspace-group");
    if (workspace)
        workspace.open = true;
    panel?.scrollIntoView({ block: "start", inline: "nearest" });
}
function renderProjectSelectors(projects, truncated) {
    const deviceSelect = el("runtime-device-select");
    const projectList = el("runtime-project-list");
    if (!deviceSelect || !projectList)
        return;
    const effective = effectiveProjects(projects);
    const signature = renderFingerprint({
        language: runtimeLanguage,
        selectedDevice: state.selectedDevice,
        selectedProject: state.selectedProject,
        projectDeviceFilter,
        projectSearch,
        truncated,
        projectRowsTotal,
        knownProjectDevices,
        projects: effective,
        runners: runnerRows.map((runner) => [runner?.client_id, runner?.connected, runner?.status]),
    });
    if (signature === renderedProjectSelectorsSignature)
        return;
    renderedProjectSelectorsSignature = signature;
    const sessionsPanel = el("runtime-workflow-sessions-panel");
    sessionsPanel?.remove();
    const devices = projectSelectorDevices(projects);
    clearNode(deviceSelect);
    const all = document.createElement("option");
    all.value = "";
    all.textContent = tr("All Runners");
    deviceSelect.appendChild(all);
    for (const clientId of devices) {
        const option = document.createElement("option");
        option.value = clientId;
        option.textContent = clientId;
        deviceSelect.appendChild(option);
    }
    deviceSelect.value = projectDeviceFilter;
    const rows = filterAndSortRuntimeProjects(effective, projectDeviceFilter, "");
    clearNode(projectList);
    show("runtime-projects-empty", rows.length === 0);
    const projectsByDevice = new Map();
    for (const project of rows) {
        const clientId = String(project?.client_id || "unknown");
        const deviceProjects = projectsByDevice.get(clientId) || [];
        deviceProjects.push(project);
        projectsByDevice.set(clientId, deviceProjects);
    }
    const visibleDevices = projectDeviceFilter ? [projectDeviceFilter] : devices;
    let sessionsAttached = false;
    for (const clientId of visibleDevices) {
        const deviceProjects = projectsByDevice.get(clientId) || [];
        const runner = runnerRows.find((candidate) => String(candidate?.client_id || "") === clientId);
        const connected = runner ? runner.connected !== false : deviceProjects.some((project) => project?.connected);
        const group = document.createElement("details");
        group.className = "device-group" + (connected ? " online" : " offline");
        group.dataset.runnerId = clientId;
        group.setAttribute("aria-label", runtimeLanguage === "zh-CN" ? "设备 " + clientId : "Device " + clientId);
        const storedDisclosure = storedDeviceDisclosure(clientId);
        const defaultOpen = projectDeviceFilter ? true : String(state.selectedDevice || "") === clientId || clientId === visibleDevices[0];
        group.open = resolveRunnerDisclosure(storedDisclosure, defaultOpen);
        const deviceHead = document.createElement("summary");
        deviceHead.className = "device-group-head";
        const deviceIcon = document.createElement("span");
        deviceIcon.className = "device-group-icon";
        deviceIcon.setAttribute("aria-hidden", "true");
        deviceIcon.appendChild(runtimeIcon("monitor"));
        const deviceIdentity = document.createElement("div");
        deviceIdentity.className = "device-group-identity";
        const deviceName = document.createElement("strong");
        deviceName.textContent = clientId;
        const deviceMeta = document.createElement("span");
        deviceMeta.className = "muted small";
        const status = runner ? String(runner.status || (connected ? "online" : "offline")) : (connected ? "online" : "offline");
        deviceMeta.textContent = tr(status) + " · " + countLabel(deviceProjects.length, "Project");
        const deviceDot = document.createElement("span");
        deviceDot.className = "device-group-dot";
        deviceDot.title = tr(status);
        deviceIdentity.appendChild(deviceName);
        deviceIdentity.appendChild(deviceMeta);
        deviceHead.appendChild(deviceIcon);
        deviceHead.appendChild(deviceIdentity);
        deviceHead.appendChild(deviceDot);
        group.appendChild(deviceHead);
        group.addEventListener("toggle", () => persistDeviceDisclosure(clientId, group.open));
        const deviceProjectList = document.createElement("div");
        deviceProjectList.className = "device-project-list";
        if (deviceProjects.length === 0) {
            const empty = document.createElement("p");
            empty.className = "device-project-empty muted small";
            empty.textContent = tr("No visible Projects");
            deviceProjectList.appendChild(empty);
        }
        for (const project of deviceProjects) {
            const workspace = document.createElement("details");
            workspace.className = "workspace-group";
            const disclosureKey = "webcodex.runtime.workspace-disclosure.v1." + encodeURIComponent(JSON.stringify([clientId, project.id]));
            workspace.open = project.id === state.selectedProject;
            try {
                workspace.open = workspace.open && window.localStorage.getItem(disclosureKey) !== "closed";
            }
            catch { }
            workspace.addEventListener("toggle", () => {
                if (!workspace.isConnected || project.id !== state.selectedProject)
                    return;
                try {
                    window.localStorage.setItem(disclosureKey, workspace.open ? "open" : "closed");
                }
                catch { }
            });
            const row = document.createElement("summary");
            row.className = "project-row" + (project.id === state.selectedProject ? " selected" : "");
            if (project.id === state.selectedProject)
                row.setAttribute("aria-current", "true");
            const projectName = String(project.name || project.id || "");
            const projectId = String(project.id || "");
            const projectPath = String(project.path || "");
            row.title = [projectName, projectId && projectId !== projectName ? projectId : "", projectPath].filter(Boolean).join(" · ");
            row.setAttribute("aria-label", row.title || projectName);
            const projectIcon = document.createElement("span");
            projectIcon.className = "project-row-icon";
            projectIcon.setAttribute("aria-hidden", "true");
            projectIcon.appendChild(runtimeIcon("folder"));
            const main = document.createElement("div");
            main.className = "project-row-main";
            const heading = document.createElement("div");
            heading.className = "project-row-heading";
            const title = document.createElement("div");
            title.className = "project-row-title";
            title.textContent = projectName;
            const signals = document.createElement("div");
            signals.className = "project-row-signals";
            const addSignal = (text, tone, signalTitle = text) => {
                const signal = document.createElement("span");
                signal.className = "project-row-state " + tone;
                signal.textContent = text;
                signal.title = signalTitle;
                signals.appendChild(signal);
            };
            const runningSessions = Math.max(0, Number(project.sessions?.running_sessions || 0));
            const projectAttention = pendingAttentionCount(project.sessions?.attention);
            if (!project.connected)
                addSignal(tr("OFFLINE"), "tone-fail");
            else if (project.agent_status && project.agent_status !== "online")
                addSignal(tr(String(project.agent_status).toUpperCase()), "tone-warn");
            if (runningSessions > 0) {
                addSignal(runtimeLanguage === "zh-CN" ? "运行中 " + runningSessions : runningSessions + " running", "tone-runtime", countLabel(runningSessions, "running Session"));
            }
            if (projectAttention > 0) {
                addSignal(runtimeLanguage === "zh-CN" ? "待处理 " + projectAttention : projectAttention + " attention", "tone-warn", attentionLabel(project.sessions?.attention));
            }
            heading.appendChild(title);
            heading.appendChild(signals);
            main.appendChild(heading);
            const meta = document.createElement("div");
            meta.className = "project-row-meta muted small";
            const metaParts = [];
            if (project.sessions) {
                metaParts.push(project.sessions.sessions_truncated
                    ? runtimeLanguage === "zh-CN"
                        ? String(project.sessions.returned_sessions || 0) + " / " + String(project.sessions.retained_sessions || 0) + " 个会话"
                        : String(project.sessions.returned_sessions || 0) + " / " + String(project.sessions.retained_sessions || 0) + " Sessions"
                    : countLabel(project.sessions.retained_sessions, "Session"));
                if (typeof project.sessions.latest_updated_at === "number") {
                    metaParts.push((runtimeLanguage === "zh-CN" ? "更新于 " : "updated ") + updatedLabel(project.sessions.latest_updated_at));
                }
                if (project.sessions.sessions_truncated)
                    metaParts.push(runtimeLanguage === "zh-CN" ? "扫描不完整" : "scan partial");
            }
            meta.textContent = metaParts.join(" · ");
            if (metaParts.length)
                main.appendChild(meta);
            row.appendChild(projectIcon);
            row.appendChild(main);
            const select = () => switchProject(String(project.client_id || ""), String(project.id || ""));
            row.addEventListener("click", (event) => {
                if (project.id === state.selectedProject)
                    return;
                event.preventDefault();
                try {
                    window.localStorage.setItem(disclosureKey, "open");
                }
                catch { }
                select();
            });
            workspace.appendChild(row);
            deviceProjectList.appendChild(workspace);
            if (project.id === state.selectedProject && sessionsPanel) {
                sessionsPanel.hidden = false;
                workspace.appendChild(sessionsPanel);
                sessionsAttached = true;
            }
        }
        group.appendChild(deviceProjectList);
        projectList.appendChild(group);
    }
    if (sessionsPanel && !sessionsAttached) {
        sessionsPanel.hidden = true;
        projectList.appendChild(sessionsPanel);
    }
    const returnedProjects = runtimeProjectsForDevice(effective, projectDeviceFilter).length;
    const totalProjects = Math.max(returnedProjects, projectRowsTotal);
    const scope = runtimeLanguage === "zh-CN"
        ? (projectDeviceFilter ? " · 位于 " + projectDeviceFilter : " · 跨全部设备")
        : (projectDeviceFilter ? " on " + projectDeviceFilter : " across fleet");
    const queryActive = !!projectSearch.trim();
    setText("runtime-device-status", devices.length
        ? countLabel(devices.length, "authorized Runner") + (projectDeviceFilter ? (runtimeLanguage === "zh-CN" ? " · 已筛选" : " · filtered") : " · " + tr("All Runners"))
        : (runtimeLanguage === "zh-CN" ? "没有已授权运行器" : "No authorized Runners"));
    setText("runtime-project-status", truncated
        ? runtimeLanguage === "zh-CN"
            ? "已显示 " + String(returnedProjects) + " / " + String(totalProjects) + (queryActive ? " 个匹配项目" : " 个可见项目") + scope + " · 有界"
            : String(returnedProjects) + " of " + String(totalProjects) + (queryActive ? " matching Projects shown" : " visible Projects shown") + scope + " · bounded"
        : countLabel(totalProjects, queryActive ? "matching Project" : "visible Project") + scope);
    renderSelectedProjectIdentity();
}
function switchProject(device, project) {
    const snapshot = effectiveProjects(projectRows).find((row) => String(row?.id || "") === project);
    selectedProjectSnapshot = snapshot || null;
    abortProjectWork();
    collaborationReplyTo = "";
    clearSessionSurface();
    if (device)
        revealRunner(device);
    const request = selectRuntimeProject(state, device, project);
    renderProjectSelectors(projectRows, projectRowsTruncated);
    renderRunnerFleet(runnerRows);
    renderRecentSessions(recentSessionRows, null);
    renderSelectedProjectIdentity();
    if (request)
        void fetchSessions(request);
    if (token)
        void fetchProjects(refreshRuntimeProjects(state, projectSearch, projectDeviceFilter));
}
function applyRunnerFilter(device) {
    stopProjectSearchTimer();
    abortProjectWork();
    collaborationReplyTo = "";
    clearSessionSurface();
    selectedProjectSnapshot = null;
    projectDeviceFilter = device;
    if (device)
        revealRunner(device);
    selectRuntimeRunnerFilter(state, device);
    renderProjectSelectors(projectRows, projectRowsTruncated);
    renderRunnerFleet(runnerRows);
    renderRecentSessions(recentSessionRows, null);
    renderSelectedProjectIdentity();
    if (token)
        void fetchProjects(refreshRuntimeProjects(state, projectSearch, projectDeviceFilter));
}
function runnerAttentionCount(runner) {
    const attention = runner?.sessions?.attention;
    return ["open_guidance", "open_questions", "open_risks", "open_todos"]
        .reduce((total, key) => total + (typeof attention?.[key] === "number" ? Math.max(0, attention[key]) : 0), 0);
}
function renderRunnerFleet(runners) {
    const node = el("runtime-runner-list");
    if (!node)
        return;
    const signature = renderFingerprint([runtimeLanguage, state.selectedDevice, runners]);
    if (signature === renderedRunnerFleetSignature)
        return;
    renderedRunnerFleetSignature = signature;
    clearNode(node);
    show("runtime-runners-empty", runners.length === 0 && !!el("runtime-runner-unavailable")?.hidden);
    for (const runner of runners) {
        const clientId = String(runner?.client_id || "");
        if (!clientId)
            continue;
        const row = document.createElement("button");
        row.type = "button";
        row.className = "fleet-row" + (clientId === state.selectedDevice ? " selected" : "");
        if (clientId === state.selectedDevice)
            row.setAttribute("aria-current", "true");
        const main = document.createElement("div");
        main.className = "fleet-row-main";
        const title = document.createElement("div");
        title.className = "fleet-row-title";
        title.textContent = clientId;
        const meta = document.createElement("div");
        meta.className = "muted small fleet-row-meta";
        const metaParts = [
            tr(runner.connected ? String(runner.status || "online") : "offline"),
            runner.version ? "v" + String(runner.version) : (runtimeLanguage === "zh-CN" ? "版本不可用" : "version unavailable"),
            runner.transport ? String(runner.transport) : (runtimeLanguage === "zh-CN" ? "传输方式不可用" : "transport unavailable"),
            runner.source_alignment
                ? (runtimeLanguage === "zh-CN" ? "源码 " : "source ") + tr(String(runner.source_alignment))
                : (runtimeLanguage === "zh-CN" ? "源码对齐状态不可用" : "source alignment unavailable"),
            typeof runner.last_seen_age_secs === "number"
                ? (runtimeLanguage === "zh-CN" ? String(runner.last_seen_age_secs) + " 秒前在线" : "seen " + String(runner.last_seen_age_secs) + "s ago")
                : (runtimeLanguage === "zh-CN" ? "最后在线时间不可用" : "last seen unavailable"),
        ];
        if (runner.build_git_commit)
            metaParts.push((runtimeLanguage === "zh-CN" ? "构建 " : "build ") + String(runner.build_git_commit));
        meta.textContent = metaParts.join(" · ");
        main.appendChild(title);
        main.appendChild(meta);
        const signals = document.createElement("div");
        signals.className = "fleet-row-signals";
        const working = Math.max(Number(runner.jobs_running || 0), Number(runner.sessions?.running_sessions || 0));
        const attention = runnerAttentionCount(runner);
        if (working > 0)
            appendChip(signals, tr("RUNNING"), "tone-runtime");
        if (attention > 0)
            appendChip(signals, tr("ATTENTION") + " " + attention, "tone-warn");
        if (!runner.connected)
            appendChip(signals, tr("OFFLINE"), "tone-fail");
        else if (String(runner.status || "") === "stale")
            appendChip(signals, tr("STALE"), "tone-warn");
        if (runner.source_alignment === "different")
            appendChip(signals, tr("SOURCE DIFFERENT"), "tone-fail");
        if (runner.version_matches_server === false)
            appendChip(signals, tr("BUILD DIFFERENT"), "tone-warn");
        if (runner.build_git_dirty === true)
            appendChip(signals, tr("DIRTY"), "tone-warn");
        const facts = document.createElement("div");
        facts.className = "muted small fleet-row-facts";
        const projectFact = runner.projects_scan_partial
            ? runtimeLanguage === "zh-CN" ? String(runner.projects_scanned || 0) + " 个项目已扫描" : String(runner.projects_scanned || 0) + " Projects scanned"
            : countLabel(runner.projects_scanned, "visible Project");
        const factParts = [
            countLabel(runner.active_jobs, "active Job"),
            countLabel(runner.jobs_running, "running Job"),
            countLabel(runner.jobs_queued, "queued Job"),
            typeof runner.job_concurrency_limit === "number"
                ? (runtimeLanguage === "zh-CN" ? "并发上限 " : "limit ") + runner.job_concurrency_limit
                : (runtimeLanguage === "zh-CN" ? "并发上限不可用" : "limit unavailable"),
            projectFact,
            countLabel(runner.sessions?.active_sessions, "active Session"),
        ];
        if (runner.projects_scan_partial)
            factParts.push(runtimeLanguage === "zh-CN" ? "设备群扫描不完整" : "fleet scan partial");
        if (runner.sessions?.sessions_truncated)
            factParts.push(runtimeLanguage === "zh-CN" ? "会话扫描不完整" : "Session scan partial");
        facts.textContent = factParts.join(" · ");
        row.appendChild(main);
        row.appendChild(signals);
        row.appendChild(facts);
        const select = () => applyRunnerFilter(clientId);
        row.addEventListener("click", select);
        node.appendChild(row);
    }
    setText("runtime-runner-count", countLabel(runners.length, "Runner"));
}
function renderRecentSessions(sessions, meta) {
    const node = el("runtime-recent-session-list");
    if (!node)
        return;
    const signature = renderFingerprint([
        runtimeLanguage,
        state.selectedProject,
        state.workflow.selectedSessionId,
        sessions,
        meta,
    ]);
    if (signature === renderedRecentSessionsSignature)
        return;
    renderedRecentSessionsSignature = signature;
    clearNode(node);
    show("runtime-recent-empty", sessions.length === 0 && !!el("runtime-recent-unavailable")?.hidden);
    for (const session of sessions) {
        const sessionId = String(session?.session_id || "");
        const projectId = String(session?.project_id || "");
        const clientId = String(session?.client_id || "");
        if (!sessionId || !projectId || !clientId)
            continue;
        const selected = projectId === state.selectedProject && sessionId === state.workflow.selectedSessionId;
        const row = document.createElement("button");
        row.type = "button";
        row.className = "recent-session-row" + (selected ? " selected" : "");
        if (selected)
            row.setAttribute("aria-current", "true");
        const main = document.createElement("div");
        main.className = "recent-session-main";
        const title = document.createElement("div");
        title.className = "session-title";
        title.textContent = session.title ? String(session.title) : sessionId;
        const location = document.createElement("div");
        location.className = "muted small recent-session-location";
        location.textContent = clientId + " · " + String(session.project_name || projectId) + (session.project_name && session.project_name !== projectId ? " · " + projectId : "");
        main.appendChild(title);
        main.appendChild(location);
        const signals = document.createElement("div");
        signals.className = "recent-session-signals";
        const liveness = localizedLivenessPresentation(session);
        if (liveness.state === "working")
            appendChip(signals, tr("RUNNING"), "tone-runtime");
        const attention = attentionLabel(session.overview?.attention);
        if (pendingAttentionCount(session.overview?.attention) > 0)
            appendChip(signals, attention, "tone-warn");
        const lifecycle = document.createElement("span");
        lifecycle.className = "muted small";
        lifecycle.textContent = [tr(String(session.lifecycle || "")), liveness.label, (runtimeLanguage === "zh-CN" ? "更新于 " : "updated ") + updatedLabel(session.updated_at)].filter(Boolean).join(" · ");
        lifecycle.title = liveness.tooltip;
        signals.appendChild(lifecycle);
        row.appendChild(main);
        row.appendChild(signals);
        appendPreview(row, tr("Now"), session.current_activity);
        appendPreview(row, tr("Last"), session.last_activity);
        const select = () => selectRecentSession(session);
        row.addEventListener("click", select);
        node.appendChild(row);
    }
    if (meta) {
        setText("runtime-recent-status", countLabel(meta.returned, "Session")
            + (meta.truncated ? (runtimeLanguage === "zh-CN" ? " · 前 " : " · top ") + String(meta.returned || 0) : "")
            + (meta.scan_truncated ? (runtimeLanguage === "zh-CN" ? " · 扫描不完整" : " · partial scan") : ""));
    }
}
function selectRecentSession(session) {
    const clientId = String(session?.client_id || "");
    const projectId = String(session?.project_id || "");
    const sessionId = String(session?.session_id || "");
    if (!clientId || !projectId || !sessionId)
        return;
    if (projectDeviceFilter && projectDeviceFilter !== clientId)
        projectDeviceFilter = "";
    const knownProject = effectiveProjects(projectRows).find((row) => String(row?.id || "") === projectId)
        || homeProjectRows.find((row) => String(row?.id || "") === projectId);
    selectedProjectSnapshot = knownProject || {
        id: projectId,
        client_id: clientId,
        name: typeof session?.project_name === "string" ? session.project_name : undefined,
    };
    abortProjectWork();
    collaborationReplyTo = "";
    clearSessionSurface();
    setHumanJoinSendEnabled(false);
    if (clientId)
        revealRunner(clientId);
    const location = selectRuntimeSessionLocation(state, clientId, projectId, sessionId);
    restoreCurrentDraft();
    renderProjectSelectors(projectRows, projectRowsTruncated);
    renderRunnerFleet(runnerRows);
    renderRecentSessions(recentSessionRows, null);
    renderSelectedProjectIdentity();
    revealWorkflowSessionDetail();
    if (location.sessionListRequest)
        void fetchSessions(location.sessionListRequest);
    if (location.detailRequest)
        void fetchSessionDetail(location.detailRequest);
    const collaborationRequest = runtimeCollaborationRequest(state);
    if (collaborationRequest)
        void startCollaboration(collaborationRequest);
    if (token)
        void fetchProjects(refreshRuntimeProjects(state, projectSearch, projectDeviceFilter));
    setMobileNavigationOpen(false, true);
}
async function fetchSessions(request) {
    abort(sessionsAbort);
    const controller = new AbortController();
    sessionsAbort = controller;
    const response = await api("workflow-sessions", { project: request.project, limit: 50 }, controller.signal);
    if (sessionsAbort === controller)
        sessionsAbort = null;
    if (!response || !isCurrentRuntimeSessionListRequest(state, request))
        return;
    if (response.status === 401)
        return lock("Credential rejected.");
    if (response.status === 403 || response.status === 404) {
        showError("Selected project is no longer available.");
        return;
    }
    if (!response.ok || !response.data) {
        showError("Could not refresh Workflow Sessions.");
        return;
    }
    const selected = String(state.workflow.selectedSessionId || "");
    const previousSelected = selected
        ? sessionRows.find((row) => String(row?.session_id || "") === selected)
        : null;
    sessionRows = Array.isArray(response.data.sessions) ? response.data.sessions : [];
    sessionListMetaSnapshot = {
        total: typeof response.data.total === "number" ? Math.max(sessionRows.length, response.data.total) : sessionRows.length,
        truncated: !!response.data.truncated,
    };
    renderSessionList(sessionRows, sessionListMetaSnapshot);
    showError("");
    const nextSelected = selected
        ? sessionRows.find((row) => String(row?.session_id || "") === selected)
        : null;
    if (selected && nextSelected) {
        if (!state.workflow.snapshot || runtimeWorkflowSessionSummaryChanged(previousSelected, nextSelected)) {
            const detailRequest = refreshRuntimeWorkflowSession(state);
            if (detailRequest)
                void fetchSessionDetail(detailRequest);
        }
    }
    else if (selected) {
        abortCollaboration();
        clearRuntimeWorkflowSession(state);
        hideDetail();
    }
}
function updatedLabel(timestamp) {
    if (typeof timestamp !== "number")
        return tr("time unavailable");
    return new Date(timestamp * 1000).toLocaleTimeString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en");
}
function dateTimeLabel(timestamp) {
    if (typeof timestamp !== "number")
        return tr("time unavailable");
    return new Date(timestamp * 1000).toLocaleString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en");
}
function localizedLivenessPresentation(session) {
    const presentation = workflowSessionLivenessPresentation(session);
    if (runtimeLanguage !== "zh-CN")
        return presentation;
    let label = tr(String(presentation.label || "idle"));
    if (presentation.state === "idle" && String(presentation.label || "").startsWith("idle · ")) {
        label = tr("idle") + " · " + String(presentation.label).slice("idle · ".length);
    }
    return { ...presentation, label, tooltip: tr(String(presentation.tooltip || "")) };
}
function localizedWorkflowText(value) {
    const source = String(value || "");
    if (runtimeLanguage !== "zh-CN" || !source)
        return source;
    const exact = {
        "Latest retained validation passed": "最近保留的验证已通过",
        "Latest validation passed": "最近验证已通过",
        "Latest retained validation failed": "最近保留的验证失败",
        "Latest validation failed": "最近验证失败",
        "Validation not run": "尚未运行验证",
        "Retained terminal validation evidence unavailable": "保留的最终验证证据不可用",
        "Terminal validation evidence unavailable": "最终验证证据不可用",
        "No work observations in retained events.": "保留事件中没有工作观察记录。",
        "No tool activity observed.": "尚未观察到工具活动。",
        "No retained open guidance, questions, risks, or todos.": "没有保留的开放指导、问题、风险或待办。",
        "No retained model-reported progress.": "没有保留的模型报告进度。",
    };
    let text = exact[source] || source;
    const prefixes = [
        ["Recent observed work: ", "最近观察到的工作："],
        ["Observed work: ", "已观察工作："],
        ["Retained open messages: ", "保留的开放消息："],
        ["Retained: ", "保留："],
        ["Recent ", "最近 "],
        ["latest ", "最近 "],
    ];
    for (const [english, chinese] of prefixes) {
        if (text.startsWith(english)) {
            text = chinese + text.slice(english.length);
            break;
        }
    }
    const nounMap = {
        edit: "次编辑", edits: "次编辑", validation: "次验证", validations: "次验证",
        exploration: "次探索", review: "次审查", reviews: "次审查", run: "次运行", runs: "次运行",
        risk: "个风险", risks: "个风险", todo: "个待办", todos: "个待办",
        question: "个问题", questions: "个问题", guidance: "条指导",
        test: "次测试", tests: "次测试", "unresolved failure": "个未解决失败",
        "unresolved failures": "个未解决失败", "unresolved validation failure": "个未解决的验证失败",
        "unresolved validation failures": "个未解决的验证失败",
    };
    return text.replace(/(\d+) (unresolved validation failures?|unresolved failures?|edits?|validations?|exploration|reviews?|runs?|risks?|todos?|questions?|guidance|tests?)/g, (_match, count, noun) => {
        return String(count) + " " + (nounMap[String(noun)] || noun);
    });
}
function activityKindLabel(activity) {
    const kind = String(activity && activity.kind || "Activity");
    if (activity && activity.job_handoff) {
        if (kind === "Tested")
            return runtimeLanguage === "zh-CN" ? "测试" : "Test";
        if (kind === "Ran")
            return runtimeLanguage === "zh-CN" ? "命令" : "Command";
    }
    if (kind === "Explored" && activity && typeof activity.group_count === "number")
        return (runtimeLanguage === "zh-CN" ? "探索 ×" : "Explored ×") + activity.group_count;
    if (runtimeLanguage !== "zh-CN")
        return kind;
    return { Activity: "活动", Progress: "进度", Explored: "探索", Edited: "编辑", Tested: "测试", Ran: "运行", Reviewed: "审查" }[kind] || kind;
}
function activityFacts(activity, includeTiming) {
    const facts = [];
    if (activity && typeof activity.group_count === "number") {
        if (Array.isArray(activity.group_kinds) && activity.group_kinds.length)
            facts.push(activity.group_kinds.map(String).join(" / "));
        if (Array.isArray(activity.group_tools) && activity.group_tools.length)
            facts.push(activity.group_tools.map(String).join(", "));
    }
    else if (activity && activity.tool)
        facts.push(String(activity.tool));
    if (activity && activity.kind === "Progress")
        facts.push(runtimeLanguage === "zh-CN" ? "仅供参考" : "informational");
    else if (activity && activity.job_handoff) {
        facts.push(runtimeLanguage === "zh-CN" ? "已移交" : "handed off");
        if (activity.execution_state)
            facts.push((runtimeLanguage === "zh-CN" ? "执行 " : "execution ") + tr(String(activity.execution_state)));
    }
    else if (activity && activity.state)
        facts.push(String(activity.state));
    if (activity && activity.job_id)
        facts.push("job " + String(activity.job_id));
    if (includeTiming && activity && typeof activity.started_at === "number")
        facts.push(new Date(activity.started_at * 1000).toLocaleTimeString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en"));
    return facts;
}
function activityDescription(activity) {
    if (!activity)
        return "";
    const parts = [activityKindLabel(activity), ...activityFacts(activity, false)];
    if (activity.summary && !activity.job_handoff)
        parts.push(String(activity.summary));
    return parts.join(" · ");
}
function appendPreview(parent, label, activity) {
    if (!activity)
        return;
    const row = document.createElement("div");
    row.className = "activity-preview muted small";
    const prefix = document.createElement("span");
    prefix.className = "activity-preview-label";
    prefix.textContent = label;
    const text = document.createElement("span");
    text.textContent = activityDescription(activity);
    row.appendChild(prefix);
    row.appendChild(text);
    parent.appendChild(row);
}
function renderSessionList(sessions, payload) {
    const node = el("runtime-session-list");
    if (!node)
        return;
    const total = typeof payload.total === "number" ? payload.total : sessions.length;
    const selected = String(state.workflow.selectedSessionId || "");
    const query = el("runtime-session-search")?.value || "";
    const visible = sessions.filter((session) => runtimeSearchMatches(query, [session.title, session.session_id, session.lifecycle]));
    const signature = renderFingerprint([runtimeLanguage, selected, total, !!payload.truncated, sessions, query]);
    if (signature === renderedSessionListSignature)
        return;
    renderedSessionListSignature = signature;
    clearNode(node);
    show("runtime-sessions-empty", visible.length === 0);
    setText("runtime-session-search-status", query ? visible.length + " / " + sessions.length : "");
    setText("runtime-sessions-count", total ? sessions.length + (payload.truncated ? " of " + total : "") : "0");
    for (const session of visible) {
        const id = String(session && session.session_id || "");
        if (!id)
            continue;
        const wrapper = document.createElement("li");
        const item = document.createElement("button");
        item.type = "button";
        item.className = "session-card" + (id === selected ? " selected" : "");
        if (id === selected)
            item.setAttribute("aria-current", "true");
        const icon = document.createElement("span");
        icon.className = "session-card-icon";
        icon.setAttribute("aria-hidden", "true");
        icon.appendChild(runtimeIcon("message"));
        const main = document.createElement("div");
        main.className = "session-card-main";
        const title = document.createElement("div");
        title.className = "session-title";
        title.textContent = session.title ? String(session.title) : id;
        const meta = document.createElement("div");
        meta.className = "chips session-meta";
        const lifecycle = String(session.lifecycle || "unknown");
        if (lifecycle !== "active")
            appendChip(meta, tr(lifecycle));
        const liveness = localizedLivenessPresentation(session);
        const livenessChip = appendChip(meta, liveness.label, liveness.state === "working" ? "tone-runtime" : liveness.state === "attention" ? "tone-warn" : "");
        livenessChip.title = liveness.tooltip;
        appendChip(meta, updatedLabel(session.updated_at));
        main.appendChild(title);
        main.appendChild(meta);
        item.appendChild(icon);
        item.appendChild(main);
        const select = () => selectSession(id);
        item.addEventListener("click", select);
        wrapper.appendChild(item);
        node.appendChild(wrapper);
    }
}
function selectSession(sessionId) {
    saveCurrentDraft();
    abort(detailAbort);
    detailAbort = null;
    abortCollaboration();
    hideDetail();
    setHumanJoinSendEnabled(false);
    const request = selectRuntimeWorkflowSession(state, sessionId);
    locallyAuthoredCollaborationMessageIds = new Set();
    resetCollaborationComposerUi();
    restoreCurrentDraft();
    renderSessionList(sessionRows, sessionListMetaSnapshot);
    revealWorkflowSessionDetail();
    if (request)
        void fetchSessionDetail(request);
    const collaborationRequest = runtimeCollaborationRequest(state);
    if (collaborationRequest)
        void startCollaboration(collaborationRequest);
    setMobileNavigationOpen(false, true);
}
async function fetchSessionDetail(request) {
    abort(detailAbort);
    const controller = new AbortController();
    detailAbort = controller;
    const response = await api("workflow-session", { project: request.project, session_id: request.sessionId, limit: 100 }, controller.signal);
    if (detailAbort === controller)
        detailAbort = null;
    if (!response || !isCurrentRuntimeWorkflowSessionRequest(state, request))
        return;
    if (response.status === 401)
        return lock("Credential rejected.");
    if (response.status === 404) {
        abortCollaboration();
        clearRuntimeWorkflowSession(state);
        hideDetail();
        resetCollaborationComposerUi();
        return;
    }
    if (!response.ok || !response.data) {
        showError("Could not refresh Workflow Session detail.");
        return;
    }
    if (!adoptRuntimeWorkflowSessionDetail(state, request, response.data))
        return;
    renderDetail(response.data);
}
function setTone(id, tone) {
    const node = el(id);
    if (!node)
        return;
    for (const name of ["pass", "warn", "fail", "muted"])
        node.classList.toggle("tone-card-" + name, tone === name);
}
function renderOverview(overview) {
    const view = workflowSessionOverviewPresentation(overview);
    setText("runtime-overview-work", localizedWorkflowText(view.workText));
    setText("runtime-overview-validation", localizedWorkflowText(view.validationText) + (typeof view.validationAt === "number" ? " · " + updatedLabel(view.validationAt) : ""));
    setTone("runtime-overview-validation-card", view.validationTone);
    setText("runtime-overview-attention", localizedWorkflowText(view.attentionText));
    setTone("runtime-overview-attention-card", view.attentionTone);
    setText("runtime-overview-progress", localizedWorkflowText(view.progressText) + (typeof view.progressAt === "number" ? (runtimeLanguage === "zh-CN" ? " · 报告于 " : " · reported ") + updatedLabel(view.progressAt) : ""));
}
function syncFollowUi() {
    show("runtime-jump-latest", !!state.workflow.selectedSessionId && !shouldFollowWorkflowSessionLatest(state.workflow));
}
function renderDetail(detail, consumeCollaborationNotice = true) {
    document.body.classList.add("runtime-has-session");
    show("runtime-session-detail-empty", false);
    show("runtime-session-detail", true);
    show("runtime-session-context", true);
    renderWorkspaceHeading();
    setText("runtime-session-lifecycle", tr(String(detail.lifecycle || "unknown")));
    setText("runtime-session-mode", (runtimeLanguage === "zh-CN" ? "模式 " : "mode ") + tr(String(detail.mode || "unknown")));
    setText("runtime-session-context-lifecycle", tr(String(detail.lifecycle || "unknown")));
    setText("runtime-session-context-mode", tr(String(detail.mode || "unknown")));
    const liveness = localizedLivenessPresentation(detail);
    setText("runtime-session-running", liveness.label);
    const livenessNode = el("runtime-session-running");
    if (livenessNode)
        livenessNode.title = liveness.tooltip;
    setText("runtime-session-id", String(detail.session_id || (runtimeLanguage === "zh-CN" ? "会话 ID 不可用" : "session id unavailable")));
    setText("runtime-session-created", dateTimeLabel(detail.created_at));
    setText("runtime-session-updated", dateTimeLabel(detail.updated_at));
    renderSessionWorkspaceIdentity();
    renderSessionWindowCorrelation(detail);
    renderOverview(detail.overview);
    renderCollaboration(undefined, consumeCollaborationNotice);
    syncResponsiveNavigation();
    const activities = Array.isArray(detail.activity) ? detail.activity : [];
    const node = el("runtime-timeline");
    const previousScrollTop = node ? node.scrollTop : 0;
    clearNode(node);
    show("runtime-timeline-empty", activities.length === 0);
    if (!node)
        return syncFollowUi();
    for (const activity of activities) {
        const item = document.createElement("li");
        item.className = "timeline-event";
        if (activity && activity.kind === "Progress")
            item.classList.add("reported-progress");
        if (activity && ["failed", "timed_out"].includes(String(activity.state || "")))
            item.classList.add("failed");
        const head = document.createElement("div");
        head.className = "timeline-head";
        const kind = document.createElement("span");
        kind.className = "timeline-kind";
        kind.textContent = activityKindLabel(activity);
        const meta = document.createElement("span");
        meta.className = "muted small";
        meta.textContent = activityFacts(activity, true).join(" · ");
        head.appendChild(kind);
        head.appendChild(meta);
        item.appendChild(head);
        if (activity && activity.summary) {
            const body = document.createElement("div");
            body.className = "timeline-body small";
            body.textContent = String(activity.summary);
            item.appendChild(body);
        }
        if (activity && Array.isArray(activity.paths) && activity.paths.length) {
            const paths = document.createElement("div");
            paths.className = "muted small";
            paths.textContent = activity.paths.map(String).join(" · ");
            item.appendChild(paths);
        }
        node.appendChild(item);
    }
    node.scrollTop = workflowSessionScrollTopAfterRender(state.workflow, previousScrollTop, node.clientHeight, node.scrollHeight);
    syncFollowUi();
}
function collaborationPhaseLabel() {
    switch (state.collaboration.phase) {
        case "live": return tr("Live");
        case "reconnecting": return tr("Reconnecting");
        case "paused": return tr("Paused");
        default: return tr("Idle");
    }
}
function syncCollaborationComposer() {
    const edit = runtimeCollaborationEditTarget(state);
    const unavailable = state.collaboration.available === false;
    const replyTargetId = String(state.collaboration.replyTargetId || "");
    const body = el("runtime-message-body");
    const kind = el("runtime-message-kind");
    const priority = el("runtime-message-priority");
    const checkbox = el("runtime-message-requires-ack");
    const send = el("runtime-message-send");
    if (unavailable)
        closeComposerOptions(false);
    show("runtime-message-reply", !!replyTargetId && !edit);
    setText("runtime-message-reply-text", replyTargetId ? (runtimeLanguage === "zh-CN" ? "回复 " : "Reply to ") + replyTargetId : "");
    show("runtime-message-edit", !!edit);
    setText("runtime-message-edit-text", edit ? (runtimeLanguage === "zh-CN" ? "正在编辑 " : "Editing ") + String(edit.message_id) : "");
    if (body) {
        body.disabled = unavailable;
        body.placeholder = unavailable ? tr("Conversation access requires runtime:read") : tr("Message this Session…");
    }
    if (kind) {
        kind.disabled = unavailable || !!edit;
        if (edit)
            kind.value = String(edit.kind || "note");
    }
    if (priority) {
        priority.disabled = unavailable || !!edit;
        if (edit)
            priority.value = String(edit.priority || "normal");
    }
    if (checkbox && edit)
        checkbox.checked = !!edit.requires_ack;
    if (send) {
        const actionLabel = edit ? tr("Replace message") : tr("Send message");
        send.title = actionLabel;
        send.setAttribute("aria-label", actionLabel);
        send.classList.toggle("replace-mode", !!edit);
    }
    syncAckComposer();
    syncCollaborationComposerLayout();
    if (unavailable && checkbox)
        checkbox.disabled = true;
}
function syncCollaborationComposerLayout() {
    const body = el("runtime-message-body");
    const composer = el("runtime-collaboration-form");
    const send = el("runtime-message-send");
    const hasContent = !!body?.value.trim();
    composer?.classList.toggle("has-content", hasContent);
    send?.classList.toggle("is-ready", hasContent);
    if (!body)
        return;
    body.style.height = "0px";
    const nextHeight = Math.min(Math.max(body.scrollHeight, 44), 180);
    body.style.height = nextHeight + "px";
    body.style.overflowY = body.scrollHeight > 180 ? "auto" : "hidden";
}
function scrollCollaborationToLatest(smooth) {
    const scroll = el("runtime-chat-scroll");
    if (!scroll)
        return;
    collaborationFollowLatest = true;
    collaborationPendingMessages = 0;
    syncNewMessageIndicator();
    const behavior = smooth && !window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "smooth" : "auto";
    window.requestAnimationFrame(() => {
        scroll.scrollTo({ top: scroll.scrollHeight, behavior });
    });
}
function syncComposerOptionSummary() {
    const kind = el("runtime-message-kind");
    const priority = el("runtime-message-priority");
    const checkbox = el("runtime-message-requires-ack");
    const options = el("runtime-message-options");
    const signals = [];
    if (kind?.value && kind.value !== "note")
        signals.push(tr(kind.value));
    if (priority?.value && priority.value !== "normal")
        signals.push(tr(priority.value));
    if (checkbox?.checked)
        signals.push(runtimeLanguage === "zh-CN" ? "需确认" : "ACK");
    setText("runtime-message-options-label", signals.length ? signals.join(" · ") : tr("Options"));
    options?.classList.toggle("has-selection", signals.length > 0);
}
function setCollaborationReplyTarget(messageId) {
    collaborationReplyTo = messageId;
    const wasEditing = !!runtimeCollaborationEditTarget(state);
    setRuntimeCollaborationReplyTarget(state, messageId);
    const body = el("runtime-message-body");
    if (wasEditing && body)
        body.value = "";
    syncCollaborationComposer();
    if (messageId) {
        setText("runtime-message-send-status", runtimeLanguage === "zh-CN"
            ? "已选择回复目标。下一条消息将回复 " + messageId + "。"
            : "Reply target selected. Your next message will reply to " + messageId + ".");
        body?.focus();
    }
    else {
        setText("runtime-message-send-status", tr("Reply target cleared."));
    }
}
function beginCollaborationEdit(message) {
    saveCurrentDraft();
    if (!setRuntimeCollaborationEditTarget(state, String(message?.message_id || "")))
        return;
    collaborationReplyTo = "";
    const body = el("runtime-message-body");
    if (body) {
        body.value = String(message?.message || "");
        body.focus();
    }
    setText("runtime-message-send-status", "");
    syncCollaborationComposer();
}
function cancelCollaborationEdit() {
    clearRuntimeCollaborationEditTarget(state);
    restoreCurrentDraft();
    setText("runtime-message-send-status", tr("Edit cancelled."));
    syncCollaborationComposer();
}
function resetCollaborationComposerUi() {
    const body = el("runtime-message-body");
    if (body)
        body.value = "";
    closeComposerOptions(false);
    syncCollaborationComposer();
}
function runtimeSearchMatches(query, values) {
    const text = values.filter((value) => typeof value === "string").join(" ").toLocaleLowerCase();
    return query.trim().toLocaleLowerCase().split(/\s+/).every((term) => text.includes(term));
}
function filterCollaborationMessages() {
    const query = el("runtime-message-search")?.value || "";
    let matches = 0;
    const cards = document.querySelectorAll("#runtime-collaboration-board .message-card");
    for (const card of Array.from(cards)) {
        const message = state.collaboration.messages.find((entry) => entry.message_id === card.dataset.messageId);
        const visible = runtimeSearchMatches(query, [message?.message, message?.resolution, message?.message_id, message?.author_session_id]);
        card.hidden = !visible;
        if (visible)
            matches++;
    }
    document.querySelectorAll("#runtime-collaboration-board .message-date-separator").forEach((node) => { node.hidden = !!query.trim(); });
    setText("runtime-message-search-status", query.trim() ? matches + " / " + cards.length : "");
}
function renderCollaboration(statusText, consumeMutationNotice = true) {
    const mutationNotice = consumeMutationNotice ? takeRuntimeCollaborationMutationNotice(state) : "";
    if (mutationNotice) {
        const editStillActive = !!runtimeCollaborationEditTarget(state);
        if (mutationNotice.includes("changed while editing")
            || mutationNotice.includes("Replacement confirmed")
            || mutationNotice.includes("Withdraw confirmed")
            || (mutationNotice.includes("Outcome not observed") && !editStillActive)) {
            const body = el("runtime-message-body");
            if (body)
                body.value = "";
        }
        const localizedMutationNotice = tr(mutationNotice);
        setText("runtime-message-send-status", localizedMutationNotice);
        statusText = [statusText ? tr(statusText) : "", localizedMutationNotice].filter(Boolean).join(" · ");
    }
    const available = state.collaboration.available !== false;
    if (!available) {
        const body = el("runtime-message-body");
        if (body)
            body.value = "";
    }
    show("runtime-collaboration-unavailable", !available);
    show("runtime-collaboration-form", true);
    el("runtime-collaboration-form")?.classList.toggle("is-unavailable", !available);
    const messages = available && Array.isArray(state.collaboration.messages) ? state.collaboration.messages : [];
    const scroll = el("runtime-chat-scroll");
    const previousScrollTop = scroll?.scrollTop || 0;
    const shouldFollowNewMessages = collaborationFollowLatest || chatIsNearLatest();
    const previouslyRenderedMessageIds = renderedCollaborationMessageIds;
    const nextRenderedMessageIds = new Set(messages.map((message) => String(message?.message_id || "")).filter(Boolean));
    const newMessageIds = Array.from(nextRenderedMessageIds).filter((id) => !previouslyRenderedMessageIds.has(id));
    const hasNewMessages = newMessageIds.length > 0;
    const firstRetainedRender = previouslyRenderedMessageIds.size === 0 && nextRenderedMessageIds.size > 0;
    show("runtime-collaboration-board", messages.length > 0);
    show("runtime-collaboration-empty", messages.length === 0);
    setText("runtime-collaboration-empty-title", available ? tr("Start this Session conversation") : tr("Conversation access unavailable"));
    setText("runtime-collaboration-empty-copy", available
        ? tr("Messages posted here are retained on the Session collaboration board.")
        : tr("This credential can inspect the Project and Session, but retained messages require runtime:read."));
    const localizedStatusText = statusText ? tr(statusText) : "";
    const status = available
        ? (runtimeLanguage === "zh-CN" ? "协作：" : "Collaboration: ") + collaborationPhaseLabel() + " · " + countLabel(messages.length, "retained message") + (localizedStatusText ? " · " + localizedStatusText : "")
        : (runtimeLanguage === "zh-CN" ? "runtime:read 不可用" : "runtime:read unavailable");
    setText("runtime-collaboration-status", status);
    const node = el("runtime-collaboration-board");
    syncCollaborationComposer();
    if (!available)
        setHumanJoinSendEnabled(false);
    const signature = renderFingerprint({
        language: runtimeLanguage,
        sessionId: state.collaboration.sessionId,
        available,
        phase: state.collaboration.phase,
        uncertainMutation: state.collaboration.uncertainMutation,
        locallyAuthored: Array.from(locallyAuthoredCollaborationMessageIds).sort(),
        messages,
    });
    if (!node) {
        renderedCollaborationMessageIds = nextRenderedMessageIds;
        return;
    }
    if (signature === renderedCollaborationSignature) {
        renderedCollaborationMessageIds = nextRenderedMessageIds;
        return;
    }
    const latestAgent = el("runtime-latest-agent-message");
    if (latestAgent) {
        const sides = runtimeCollaborationMessageSides(messages, locallyAuthoredCollaborationMessageIds);
        const latest = [...messages].reverse().find((message) => sides.get(String(message.message_id)) === "incoming" && !message.superseded_by_message_id && message.closure_kind !== "withdrawn");
        clearNode(latestAgent);
        if (latest) {
            appendRichMessage(latestAgent, latest.message);
            const time = document.createElement("p");
            time.className = "muted small";
            time.textContent = updatedLabel(latest.created_at);
            latestAgent.appendChild(time);
        }
        else
            latestAgent.textContent = runtimeLanguage === "zh-CN"
                ? "当前保留范围内暂无 Agent 留言。ACK 不包含回复正文；下方可查看模型报告的进度。"
                : "No Agent message in the retained window. ACK contains no reply text; model-reported progress appears below.";
    }
    renderedCollaborationSignature = signature;
    clearNode(node);
    if (!available) {
        renderedCollaborationMessageIds = nextRenderedMessageIds;
        return;
    }
    const byId = new Map();
    const children = new Map();
    for (const message of messages) {
        const id = String(message?.message_id || "");
        if (id)
            byId.set(id, message);
    }
    for (const message of messages) {
        const parent = typeof message?.reply_to === "string" ? message.reply_to : "";
        if (parent && byId.has(parent)) {
            const list = children.get(parent) || [];
            list.push(message);
            children.set(parent, list);
        }
    }
    const messageSides = runtimeCollaborationMessageSides(messages, locallyAuthoredCollaborationMessageIds);
    const visited = new Set();
    let previousRenderedSide = "";
    let previousRenderedDay = "";
    const appendMessage = (message, depth, parentUnavailable) => {
        const id = String(message?.message_id || "");
        if (!id || visited.has(id))
            return;
        visited.add(id);
        const card = document.createElement("article");
        card.dataset.messageId = id;
        card.className = "message-card " + String(message?.kind || "note") + (String(message?.status || "") === "resolved" ? " resolved" : "") + (parentUnavailable ? " retained-reply" : "");
        const messageSide = messageSides.get(id) || "neutral";
        card.classList.add(messageSide === "incoming" ? "agent-authored" : messageSide === "outgoing" ? "human-authored" : "provenance-unknown");
        const createdAt = typeof message?.created_at === "number" ? message.created_at : 0;
        const createdDate = createdAt ? new Date(createdAt * 1000) : null;
        const dayKey = createdDate ? [createdDate.getFullYear(), createdDate.getMonth(), createdDate.getDate()].join("-") : "";
        if (dayKey && dayKey !== previousRenderedDay) {
            const separator = document.createElement("div");
            separator.className = "message-date-separator";
            const label = document.createElement("span");
            label.textContent = createdDate?.toLocaleDateString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en", { month: "short", day: "numeric", year: "numeric" }) || "";
            separator.appendChild(label);
            node.appendChild(separator);
            previousRenderedDay = dayKey;
            previousRenderedSide = "";
        }
        card.classList.add(messageSide === "incoming" ? "message-incoming" : messageSide === "outgoing" ? "message-outgoing" : "message-neutral");
        if (!previouslyRenderedMessageIds.has(id))
            card.classList.add("message-entering");
        if (previousRenderedSide === messageSide)
            card.classList.add("message-group-continuation");
        previousRenderedSide = messageSide;
        if (depth > 0)
            card.classList.add("message-thread");
        const content = document.createElement("div");
        content.className = "message-content";
        const author = document.createElement("div");
        author.className = "message-author";
        const authorName = document.createElement("span");
        authorName.className = "message-author-name";
        authorName.textContent = messageSide === "incoming" ? tr("Agent") : messageSide === "outgoing" ? tr("You") : tr("Retained message");
        if (message?.author_session_id)
            authorName.title = String(message.author_session_id);
        else if (messageSide === "neutral")
            authorName.title = tr("Author provenance unavailable");
        author.appendChild(authorName);
        content.appendChild(author);
        if (message?.reply_to) {
            const replyContext = document.createElement("div");
            replyContext.className = "message-reply-context";
            replyContext.appendChild(runtimeIcon("reply"));
            const replyText = document.createElement("span");
            const parent = byId.get(String(message.reply_to));
            const preview = parent?.message ? String(parent.message).replace(/\s+/g, " ").trim().slice(0, 120) : tr("Original message unavailable");
            replyText.textContent = tr("Replying to") + " · " + preview;
            replyContext.appendChild(replyText);
            content.appendChild(replyContext);
        }
        const footer = document.createElement("div");
        footer.className = "message-footer";
        const head = document.createElement("div");
        head.className = "message-head";
        const kindValue = String(message?.kind || "note");
        const priorityValue = String(message?.priority || "normal");
        const statusValue = String(message?.status || "open");
        const messageSignals = [];
        if (kindValue !== "note")
            messageSignals.push(tr(kindValue));
        if (priorityValue !== "normal")
            messageSignals.push(tr(priorityValue));
        if (statusValue && statusValue !== "open" && statusValue !== "resolved")
            messageSignals.push(tr(statusValue));
        if (messageSignals.length) {
            const kind = document.createElement("span");
            kind.className = "message-kind";
            kind.textContent = messageSignals.join(" · ");
            head.appendChild(kind);
        }
        const time = document.createElement("span");
        time.className = "muted small";
        time.textContent = updatedLabel(message?.created_at);
        head.appendChild(time);
        footer.appendChild(head);
        const meta = document.createElement("div");
        meta.className = "message-meta";
        const metaParts = [id];
        if (message?.author_session_id)
            metaParts.push((runtimeLanguage === "zh-CN" ? "作者 " : "author ") + String(message.author_session_id));
        if (parentUnavailable)
            metaParts.push(runtimeLanguage === "zh-CN" ? "保留的回复 · 上级消息不可用" : "retained reply · parent unavailable");
        else if (message?.reply_to)
            metaParts.push((runtimeLanguage === "zh-CN" ? "回复 " : "reply to ") + String(message.reply_to));
        if (message?.superseded_by_message_id) {
            const replacementId = String(message.superseded_by_message_id);
            metaParts.push(byId.has(replacementId)
                ? "superseded by " + replacementId
                : "superseded by " + replacementId + " · replacement unavailable / retained link only");
        }
        if (message?.supersedes_message_id) {
            const originalId = String(message.supersedes_message_id);
            metaParts.push(byId.has(originalId)
                ? "replaces " + originalId
                : "replaces " + originalId + " · retained link only");
        }
        meta.textContent = metaParts.join(" · ");
        footer.appendChild(meta);
        footer.title = meta.textContent;
        const bubble = document.createElement("div");
        bubble.className = "message-bubble";
        appendRichMessage(bubble, message?.message);
        content.appendChild(bubble);
        if (message?.requires_ack) {
            const ack = document.createElement("div");
            ack.className = "message-ack";
            const acknowledged = typeof message?.first_ack_observed_at === "number";
            ack.classList.toggle("observed", acknowledged);
            ack.textContent = acknowledged
                ? (runtimeLanguage === "zh-CN" ? "已观察到 ACK（不代表回复或完成）" : "ACK observed (not a reply or completion)") + " · " + updatedLabel(message.first_ack_observed_at)
                : tr("Acknowledgement required");
            ack.title = acknowledged
                ? "ACK required · First ACK observed " + updatedLabel(message.first_ack_observed_at)
                : "ACK required";
            footer.appendChild(ack);
        }
        if (message?.resolved_at || message?.resolution || message?.resolved_by_message_id || message?.closure_kind) {
            const resolution = document.createElement("div");
            resolution.className = "message-resolution";
            const parts = [];
            if (message?.closure_kind === "withdrawn")
                parts.push("withdrawn" + (message.resolved_at ? " " + updatedLabel(message.resolved_at) : ""));
            else if (message?.closure_kind === "superseded")
                parts.push("superseded" + (message.resolved_at ? " " + updatedLabel(message.resolved_at) : ""));
            else if (message.resolved_at)
                parts.push("resolved " + updatedLabel(message.resolved_at));
            if (message.resolution)
                parts.push(String(message.resolution));
            if (message.resolved_by_message_id)
                parts.push("by " + String(message.resolved_by_message_id));
            const resolutionLabel = message?.closure_kind === "withdrawn"
                ? tr("Withdrawn")
                : message?.closure_kind === "superseded"
                    ? tr("Replaced")
                    : tr("Resolved");
            resolution.textContent = resolutionLabel + (message.resolved_at ? " · " + updatedLabel(message.resolved_at) : "");
            resolution.title = parts.join(" · ");
            footer.appendChild(resolution);
            if (message.resolution) {
                const explanation = document.createElement("section");
                explanation.className = "message-resolution-body";
                const label = document.createElement("strong");
                label.textContent = runtimeLanguage === "zh-CN" ? "处理说明" : "Resolution";
                explanation.appendChild(label);
                appendRichMessage(explanation, message.resolution);
                content.appendChild(explanation);
            }
        }
        const actions = document.createElement("div");
        actions.className = "message-actions";
        actions.appendChild(createMessageAction(tr("Reply"), "reply", () => setCollaborationReplyTarget(id)));
        if (runtimeCollaborationMessageCanMutate(message) && state.collaboration.phase === "live" && !state.collaboration.uncertainMutation) {
            const editLabel = runtimeLanguage === "zh-CN" ? "替换这条保留消息，同时保留其历史记录。" : "Replace this retained message while preserving its history.";
            const deleteLabel = runtimeLanguage === "zh-CN" ? "撤回这条保留消息；历史记录仍会保留。" : "Withdraw this retained message; history is preserved.";
            actions.appendChild(createMessageAction(editLabel, "edit", () => beginCollaborationEdit(message)));
            actions.appendChild(createMessageAction(deleteLabel, "trash", () => void withdrawHumanCollaborationMessage(id), true));
        }
        footer.appendChild(actions);
        content.appendChild(footer);
        card.appendChild(content);
        node.appendChild(card);
        for (const child of children.get(id) || [])
            appendMessage(child, depth + 1, false);
    };
    for (const message of messages) {
        const parent = typeof message?.reply_to === "string" ? message.reply_to : "";
        if (!parent || !byId.has(parent))
            appendMessage(message, 0, !!parent);
    }
    for (const message of messages)
        appendMessage(message, 0, false);
    renderedCollaborationMessageIds = nextRenderedMessageIds;
    filterCollaborationMessages();
    if (hasNewMessages && !firstRetainedRender)
        announceNewCollaborationMessages(newMessageIds.length);
    if (firstRetainedRender || (hasNewMessages && shouldFollowNewMessages)) {
        scrollCollaborationToLatest(!firstRetainedRender);
    }
    else {
        window.requestAnimationFrame(() => {
            if (scroll)
                scroll.scrollTop = previousScrollTop;
        });
        if (hasNewMessages) {
            collaborationFollowLatest = false;
            collaborationPendingMessages += newMessageIds.length;
            syncNewMessageIndicator();
        }
    }
}
async function confirmCollaborationMutationDurability(request, mutation, controller) {
    const replacing = mutation?.kind === "replace";
    const payload = {
        project: request.project,
        session_id: request.sessionId,
        message_id: String(mutation?.messageId || ""),
    };
    if (replacing)
        payload.message = String(mutation?.message || "");
    setText("runtime-message-send-status", tr(replacing
        ? "Confirming replacement durability…"
        : "Confirming withdrawal durability…"));
    const response = await api(replacing ? "workflow-session-replace-message" : "workflow-session-withdraw-message", payload, controller.signal);
    if (!response || !isCurrentRuntimeCollaborationRequest(state, request))
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 0 || response.status === 503) {
        markRuntimeCollaborationMutationUncertain(state, request, mutation);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("durability confirmation still uncertain · refresh before retry");
        return false;
    }
    if (response.status === 403) {
        setRuntimeCollaborationAvailable(state, request, false);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration();
        return false;
    }
    if (response.status === 404 || response.status === 409) {
        markRuntimeCollaborationMutationUncertain(state, request, mutation);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("message changed during durability confirmation · refresh retained state");
        return false;
    }
    const valid = replacing
        ? response.ok && response.data?.original && response.data?.replacement
        : response.ok && response.data?.message;
    if (!valid) {
        markRuntimeCollaborationMutationUncertain(state, request, mutation);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("durability confirmation failed · refresh before retry");
        return false;
    }
    if (replacing) {
        adoptRuntimeCollaborationObservation(state, request, {
            messages: [response.data.original, response.data.replacement],
        });
    }
    else {
        adoptRuntimeCollaborationObservation(state, request, { messages: [response.data.message] });
    }
    completeRuntimeCollaborationMutationRecovery(state, request, replacing
        ? "Replacement durably confirmed after exact replay."
        : "Withdraw durably confirmed after exact replay.");
    return true;
}
async function loadRetainedCollaboration(request, controller) {
    // Establish the cursor before the retained snapshot. A mutation between these
    // two reads is then present in the snapshot, the subsequent delta, or both;
    // merge-by-id makes the overlap harmless. Listing first and baselining second
    // would permanently skip a mutation that lands in that gap.
    setRuntimeCollaborationPhase(state, request, "reconnecting");
    renderCollaboration("establishing retained baseline");
    const baseline = await api("workflow-session-observe", { project: request.project, session_id: request.sessionId, limit: 100 }, controller.signal);
    if (!baseline || !isCurrentRuntimeCollaborationRequest(state, request))
        return null;
    if (baseline.status === 401) {
        lock("Credential rejected.");
        return null;
    }
    if (baseline.status === 403) {
        setRuntimeCollaborationAvailable(state, request, false);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration();
        return null;
    }
    if (baseline.status === 404) {
        setRuntimeCollaborationAvailable(state, request, false);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("Session unavailable");
        return null;
    }
    if (!baseline.ok || !baseline.data || typeof baseline.data.observation_token !== "string") {
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("observation unavailable");
        return null;
    }
    const response = await api("workflow-session-messages", { project: request.project, session_id: request.sessionId, limit: 100 }, controller.signal);
    if (!response || !isCurrentRuntimeCollaborationRequest(state, request))
        return null;
    if (response.status === 401) {
        lock("Credential rejected.");
        return null;
    }
    if (response.status === 403) {
        setRuntimeCollaborationAvailable(state, request, false);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration();
        return null;
    }
    if (response.status === 404) {
        setRuntimeCollaborationAvailable(state, request, false);
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("Session unavailable");
        return null;
    }
    if (!response.ok || !response.data) {
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("retained snapshot failed");
        return null;
    }
    setRuntimeCollaborationAvailable(state, request, true);
    if (!adoptRuntimeCollaborationList(state, request, Array.isArray(response.data.messages) ? response.data.messages : []))
        return null;
    adoptRuntimeCollaborationObservation(state, request, baseline.data);
    const mutationRecovery = runtimeCollaborationMutationRecovery(state, request);
    if (mutationRecovery && !(await confirmCollaborationMutationDurability(request, mutationRecovery, controller)))
        return null;
    setRuntimeCollaborationPhase(state, request, "live");
    setHumanJoinSendEnabled(true);
    renderCollaboration("bounded long-poll");
    return baseline.data.observation_token;
}
async function startCollaboration(request) {
    abortCollaboration();
    const controller = new AbortController();
    collaborationAbort = controller;
    let observationToken = await loadRetainedCollaboration(request, controller);
    while (observationToken && collaborationAbort === controller && isCurrentRuntimeCollaborationRequest(state, request)) {
        const response = await api("workflow-session-observe", {
            project: request.project,
            session_id: request.sessionId,
            after_observation_token: observationToken,
            wait_secs: COLLABORATION_WAIT_SECS,
            limit: 100,
        }, controller.signal);
        if (!response || collaborationAbort !== controller || !isCurrentRuntimeCollaborationRequest(state, request))
            break;
        if (response.status === 401) {
            lock("Credential rejected.");
            break;
        }
        if (response.status === 403) {
            setRuntimeCollaborationAvailable(state, request, false);
            setRuntimeCollaborationPhase(state, request, "paused");
            renderCollaboration();
            break;
        }
        if (!response.ok || !response.data) {
            setRuntimeCollaborationPhase(state, request, "paused");
            renderCollaboration("request failed");
            break;
        }
        const action = runtimeCollaborationObservationAction(response.data);
        if (action === "reload") {
            renderCollaboration("retention changed · reloading");
            observationToken = await loadRetainedCollaboration(request, controller);
            continue;
        }
        if (!adoptRuntimeCollaborationObservation(state, request, response.data))
            break;
        observationToken = String(response.data.observation_token || observationToken);
        setRuntimeCollaborationPhase(state, request, "live");
        renderCollaboration(action === "drain" ? "draining retained changes" : "bounded long-poll");
        if (action === "drain") {
            let draining = true;
            while (draining && observationToken && collaborationAbort === controller && isCurrentRuntimeCollaborationRequest(state, request)) {
                const drain = await api("workflow-session-observe", {
                    project: request.project,
                    session_id: request.sessionId,
                    after_observation_token: observationToken,
                    limit: 100,
                }, controller.signal);
                if (!drain || collaborationAbort !== controller || !isCurrentRuntimeCollaborationRequest(state, request))
                    break;
                if (!drain.ok || !drain.data) {
                    setRuntimeCollaborationPhase(state, request, "paused");
                    renderCollaboration("delta drain failed");
                    observationToken = null;
                    break;
                }
                if (runtimeCollaborationObservationAction(drain.data) === "reload") {
                    observationToken = await loadRetainedCollaboration(request, controller);
                    draining = false;
                    continue;
                }
                adoptRuntimeCollaborationObservation(state, request, drain.data);
                observationToken = String(drain.data.observation_token || observationToken);
                draining = !!drain.data.has_more;
                setRuntimeCollaborationPhase(state, request, "live");
                renderCollaboration(draining ? "draining retained changes" : "bounded long-poll");
            }
        }
    }
    if (collaborationAbort === controller)
        collaborationAbort = null;
}
function jumpLatest() {
    jumpWorkflowSessionToLatest(state.workflow);
    const node = el("runtime-timeline");
    if (node)
        node.scrollTop = node.scrollHeight;
    syncFollowUi();
}
function sessionCollaborationAuthorityFailure(response) {
    if (response?.status !== 403)
        return null;
    return "Session collaboration access required. This credential can still read the Session; add session:collaborate to send, edit, or withdraw messages.";
}
function setHumanJoinSendEnabled(enabled) {
    const send = el("runtime-message-send");
    if (send)
        send.disabled = !enabled;
}
function syncAckComposer() {
    const kind = el("runtime-message-kind");
    const priority = el("runtime-message-priority");
    const checkbox = el("runtime-message-requires-ack");
    const edit = runtimeCollaborationEditTarget(state);
    const guidance = edit ? edit.kind === "guidance" : kind?.value === "guidance";
    show("runtime-message-ack-label", guidance);
    if (!checkbox) {
        syncComposerOptionSummary();
        return;
    }
    if (edit) {
        checkbox.disabled = true;
        checkbox.checked = !!edit.requires_ack;
        checkbox.title = "Inherited from the original retained message.";
        syncComposerOptionSummary();
        return;
    }
    checkbox.disabled = !guidance || priority?.value !== "high";
    if (checkbox.disabled)
        checkbox.checked = false;
    checkbox.title = guidance && priority?.value !== "high" ? "ACK requirement is available for High priority guidance." : "";
    syncComposerOptionSummary();
}
async function withdrawHumanCollaborationMessage(messageId) {
    const request = runtimeCollaborationRequest(state);
    if (!request || state.collaboration.available === false)
        return;
    setText("runtime-message-send-status", tr("Withdrawing retained message…"));
    const response = await api("workflow-session-withdraw-message", {
        project: request.project,
        session_id: request.sessionId,
        message_id: messageId,
    });
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return;
    if (response?.status === 0 || response?.status === 503) {
        markRuntimeCollaborationMutationUncertain(state, request, { kind: "withdraw", messageId });
        abortCollaboration();
        setRuntimeCollaborationPhase(state, request, "paused");
        renderCollaboration("withdraw outcome unknown · refresh before retry");
        return;
    }
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    const authorityFailure = sessionCollaborationAuthorityFailure(response);
    if (authorityFailure) {
        setText("runtime-message-send-status", authorityFailure);
        return;
    }
    if (response?.status === 409) {
        abortCollaboration();
        setRuntimeCollaborationPhase(state, request, "paused");
        setText("runtime-message-send-status", tr("Message changed before Delete. Refresh retained messages before retrying."));
        renderCollaboration("message changed · refresh retained state");
        return;
    }
    if (!response?.ok || !response.data?.message) {
        setText("runtime-message-send-status", tr("Delete failed."));
        return;
    }
    if (String(state.collaboration.editTargetId || "") === messageId) {
        clearRuntimeCollaborationEditTarget(state);
        restoreCurrentDraft();
    }
    adoptRuntimeCollaborationObservation(state, request, { messages: [response.data.message] });
    setText("runtime-message-send-status", tr("Retained message withdrawn."));
    renderCollaboration();
}
async function postHumanCollaborationMessage(event) {
    event.preventDefault();
    const request = runtimeCollaborationRequest(state);
    if (!request || state.collaboration.available === false)
        return;
    const kind = el("runtime-message-kind");
    const priority = el("runtime-message-priority");
    const body = el("runtime-message-body");
    const checkbox = el("runtime-message-requires-ack");
    const send = el("runtime-message-send");
    const message = body?.value.trim() || "";
    if (!message) {
        setText("runtime-message-send-status", tr("Enter a message."));
        return;
    }
    closeComposerOptions(false);
    const editTarget = runtimeCollaborationEditTarget(state);
    if (editTarget) {
        if (send)
            send.disabled = true;
        setText("runtime-message-send-status", tr("Replacing retained message…"));
        const response = await api("workflow-session-replace-message", {
            project: request.project,
            session_id: request.sessionId,
            message_id: editTarget.message_id,
            message,
        });
        if (!isCurrentRuntimeCollaborationRequest(state, request))
            return;
        if (response?.status === 0 || response?.status === 503) {
            markRuntimeCollaborationMutationUncertain(state, request, {
                kind: "replace",
                messageId: String(editTarget.message_id),
                message,
            });
            abortCollaboration();
            setRuntimeCollaborationPhase(state, request, "paused");
            renderCollaboration("replace outcome unknown · refresh before retry");
            return;
        }
        if (send)
            send.disabled = false;
        if (response?.status === 401) {
            lock("Credential rejected.");
            return;
        }
        const authorityFailure = sessionCollaborationAuthorityFailure(response);
        if (authorityFailure) {
            setText("runtime-message-send-status", authorityFailure);
            return;
        }
        if (response?.status === 409) {
            clearRuntimeCollaborationEditTarget(state);
            restoreCurrentDraft();
            abortCollaboration();
            setRuntimeCollaborationPhase(state, request, "paused");
            setText("runtime-message-send-status", tr("Message changed before Replace. Refresh retained messages before retrying."));
            renderCollaboration("message changed · refresh retained state");
            return;
        }
        if (!response?.ok || !response.data?.original || !response.data?.replacement) {
            setText("runtime-message-send-status", tr("Replace failed."));
            return;
        }
        clearRuntimeCollaborationEditTarget(state);
        rememberLocalCollaborationMessage(response.data.replacement?.message_id);
        adoptRuntimeCollaborationObservation(state, request, {
            messages: [response.data.original, response.data.replacement],
        });
        restoreCurrentDraft();
        setText("runtime-message-send-status", tr(response.data.replayed ? "Replacement already retained." : "Message replaced."));
        renderCollaboration();
        return;
    }
    if (send)
        send.disabled = true;
    setText("runtime-message-send-status", tr("Sending…"));
    const response = await api("workflow-session-post-message", {
        project: request.project,
        session_id: request.sessionId,
        kind: kind?.value || "note",
        priority: priority?.value || "normal",
        message,
        reply_to: state.collaboration.replyTargetId || null,
        requires_ack: !!checkbox?.checked,
    });
    if (!isCurrentRuntimeCollaborationRequest(state, request))
        return;
    if (response?.status === 0) {
        abortCollaboration();
        setRuntimeCollaborationPhase(state, request, "paused");
        setText("runtime-message-send-status", tr("Send outcome unknown. Refresh and review retained messages before retrying."));
        renderCollaboration("send outcome unknown · refresh before retry");
        return;
    }
    if (send)
        send.disabled = false;
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    const authorityFailure = sessionCollaborationAuthorityFailure(response);
    if (authorityFailure) {
        setText("runtime-message-send-status", authorityFailure);
        return;
    }
    if (!response?.ok || !response.data) {
        setText("runtime-message-send-status", tr("Send failed."));
        return;
    }
    rememberLocalCollaborationMessage(response.data?.message_id);
    adoptRuntimeCollaborationObservation(state, request, { messages: [response.data] });
    if (body)
        body.value = "";
    clearCurrentDraft();
    setCollaborationReplyTarget("");
    setText("runtime-message-send-status", tr("Sent."));
    renderCollaboration();
}
function operationKey(prefix) {
    const random = typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
        ? crypto.randomUUID()
        : Date.now().toString(36) + "-" + Math.random().toString(36).slice(2);
    return prefix + "-" + random;
}
function communicationTimeLabel(value) {
    if (typeof value !== "number" || !Number.isFinite(value))
        return tr("time unavailable");
    return new Date(value).toLocaleString(runtimeLanguage === "zh-CN" ? "zh-CN" : "en");
}
function parseAgentIds(value) {
    const ids = value
        .split(/[\s,]+/)
        .map((item) => item.trim())
        .filter(Boolean);
    return Array.from(new Set(ids));
}
function communicationAgent(agentId) {
    return communicationAgents.find((agent) => String(agent?.agent_id || "") === agentId) || null;
}
function selectedCommunicationAgent() {
    return communicationAgent(selectedCommunicationAgentId);
}
function selectedCommunicationConversation() {
    return communicationConversations.find((conversation) => String(conversation?.conversation_id || "") === selectedCommunicationConversationId) || null;
}
function communicationEndpoint(agentId = selectedCommunicationAgentId) {
    return communicationEndpoints.get(agentId) || null;
}
function communicationEndpointId(agentId = selectedCommunicationAgentId) {
    return communicationEndpoint(agentId)?.endpoint_id || "";
}
function idempotencyKeyFor(pending, fingerprint, prefix) {
    return pending && pending.fingerprint === fingerprint
        ? pending
        : { fingerprint, key: operationKey(prefix) };
}
function resetCommunicationSurface() {
    communicationGeneration += 1;
    renderedCommunicationSurfaceSignature = "";
    communicationAgents = [];
    communicationConversations = [];
    communicationDetail = null;
    communicationInbox = [];
    selectedCommunicationAgentId = "";
    selectedCommunicationConversationId = "";
    communicationReadAvailable = null;
    communicationManageAvailable = null;
    communicationRefreshCoordinator.reset();
    communicationEndpoints.clear();
    pendingEndpointAttach.clear();
    pendingAgentCreate = null;
    pendingConversationCreate = null;
    pendingConversationMessage = null;
    const agentUpdateForm = el("runtime-agent-update-form");
    if (agentUpdateForm) {
        delete agentUpdateForm.dataset.agentId;
        delete agentUpdateForm.dataset.profileRevision;
    }
    clearNode(el("runtime-agent-list"));
    clearNode(el("runtime-conversation-list"));
    clearNode(el("runtime-conversation-transcript"));
    clearNode(el("runtime-conversation-participants"));
    clearNode(el("runtime-inbox-list"));
    renderCommunicationSurface();
}
function detachCommunicationEndpointsBestEffort() {
    if (!token || communicationEndpoints.size === 0)
        return;
    const credential = token;
    for (const endpoint of communicationEndpoints.values()) {
        void fetch(API_BASE + "communication/endpoint/detach", {
            method: "POST",
            headers: { Authorization: "Bearer " + credential, "Content-Type": "application/json" },
            body: JSON.stringify({ endpoint_id: endpoint.endpoint_id }),
            keepalive: true,
        }).catch(() => undefined);
    }
    communicationEndpoints.clear();
}
function renderCommunicationAvailability() {
    const available = communicationReadAvailable !== false;
    show("runtime-communication-unavailable", !available);
    show("runtime-communication-surface", available);
    const access = communicationReadAvailable === null
        ? (runtimeLanguage === "zh-CN" ? "正在检查 communication:read…" : "communication:read checking…")
        : available
            ? "communication:read" + (communicationManageAvailable === false
                ? (runtimeLanguage === "zh-CN" ? " · 只读" : " · read only")
                : (runtimeLanguage === "zh-CN" ? " · 当前视图每 30 秒刷新 · 端点租约每 30 秒续期" : " · 30s refresh while visible · 30s endpoint lease renewal"))
            : (runtimeLanguage === "zh-CN" ? "communication:read 不可用" : "communication:read unavailable");
    setText("runtime-communication-status", access);
}
function renderCommunicationAgents() {
    setText("runtime-communication-count", countLabel(communicationAgents.length, "Agent"));
    const list = el("runtime-agent-list");
    clearNode(list);
    show("runtime-agent-empty", communicationReadAvailable === true && communicationAgents.length === 0);
    if (!list)
        return;
    for (const agent of communicationAgents) {
        const agentId = String(agent?.agent_id || "");
        if (!agentId)
            continue;
        const row = document.createElement("button");
        row.type = "button";
        row.className = "communication-row" + (agentId === selectedCommunicationAgentId ? " selected" : "");
        if (agentId === selectedCommunicationAgentId)
            row.setAttribute("aria-current", "true");
        const head = document.createElement("div");
        head.className = "communication-row-head";
        const title = document.createElement("span");
        title.className = "communication-row-title";
        title.textContent = String(agent?.display_name || agent?.handle || "Agent") + " · @" + String(agent?.handle || "agent");
        const unread = document.createElement("span");
        unread.className = "chip" + (Number(agent?.queued_delivery_count || 0) > 0 ? " tone-warn" : "");
        unread.textContent = countLabel(agent?.queued_delivery_count, "queued delivery");
        head.appendChild(title);
        head.appendChild(unread);
        row.appendChild(head);
        const meta = document.createElement("span");
        meta.className = "communication-row-meta";
        meta.textContent = agentId
            + (runtimeLanguage === "zh-CN" ? " · 配置版本 r" : " · profile r") + String(agent?.profile_revision || 0)
            + (runtimeLanguage === "zh-CN" ? " · 控制器 g" : " · controller g") + String(agent?.current_controller_generation || 0)
            + " · " + countLabel(agent?.active_endpoint_count, "active Endpoint")
            + " · " + countLabel(agent?.unresolved_wake_count, "unresolved Wake");
        row.appendChild(meta);
        row.addEventListener("click", () => {
            selectedCommunicationAgentId = agentId;
            communicationInbox = [];
            const participants = el("runtime-conversation-agent-ids");
            if (participants && !participants.value.trim())
                participants.value = agentId;
            renderCommunicationAgents();
            renderCommunicationAgentCard();
            renderCommunicationInbox();
            if (communicationEndpointId(agentId))
                void fetchCommunicationInbox(communicationGeneration);
        });
        list.appendChild(row);
    }
}
function renderCommunicationAgentCard() {
    const agent = selectedCommunicationAgent();
    show("runtime-agent-card", !!agent);
    if (!agent)
        return;
    const agentId = String(agent.agent_id || "");
    setText("runtime-agent-card-name", String(agent.display_name || agent.handle || tr("Agent Card")) + " · @" + String(agent.handle || "agent"));
    setText("runtime-agent-card-id", agentId);
    setText("runtime-agent-card-description", String(agent.description || tr("No description.")));
    setText("runtime-agent-card-revision", (runtimeLanguage === "zh-CN" ? "配置版本 " : "Profile revision ") + String(agent.profile_revision || 0)
        + (runtimeLanguage === "zh-CN" ? " · 控制器代数 " : " · controller generation ") + String(agent.current_controller_generation || 0)
        + (runtimeLanguage === "zh-CN" ? " · 更新于 " : " · updated ") + communicationTimeLabel(agent.updated_at_unix_ms));
    setText("runtime-agent-unread", countLabel(agent.queued_delivery_count, "queued"));
    const labels = el("runtime-agent-card-labels");
    clearNode(labels);
    if (labels) {
        for (const label of Array.isArray(agent.specialty_labels) ? agent.specialty_labels : []) {
            appendChip(labels, String(label));
        }
    }
    const unresolvedWakeCount = Number(agent.unresolved_wake_count || 0);
    const latestWakeState = String(agent.latest_wake_state || "none");
    setText("runtime-agent-wake-status", countLabel(unresolvedWakeCount, "unresolved Wake")
        + (runtimeLanguage === "zh-CN" ? " · 最近状态 " : " · latest ") + tr(latestWakeState)
        + (runtimeLanguage === "zh-CN" ? " · 收件箱投递与唤醒消费彼此独立" : " · Inbox Delivery and Wake consumption remain independent"));
    const updateForm = el("runtime-agent-update-form");
    const revision = String(agent.profile_revision || 0);
    if (updateForm && (updateForm.dataset.agentId !== agentId
        || updateForm.dataset.profileRevision !== revision)) {
        const handle = el("runtime-agent-update-handle");
        const displayName = el("runtime-agent-update-display-name");
        const description = el("runtime-agent-update-description");
        const labelsInput = el("runtime-agent-update-labels");
        if (handle)
            handle.value = String(agent.handle || "");
        if (displayName)
            displayName.value = String(agent.display_name || "");
        if (description)
            description.value = String(agent.description || "");
        if (labelsInput) {
            labelsInput.value = Array.isArray(agent.specialty_labels)
                ? agent.specialty_labels.join(", ")
                : "";
        }
        updateForm.dataset.agentId = agentId;
        updateForm.dataset.profileRevision = revision;
        setText("runtime-agent-update-status", "");
    }
    const endpoint = communicationEndpoint(agentId);
    setText("runtime-agent-endpoint-status", endpoint
        ? (runtimeLanguage === "zh-CN" ? "浏览器端点 " : "Browser Endpoint ") + endpoint.endpoint_id
            + " · " + tr(endpoint.lifecycle)
            + (runtimeLanguage === "zh-CN" ? " · 代数 " : " · generation ") + String(endpoint.controller_generation)
            + (runtimeLanguage === "zh-CN" ? " · 租约至 " : " · lease ") + communicationTimeLabel(endpoint.lease_expires_at_unix_ms)
            + (runtimeLanguage === "zh-CN" ? " · 运行控制台适配器：仅轮询（运行时可唤醒：" : " · Runtime Console adapter: polling only (runtime wake capable: ")
            + String(endpoint.wake_capable) + ")"
        : (runtimeLanguage === "zh-CN"
            ? "此窗口尚未作为该 Agent。Agent 卡片、对话、收件箱投递和唤醒意图仍会持久保留。"
            : "This window is not acting as the Agent. Agent Card, Conversations, Inbox deliveries, and Wake Intents remain durable."));
    show("runtime-agent-attach", !endpoint);
    show("runtime-agent-detach", !!endpoint);
}
function renderCommunicationConversations() {
    const list = el("runtime-conversation-list");
    clearNode(list);
    show("runtime-conversation-empty", communicationReadAvailable === true && communicationConversations.length === 0);
    if (!list)
        return;
    for (const conversation of communicationConversations) {
        const conversationId = String(conversation?.conversation_id || "");
        if (!conversationId)
            continue;
        const row = document.createElement("button");
        row.type = "button";
        row.className = "communication-row" + (conversationId === selectedCommunicationConversationId ? " selected" : "");
        if (conversationId === selectedCommunicationConversationId)
            row.setAttribute("aria-current", "true");
        const head = document.createElement("div");
        head.className = "communication-row-head";
        const title = document.createElement("span");
        title.className = "communication-row-title";
        title.textContent = String(conversation?.title || tr("Untitled Conversation"));
        const count = document.createElement("span");
        count.className = "chip";
        count.textContent = countLabel(conversation?.message_count, "message");
        head.appendChild(title);
        head.appendChild(count);
        row.appendChild(head);
        const meta = document.createElement("span");
        meta.className = "communication-row-meta";
        meta.textContent = conversationId + " · " + countLabel(conversation?.participant_count, "participant") + (runtimeLanguage === "zh-CN" ? " · 序号 " : " · seq ") + String(conversation?.last_seq || 0);
        row.appendChild(meta);
        row.addEventListener("click", () => {
            selectedCommunicationConversationId = conversationId;
            communicationDetail = null;
            renderCommunicationConversations();
            renderCommunicationConversation();
            void fetchCommunicationConversation(communicationGeneration);
        });
        list.appendChild(row);
    }
}
function deliveryAgentLabel(agentId) {
    const agent = communicationAgent(agentId);
    return agent ? String(agent.display_name || agent.handle || agentId) : agentId;
}
function renderCommunicationConversation() {
    const detail = communicationDetail;
    const summary = detail?.conversation || selectedCommunicationConversation();
    const available = !!summary && String(summary?.conversation_id || "") === selectedCommunicationConversationId;
    show("runtime-conversation-detail", available);
    show("runtime-conversation-detail-empty", !available);
    const transcript = el("runtime-conversation-transcript");
    clearNode(transcript);
    clearNode(el("runtime-conversation-participants"));
    if (!available || !detail)
        return;
    setText("runtime-conversation-name", String(summary.title || tr("Untitled Conversation")));
    setText("runtime-conversation-id", String(summary.conversation_id || ""));
    setText("runtime-conversation-seq", (runtimeLanguage === "zh-CN" ? "序号 " : "seq ") + String(summary.last_seq || 0) + " · " + countLabel(summary.message_count, "message") + ((Number(detail?.after_seq || 0) > 0 || detail.truncated) ? (runtimeLanguage === "zh-CN" ? " · 最近有界页面" : " · recent bounded page") : ""));
    const participants = el("runtime-conversation-participants");
    if (participants) {
        for (const participant of Array.isArray(detail.participants) ? detail.participants : []) {
            const kind = String(participant?.participant_kind || "participant");
            const label = kind === "agent"
                ? "Agent · " + String(participant?.display_name || participant?.handle || participant?.agent_id || tr("unknown"))
                : (runtimeLanguage === "zh-CN" ? "人工 · " : "Human · ") + String(participant?.principal_kind || (runtimeLanguage === "zh-CN" ? "凭证主体" : "credential principal"));
            appendChip(participants, label, kind === "agent" ? "tone-pass" : "tone-runtime");
        }
    }
    const messages = Array.isArray(detail.messages) ? detail.messages : [];
    show("runtime-conversation-transcript-empty", messages.length === 0);
    if (!transcript)
        return;
    for (const message of messages) {
        const author = message?.author || {};
        const agentAuthored = String(author.participant_kind || "") === "agent";
        const card = document.createElement("article");
        card.className = "conversation-message" + (agentAuthored ? " agent-authored" : "");
        const head = document.createElement("div");
        head.className = "conversation-message-head";
        const name = document.createElement("span");
        name.className = "conversation-message-author";
        name.textContent = agentAuthored
            ? "Agent · " + String(author.display_name || author.handle || author.agent_id || tr("unknown"))
            : (runtimeLanguage === "zh-CN" ? "人工 · " : "Human · ") + String(author.principal_kind || (runtimeLanguage === "zh-CN" ? "凭证主体" : "credential principal"));
        const seq = document.createElement("span");
        seq.className = "muted small";
        seq.textContent = "#" + String(message?.seq || 0) + " · " + communicationTimeLabel(message?.created_at_unix_ms);
        head.appendChild(name);
        head.appendChild(seq);
        card.appendChild(head);
        const meta = document.createElement("div");
        meta.className = "conversation-message-meta";
        const metaParts = [String(message?.message_id || "")];
        if (author.agent_id)
            metaParts.push(String(author.agent_id));
        if (message?.reply_to)
            metaParts.push((runtimeLanguage === "zh-CN" ? "回复 " : "reply to ") + String(message.reply_to));
        meta.textContent = metaParts.join(" · ");
        card.appendChild(meta);
        const body = document.createElement("div");
        body.className = "conversation-message-body";
        body.textContent = String(message?.body || "");
        card.appendChild(body);
        const deliveries = Array.isArray(message?.deliveries) ? message.deliveries : [];
        const delivery = document.createElement("div");
        delivery.className = "conversation-message-deliveries";
        delivery.textContent = deliveries.length
            ? (runtimeLanguage === "zh-CN" ? "Agent 收件箱：" : "Agent Inbox: ") + deliveries.map((item) => deliveryAgentLabel(String(item?.recipient_agent_id || "")) + " " + tr(String(item?.state || "unknown"))).join(" · ")
            : (runtimeLanguage === "zh-CN" ? "没有 Agent 收件箱投递 · 仅保留记录 / 人工房间" : "No Agent Inbox delivery · transcript / Human room only");
        card.appendChild(delivery);
        transcript.appendChild(card);
    }
    transcript.scrollTop = transcript.scrollHeight;
}
function renderCommunicationInbox() {
    const list = el("runtime-inbox-list");
    clearNode(list);
    const agent = selectedCommunicationAgent();
    const endpointId = communicationEndpointId();
    show("runtime-inbox-consume-all", !!endpointId && communicationInbox.length > 0);
    if (!agent) {
        setText("runtime-inbox-status", runtimeLanguage === "zh-CN" ? "选择一个 Agent 以查看收件人专属的排队投递。" : "Select an Agent to inspect recipient-specific queued deliveries.");
        return;
    }
    if (!endpointId) {
        setText("runtime-inbox-status", runtimeLanguage === "zh-CN" ? "将此浏览器附加为端点。离线期间排队投递仍会持久保留。" : "Attach this browser as an Endpoint. Queued deliveries remain durable while offline.");
        return;
    }
    const totalQueued = Number(agent.queued_delivery_count || 0);
    setText("runtime-inbox-status", countLabel(totalQueued, "queued delivery")
        + (communicationInbox.length < totalQueued ? (runtimeLanguage === "zh-CN" ? " · 当前显示 " : " · showing ") + String(communicationInbox.length) : "")
        + (runtimeLanguage === "zh-CN" ? " · 读取不会消费投递或唤醒模型" : " · reading does not consume or wake a model"));
    if (!list)
        return;
    for (const item of communicationInbox) {
        const row = document.createElement("article");
        row.className = "communication-row inbox-delivery";
        const head = document.createElement("div");
        head.className = "communication-row-head";
        const title = document.createElement("span");
        title.className = "communication-row-title";
        title.textContent = String(item?.conversation_title || tr("Untitled Conversation")) + " · #" + String(item?.message?.seq || 0);
        const consume = document.createElement("button");
        consume.type = "button";
        consume.className = "text-button";
        consume.textContent = tr("Consume");
        consume.addEventListener("click", () => void consumeCommunicationDeliveries([String(item?.delivery_id || "")]));
        head.appendChild(title);
        head.appendChild(consume);
        row.appendChild(head);
        const meta = document.createElement("span");
        meta.className = "communication-row-meta";
        meta.textContent = String(item?.delivery_id || "") + (runtimeLanguage === "zh-CN" ? " · 来自 " : " · from ") + (item?.message?.author?.participant_kind === "agent" ? deliveryAgentLabel(String(item.message.author.agent_id || "")) : (runtimeLanguage === "zh-CN" ? "人工" : "Human"));
        row.appendChild(meta);
        const body = document.createElement("div");
        body.className = "inbox-message-preview";
        body.textContent = String(item?.message?.body || "");
        row.appendChild(body);
        list.appendChild(row);
    }
}
function renderCommunicationSurface() {
    renderCommunicationAvailability();
    const signature = renderFingerprint({
        language: runtimeLanguage,
        read: communicationReadAvailable,
        manage: communicationManageAvailable,
        selectedAgent: selectedCommunicationAgentId,
        selectedConversation: selectedCommunicationConversationId,
        endpoints: Array.from(communicationEndpoints.entries()).sort(([left], [right]) => left.localeCompare(right)),
        agents: communicationAgents,
        conversations: communicationConversations,
        detail: communicationDetail,
        inbox: communicationInbox,
    });
    if (signature === renderedCommunicationSurfaceSignature)
        return;
    renderedCommunicationSurfaceSignature = signature;
    renderCommunicationAgents();
    renderCommunicationAgentCard();
    renderCommunicationConversations();
    renderCommunicationConversation();
    renderCommunicationInbox();
}
async function fetchCommunicationAgents(generation, render = true) {
    const response = await api("communication/agents", { offset: 0, limit: 100 });
    if (generation !== communicationGeneration || !response)
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 403) {
        communicationReadAvailable = false;
        communicationAgents = [];
        communicationConversations = [];
        communicationDetail = null;
        communicationInbox = [];
        if (render)
            renderCommunicationSurface();
        return true;
    }
    if (!response.ok || !response.data)
        return false;
    communicationReadAvailable = true;
    communicationAgents = Array.isArray(response.data.agents) ? response.data.agents : [];
    if (!communicationAgents.some((agent) => String(agent?.agent_id || "") === selectedCommunicationAgentId)) {
        selectedCommunicationAgentId = String(communicationAgents[0]?.agent_id || "");
        communicationInbox = [];
    }
    if (render) {
        renderCommunicationAgents();
        renderCommunicationAgentCard();
    }
    return true;
}
async function fetchCommunicationConversations(generation, render = true) {
    const response = await api("communication/conversations", { offset: 0, limit: 100 });
    if (generation !== communicationGeneration || !response)
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 403) {
        communicationReadAvailable = false;
        if (render)
            renderCommunicationSurface();
        return true;
    }
    if (!response.ok || !response.data)
        return false;
    communicationReadAvailable = true;
    communicationConversations = Array.isArray(response.data.conversations) ? response.data.conversations : [];
    if (!communicationConversations.some((conversation) => String(conversation?.conversation_id || "") === selectedCommunicationConversationId)) {
        selectedCommunicationConversationId = String(communicationConversations[0]?.conversation_id || "");
        communicationDetail = null;
    }
    if (render)
        renderCommunicationConversations();
    return true;
}
async function fetchCommunicationConversation(generation, render = true) {
    const conversationId = selectedCommunicationConversationId;
    if (!conversationId) {
        communicationDetail = null;
        if (render)
            renderCommunicationConversation();
        return true;
    }
    const afterSeq = runtimeCommunicationTranscriptAfterSeq(selectedCommunicationConversation()?.last_seq, 100);
    const response = await api("communication/conversation", {
        conversation_id: conversationId,
        after_seq: afterSeq,
        limit: 100,
    });
    if (generation !== communicationGeneration || conversationId !== selectedCommunicationConversationId || !response)
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 403) {
        communicationReadAvailable = false;
        if (render)
            renderCommunicationSurface();
        return true;
    }
    if (response.status === 404) {
        communicationDetail = null;
        selectedCommunicationConversationId = "";
        if (render)
            renderCommunicationConversation();
        return false;
    }
    if (!response.ok || !response.data)
        return false;
    communicationDetail = response.data;
    if (render)
        renderCommunicationConversation();
    return true;
}
async function fetchCommunicationInbox(generation, render = true) {
    const agentId = selectedCommunicationAgentId;
    const endpoint = communicationEndpoint(agentId);
    const endpointId = endpoint?.endpoint_id || "";
    if (!agentId || !endpoint) {
        communicationInbox = [];
        if (render)
            renderCommunicationInbox();
        return true;
    }
    const response = await api("communication/inbox", {
        agent_id: agentId,
        endpoint_id: endpointId,
        expected_controller_generation: endpoint.controller_generation,
        after_delivery_order: 0,
        limit: 100,
    });
    if (generation !== communicationGeneration || agentId !== selectedCommunicationAgentId || endpointId !== communicationEndpointId(agentId) || !response)
        return false;
    if (response.status === 401) {
        lock("Credential rejected.");
        return false;
    }
    if (response.status === 403) {
        communicationReadAvailable = false;
        if (render)
            renderCommunicationSurface();
        return true;
    }
    if (response.status === 404 || response.status === 400) {
        communicationEndpoints.delete(agentId);
        communicationInbox = [];
        if (render) {
            renderCommunicationAgentCard();
            renderCommunicationInbox();
        }
        return false;
    }
    if (!response.ok || !response.data)
        return false;
    communicationInbox = Array.isArray(response.data.deliveries) ? response.data.deliveries : [];
    if (render)
        renderCommunicationInbox();
    return true;
}
async function renewCommunicationEndpoints(generation) {
    if (communicationEndpoints.size === 0 || communicationManageAvailable === false)
        return true;
    for (const [agentId, endpoint] of Array.from(communicationEndpoints.entries())) {
        const response = await api("communication/endpoint/renew", {
            endpoint_id: endpoint.endpoint_id,
            expected_controller_generation: endpoint.controller_generation,
        });
        if (generation !== communicationGeneration)
            return false;
        if (!response)
            return false;
        if (response.status === 401) {
            lock("Credential rejected.");
            return false;
        }
        if (response.status === 403) {
            communicationManageAvailable = false;
            renderCommunicationAvailability();
            return true;
        }
        if (response.status === 400 || response.status === 404) {
            communicationEndpoints.delete(agentId);
            if (agentId === selectedCommunicationAgentId)
                communicationInbox = [];
            continue;
        }
        if (!response.ok || !response.data?.endpoint?.endpoint_id)
            return false;
        communicationManageAvailable = true;
        communicationEndpoints.set(agentId, response.data.endpoint);
    }
    return true;
}
async function performCommunicationRefresh(includeData) {
    if (!token)
        return true;
    const generation = ++communicationGeneration;
    if (includeData && communicationReadAvailable !== false) {
        setText("runtime-communication-status", tr("Refreshing durable communication…"));
    }
    const endpointsOk = await renewCommunicationEndpoints(generation);
    if (generation !== communicationGeneration)
        return false;
    if (!includeData || communicationReadAvailable === false)
        return endpointsOk;
    const agentsOk = await fetchCommunicationAgents(generation, false);
    if (generation !== communicationGeneration || !agentsOk || communicationReadAvailable !== true) {
        renderCommunicationSurface();
        return endpointsOk && agentsOk;
    }
    const conversationsOk = await fetchCommunicationConversations(generation, false);
    if (generation !== communicationGeneration || !conversationsOk || communicationReadAvailable !== true) {
        renderCommunicationSurface();
        return endpointsOk && agentsOk && conversationsOk;
    }
    const [conversationOk, inboxOk] = await Promise.all([
        fetchCommunicationConversation(generation, false),
        fetchCommunicationInbox(generation, false),
    ]);
    renderCommunicationSurface();
    return endpointsOk && agentsOk && conversationsOk && conversationOk && inboxOk;
}
function refreshCommunication(includeData = true) {
    if (!token)
        return Promise.resolve(true);
    return communicationRefreshCoordinator.refresh(includeData);
}
async function createCommunicationAgent(event) {
    event.preventDefault();
    const handle = el("runtime-agent-handle")?.value.trim() || "";
    const displayName = el("runtime-agent-display-name")?.value.trim() || "";
    const description = el("runtime-agent-description")?.value.trim() || "";
    const labels = parseAgentIds(el("runtime-agent-labels")?.value || "");
    if (!handle || !displayName) {
        setText("runtime-agent-create-status", tr("Handle and display name are required."));
        return;
    }
    const fingerprint = JSON.stringify({ handle, displayName, description, labels });
    pendingAgentCreate = idempotencyKeyFor(pendingAgentCreate, fingerprint, "runtime-agent");
    setText("runtime-agent-create-status", tr("Creating durable Agent…"));
    const response = await api("communication/agent/create", {
        handle,
        display_name: displayName,
        description,
        specialty_labels: labels,
        idempotency_key: pendingAgentCreate.key,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-agent-create-status", tr("communication:manage required."));
        renderCommunicationAvailability();
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-agent-create-status", tr("Outcome uncertain. Keep inputs unchanged and retry to replay the same idempotency key, or refresh before deciding."));
        return;
    }
    if (!response.ok || !response.data?.agent) {
        setText("runtime-agent-create-status", String(response.data?.message || "Agent creation failed."));
        return;
    }
    communicationManageAvailable = true;
    selectedCommunicationAgentId = String(response.data.agent.agent_id || "");
    pendingAgentCreate = null;
    for (const id of ["runtime-agent-handle", "runtime-agent-display-name", "runtime-agent-description", "runtime-agent-labels"]) {
        const input = el(id);
        if (input)
            input.value = "";
    }
    setText("runtime-agent-create-status", tr(response.data.replayed ? "Existing idempotent Agent replayed." : "Agent created."));
    await refreshCommunication();
}
async function updateCommunicationAgent(event) {
    event.preventDefault();
    const agent = selectedCommunicationAgent();
    if (!agent)
        return;
    const handle = el("runtime-agent-update-handle")?.value.trim() || "";
    const displayName = el("runtime-agent-update-display-name")?.value.trim() || "";
    const description = el("runtime-agent-update-description")?.value.trim() || "";
    const specialtyLabels = parseAgentIds(el("runtime-agent-update-labels")?.value || "");
    if (!handle || !displayName) {
        setText("runtime-agent-update-status", tr("Handle and display name are required."));
        return;
    }
    setText("runtime-agent-update-status", tr("Updating Agent Card…"));
    const response = await api("communication/agent/update", {
        agent_id: String(agent.agent_id || ""),
        expected_profile_revision: Number(agent.profile_revision || 0),
        handle,
        display_name: displayName,
        description,
        specialty_labels: specialtyLabels,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-agent-update-status", tr("communication:manage required."));
        renderCommunicationAvailability();
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-agent-update-status", tr("Outcome uncertain. Refresh the Card before deciding whether to retry."));
        return;
    }
    if (!response.ok || !response.data?.agent_id) {
        setText("runtime-agent-update-status", response.data?.message ? String(response.data.message) : tr("Agent Card update failed; refresh before retrying a stale revision."));
        return;
    }
    communicationManageAvailable = true;
    setText("runtime-agent-update-status", tr("Agent Card updated."));
    await refreshCommunication();
}
async function attachCommunicationEndpoint() {
    const agentId = selectedCommunicationAgentId;
    if (!agentId)
        return;
    for (const [otherAgentId, otherEndpoint] of Array.from(communicationEndpoints.entries())) {
        if (otherAgentId === agentId)
            continue;
        setText("runtime-agent-endpoint-status", tr("Releasing this window’s previous Agent Endpoint…"));
        const detached = await api("communication/endpoint/detach", {
            endpoint_id: otherEndpoint.endpoint_id,
        });
        if (detached?.status === 401) {
            lock("Credential rejected.");
            return;
        }
        if (detached?.status === 403) {
            communicationManageAvailable = false;
            setText("runtime-agent-endpoint-status", tr("communication:manage required."));
            return;
        }
        if (!detached || detached.status === 0 || detached.status === 503) {
            setText("runtime-agent-endpoint-status", tr("Previous Endpoint detach is uncertain. Refresh before switching this window to another Agent."));
            return;
        }
        if (!detached.ok && detached.status !== 404) {
            setText("runtime-agent-endpoint-status", detached.data?.message ? String(detached.data.message) : tr("Could not release the previous Agent Endpoint."));
            return;
        }
        communicationEndpoints.delete(otherAgentId);
    }
    let pending = pendingEndpointAttach.get(agentId);
    if (!pending) {
        pending = { key: operationKey("runtime-endpoint"), attachmentId: pageAttachmentId + "-" + agentId.slice(-8) };
        pendingEndpointAttach.set(agentId, pending);
    }
    setText("runtime-agent-endpoint-status", tr("Attaching browser Endpoint…"));
    const response = await api("communication/endpoint/attach", {
        agent_id: agentId,
        host: "Runtime Console",
        client_attachment_id: pending.attachmentId,
        idempotency_key: pending.key,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-agent-endpoint-status", tr("communication:manage required."));
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-agent-endpoint-status", tr("Outcome uncertain. Retry Attach to replay the same idempotency key; do not create a new attachment."));
        return;
    }
    if (!response.ok || !response.data?.endpoint?.endpoint_id) {
        setText("runtime-agent-endpoint-status", String(response.data?.message || "Endpoint attach failed."));
        return;
    }
    if (String(response.data.endpoint.lifecycle || "") !== "attached") {
        pendingEndpointAttach.delete(agentId);
        setText("runtime-agent-endpoint-status", tr("The exact Attach replay was already replaced. Choose “Continue as this Agent” again to create a fresh Endpoint generation."));
        return;
    }
    communicationManageAvailable = true;
    communicationEndpoints.set(agentId, response.data.endpoint);
    pendingEndpointAttach.delete(agentId);
    renderCommunicationAgentCard();
    await fetchCommunicationInbox(communicationGeneration);
}
async function detachCommunicationEndpoint() {
    const agentId = selectedCommunicationAgentId;
    const endpointId = communicationEndpointId(agentId);
    if (!agentId || !endpointId)
        return;
    setText("runtime-agent-endpoint-status", tr("Detaching browser Endpoint…"));
    const response = await api("communication/endpoint/detach", { endpoint_id: endpointId });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-agent-endpoint-status", tr("communication:manage required."));
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-agent-endpoint-status", tr("Detach outcome uncertain. Refresh before retry; the durable Agent and Inbox are unaffected."));
        return;
    }
    if (!response.ok) {
        setText("runtime-agent-endpoint-status", String(response.data?.message || "Endpoint detach failed."));
        return;
    }
    communicationEndpoints.delete(agentId);
    communicationInbox = [];
    renderCommunicationAgentCard();
    renderCommunicationInbox();
    await refreshCommunication();
}
async function createCommunicationConversation(event) {
    event.preventDefault();
    const title = el("runtime-conversation-title")?.value.trim() || "";
    const idsInput = el("runtime-conversation-agent-ids")?.value || "";
    const agentIds = parseAgentIds(idsInput || selectedCommunicationAgentId);
    if (agentIds.length === 0) {
        setText("runtime-conversation-create-status", tr("At least one Agent id is required."));
        return;
    }
    const fingerprint = JSON.stringify({ title, agentIds: [...agentIds].sort() });
    pendingConversationCreate = idempotencyKeyFor(pendingConversationCreate, fingerprint, "runtime-conversation");
    setText("runtime-conversation-create-status", tr("Creating durable Conversation…"));
    const response = await api("communication/conversation/create", {
        title: title || null,
        agent_ids: agentIds,
        idempotency_key: pendingConversationCreate.key,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-conversation-create-status", tr("communication:manage required."));
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-conversation-create-status", tr("Outcome uncertain. Keep inputs unchanged and retry to replay the same idempotency key."));
        return;
    }
    if (!response.ok || !response.data?.conversation?.conversation?.conversation_id) {
        setText("runtime-conversation-create-status", String(response.data?.message || "Conversation creation failed."));
        return;
    }
    communicationManageAvailable = true;
    selectedCommunicationConversationId = String(response.data.conversation.conversation.conversation_id);
    pendingConversationCreate = null;
    const titleInput = el("runtime-conversation-title");
    const agentsInput = el("runtime-conversation-agent-ids");
    if (titleInput)
        titleInput.value = "";
    if (agentsInput)
        agentsInput.value = selectedCommunicationAgentId;
    setText("runtime-conversation-create-status", tr(response.data.replayed ? "Existing idempotent Conversation replayed." : "Conversation created."));
    await refreshCommunication();
}
async function postCommunicationMessage(event) {
    event.preventDefault();
    const conversationId = selectedCommunicationConversationId;
    const bodyNode = el("runtime-conversation-body");
    const recipientsNode = el("runtime-conversation-recipients");
    const body = bodyNode?.value.trim() || "";
    const recipientsText = recipientsNode?.value.trim() || "";
    const sendAsAgent = el("runtime-conversation-send-as-agent")?.checked === true;
    if (!conversationId || !body) {
        setText("runtime-conversation-send-status", tr("Select a Conversation and enter a message."));
        return;
    }
    const actingAgent = sendAsAgent ? selectedCommunicationAgent() : null;
    const endpoint = actingAgent ? communicationEndpoint(String(actingAgent.agent_id || "")) : null;
    if (sendAsAgent && (!actingAgent || !endpoint)) {
        setText("runtime-conversation-send-status", tr("Select an Agent and choose “Continue as this Agent” before sending as it."));
        return;
    }
    const recipientAgentIds = recipientsText ? parseAgentIds(recipientsText) : null;
    const fingerprint = JSON.stringify({
        conversationId,
        body,
        recipientAgentIds,
        authorAgentId: actingAgent?.agent_id || null,
        endpointId: endpoint?.endpoint_id || null,
        controllerGeneration: endpoint?.controller_generation || null,
    });
    pendingConversationMessage = idempotencyKeyFor(pendingConversationMessage, fingerprint, "runtime-message");
    setText("runtime-conversation-send-status", tr("Appending Message and Agent deliveries atomically…"));
    const response = await api("communication/message/post", {
        conversation_id: conversationId,
        body,
        author_agent_id: actingAgent?.agent_id || null,
        endpoint_id: endpoint?.endpoint_id || null,
        expected_controller_generation: endpoint?.controller_generation || null,
        recipient_agent_ids: recipientAgentIds,
        idempotency_key: pendingConversationMessage.key,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-conversation-send-status", tr("communication:manage required."));
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-conversation-send-status", tr("Outcome uncertain. Keep the message unchanged and retry only to replay the same idempotency key, or refresh the transcript first."));
        return;
    }
    if (!response.ok || !response.data?.message) {
        setText("runtime-conversation-send-status", String(response.data?.message || "Message append failed."));
        return;
    }
    communicationManageAvailable = true;
    pendingConversationMessage = null;
    if (bodyNode)
        bodyNode.value = "";
    setText("runtime-conversation-send-status", tr(response.data.replayed ? "Existing Message replayed without duplicate delivery." : "Durable Message sent."));
    await refreshCommunication();
}
async function consumeCommunicationDeliveries(deliveryIds) {
    const agentId = selectedCommunicationAgentId;
    const endpoint = communicationEndpoint(agentId);
    const endpointId = endpoint?.endpoint_id || "";
    const ids = deliveryIds.filter(Boolean);
    if (!agentId || !endpoint || ids.length === 0)
        return;
    setText("runtime-inbox-status", tr("Consuming recipient state…"));
    const response = await api("communication/inbox/consume", {
        agent_id: agentId,
        endpoint_id: endpointId,
        expected_controller_generation: endpoint.controller_generation,
        delivery_ids: ids,
    });
    if (response?.status === 401) {
        lock("Credential rejected.");
        return;
    }
    if (response?.status === 403) {
        communicationManageAvailable = false;
        setText("runtime-inbox-status", tr("communication:manage required to consume deliveries."));
        return;
    }
    if (!response || response.status === 0 || response.status === 503) {
        setText("runtime-inbox-status", tr("Consume outcome uncertain. Refresh before retry; desired-state replay is safe."));
        return;
    }
    if (!response.ok) {
        setText("runtime-inbox-status", response.data?.message ? String(response.data.message) : tr("Delivery consume failed."));
        return;
    }
    communicationManageAvailable = true;
    await refreshCommunication();
}
function setRefreshBusy(active) {
    refreshInFlight = active;
    const button = el("runtime-refresh");
    if (button) {
        button.disabled = active;
        button.classList.toggle("is-busy", active);
        button.title = active ? tr("Refreshing runtime") : tr("Refresh runtime");
        button.setAttribute("aria-label", button.title);
    }
}
async function refreshAll() {
    if (!token || refreshInFlight)
        return;
    setRefreshBusy(true);
    setText("runtime-refresh-status", tr("Refreshing…"));
    const recoverCollaboration = runtimeCollaborationNeedsRefreshRecovery(state);
    const overviewRequest = refreshRuntimeOverview(state);
    const projectsRequest = refreshRuntimeProjects(state, projectSearch, projectDeviceFilter);
    try {
        const [overviewOk, projectsOk, communicationOk] = await Promise.all([
            fetchOverview(overviewRequest),
            fetchProjects(projectsRequest),
            refreshCommunication(),
        ]);
        if (!token)
            return;
        if (overviewOk && projectsOk && communicationOk) {
            setText("runtime-refresh-status", tr("Refreshed") + " " + new Date().toLocaleTimeString());
        }
        else {
            setText("runtime-refresh-status", tr("Refresh failed · showing previous data"));
        }
        if (recoverCollaboration && runtimeCollaborationNeedsRefreshRecovery(state)) {
            const collaborationRequest = runtimeCollaborationRequest(state);
            if (collaborationRequest)
                void startCollaboration(collaborationRequest);
        }
    }
    finally {
        setRefreshBusy(false);
    }
}
function refreshAutoSurfaces() {
    if (!token)
        return;
    if (document.hidden) {
        void refreshCommunication(false);
        return;
    }
    void fetchOverview(refreshRuntimeOverview(state));
    const request = refreshRuntimeSessionList(state);
    if (request)
        void fetchSessions(request);
    void refreshCommunication(workspaceView === "operations");
}
function startAuto() {
    stopAuto();
    timer = window.setInterval(refreshAutoSurfaces, REFRESH_MS);
}
function stopAuto() { if (timer)
    window.clearInterval(timer); timer = 0; }
function startWindowAuto() {
    stopWindowAuto();
    if (!token || workspaceView !== "windows" || document.hidden)
        return;
    windowTimer = window.setInterval(() => void refreshWindows(true), WINDOW_REFRESH_MS);
}
function stopWindowAuto() {
    if (windowTimer)
        window.clearInterval(windowTimer);
    windowTimer = 0;
}
function connectRuntimeCredential(nextToken, rememberForTab) {
    rememberCredentialForTab = rememberForTab;
    token = nextToken;
    setRuntimeConnectionState("connecting");
    const remember = el("runtime-token-remember");
    if (remember)
        remember.checked = rememberForTab;
    const request = beginRuntimeCredential(state);
    void fetchOverview(refreshRuntimeOverview(state));
    void fetchProjects(request, true);
    void refreshCommunication(workspaceView === "operations");
    if (workspaceView === "windows") {
        void refreshWindows(true);
        startWindowAuto();
    }
}
el("runtime-token-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const input = el("runtime-token-input");
    const remember = el("runtime-token-remember");
    const nextToken = input ? input.value.trim() : "";
    if (input)
        input.value = "";
    if (!nextToken) {
        setText("runtime-token-error", tr("Enter a runtime Bearer credential."));
        return;
    }
    connectRuntimeCredential(nextToken, remember?.checked !== false);
});
el("runtime-agent-create-form")?.addEventListener("submit", (event) => void createCommunicationAgent(event));
el("runtime-window-copy-key")?.addEventListener("click", () => {
    const key = String(selectedWindowDetail?.client_window_key || selectedWindowKey || "");
    void copyRuntimeValue(key, "runtime-window-list-status");
});
el("runtime-agent-update-form")?.addEventListener("submit", (event) => void updateCommunicationAgent(event));
el("runtime-agent-attach")?.addEventListener("click", () => void attachCommunicationEndpoint());
el("runtime-agent-detach")?.addEventListener("click", () => void detachCommunicationEndpoint());
el("runtime-conversation-create-form")?.addEventListener("submit", (event) => void createCommunicationConversation(event));
el("runtime-conversation-message-form")?.addEventListener("submit", (event) => void postCommunicationMessage(event));
el("runtime-inbox-consume-all")?.addEventListener("click", () => {
    void consumeCommunicationDeliveries(communicationInbox.map((item) => String(item?.delivery_id || "")).filter(Boolean));
});
el("runtime-device-select")?.addEventListener("change", () => {
    const select = el("runtime-device-select");
    if (!select)
        return;
    applyRunnerFilter(select.value);
});
el("runtime-session-search")?.addEventListener("input", () => renderSessionList(sessionRows, sessionListMetaSnapshot));
el("runtime-message-search")?.addEventListener("input", filterCollaborationMessages);
el("runtime-project-search")?.addEventListener("input", () => {
    const input = el("runtime-project-search");
    projectSearch = input?.value || "";
    stopProjectSearchTimer();
    if (!token)
        return;
    setText("runtime-project-status", tr("Searching…"));
    projectSearchTimer = window.setTimeout(() => {
        projectSearchTimer = 0;
        if (!token)
            return;
        void fetchProjects(refreshRuntimeProjects(state, projectSearch, projectDeviceFilter));
    }, PROJECT_SEARCH_DEBOUNCE_MS);
});
el("runtime-message-kind")?.addEventListener("change", syncAckComposer);
el("runtime-message-priority")?.addEventListener("change", syncAckComposer);
el("runtime-message-requires-ack")?.addEventListener("change", syncComposerOptionSummary);
el("runtime-message-body")?.addEventListener("input", () => {
    setText("runtime-message-send-status", "");
    saveCurrentDraft();
    syncCollaborationComposerLayout();
});
el("runtime-message-body")?.addEventListener("keydown", (event) => {
    if (!(event instanceof KeyboardEvent) || event.key !== "Enter" || event.shiftKey || event.isComposing || event.keyCode === 229)
        return;
    const body = event.currentTarget;
    const send = el("runtime-message-send");
    const form = el("runtime-collaboration-form");
    event.preventDefault();
    if (!body?.value.trim() || send?.disabled || !form)
        return;
    form.requestSubmit();
});
el("runtime-message-reply-clear")?.addEventListener("click", () => setCollaborationReplyTarget(""));
el("runtime-message-edit-clear")?.addEventListener("click", cancelCollaborationEdit);
el("runtime-collaboration-form")?.addEventListener("submit", (event) => void postHumanCollaborationMessage(event));
el("runtime-chat-scroll")?.addEventListener("scroll", updateCollaborationFollowFromScroll, { passive: true });
el("runtime-new-messages")?.addEventListener("click", () => scrollCollaborationToLatest(true));
el("runtime-refresh")?.addEventListener("click", () => {
    closeTopbarMore(false);
    void refreshAll();
});
el("runtime-lock")?.addEventListener("click", () => lock());
el("runtime-mobile-nav-toggle")?.addEventListener("click", () => setMobileNavigationOpen(true));
el("runtime-find-project")?.addEventListener("click", focusProjectNavigation);
el("runtime-welcome-overview")?.addEventListener("click", () => revealOperationsSection("runtime-operations-overview"));
el("runtime-welcome-agents")?.addEventListener("click", () => revealOperationsSection("runtime-operations-agents"));
el("runtime-mobile-nav-close")?.addEventListener("click", () => setMobileNavigationOpen(false, true));
el("runtime-mobile-nav-backdrop")?.addEventListener("click", () => setMobileNavigationOpen(false, true));
el("runtime-inspector-backdrop")?.addEventListener("click", () => closeRuntimeInspector(true));
el("runtime-inspector-close")?.addEventListener("click", () => closeRuntimeInspector(true, true));
document.querySelector(".context-trigger")?.addEventListener("click", (event) => {
    event.preventDefault();
    const inspector = document.querySelector(".runtime-inspector");
    const currentlyVisible = !!inspector?.open;
    contextUserIntent = reduceRuntimeContextUserIntent(contextUserIntent, {
        type: "toggle_trigger",
        currentVisible: currentlyVisible,
    });
    if (contextUserIntent && mobileNavigationViewport()) {
        setMobileNavigationOpen(false, false);
    }
    syncContextUi(false);
});
document.querySelectorAll("[data-runtime-view]").forEach((button) => {
    button.addEventListener("click", () => applyWorkspaceView(workspaceViewPreference(button.dataset.runtimeView)));
});
document.querySelectorAll("[data-context-target]").forEach((button) => {
    button.addEventListener("click", () => {
        document.querySelectorAll("[data-context-target]").forEach((item) => {
            const selected = item === button;
            item.setAttribute("aria-pressed", String(selected));
            show(String(item.dataset.contextTarget), selected);
        });
    });
});
document.querySelectorAll("[data-operations-target]").forEach((button) => {
    button.addEventListener("click", () => revealOperationsSection(String(button.dataset.operationsTarget || "runtime-operations-overview")));
});
document.querySelectorAll("[data-language-toggle]").forEach((button) => {
    button.addEventListener("click", () => {
        applyLanguage(runtimeLanguage === "zh-CN" ? "en" : "zh-CN");
        closeAppearanceMenus(false);
        closeTopbarMore(false);
    });
});
document.querySelectorAll("[data-theme-option]").forEach((button) => {
    button.addEventListener("click", () => {
        applyAppearance(appearancePreference(button.dataset.themeOption));
        const menu = button.closest("details.theme-menu");
        if (menu)
            menu.open = false;
        closeTopbarMore(false);
    });
});
el("runtime-topbar-more")?.addEventListener("toggle", (event) => {
    const menu = event.currentTarget;
    if (!menu?.open)
        return;
    closeComposerOptions(false);
    closeRuntimeInspector(false);
    setMobileNavigationOpen(false, false);
});
document.querySelectorAll("details.theme-menu").forEach((menu) => {
    menu.addEventListener("toggle", () => {
        if (!menu.open)
            return;
        closeAppearanceMenus(false, menu);
        closeRuntimeInspector(false);
        setMobileNavigationOpen(false, false);
    });
});
el("runtime-message-options")?.addEventListener("toggle", (event) => {
    const options = event.currentTarget;
    if (!options?.open)
        return;
    closeAppearanceMenus(false);
    setMobileNavigationOpen(false, false);
});
document.addEventListener("pointerdown", (event) => {
    const target = event.target;
    if (!(target instanceof Node))
        return;
    document.querySelectorAll("details.theme-menu[open]").forEach((menu) => {
        if (!menu.contains(target))
            menu.open = false;
    });
    const options = el("runtime-message-options");
    if (options?.open && !options.contains(target))
        options.open = false;
    const topbarMore = el("runtime-topbar-more");
    if (topbarMore?.open && !topbarMore.contains(target))
        topbarMore.open = false;
});
el("runtime-jump-latest")?.addEventListener("click", jumpLatest);
el("runtime-timeline")?.addEventListener("scroll", () => {
    const node = el("runtime-timeline");
    if (!node)
        return;
    updateWorkflowSessionFollowFromScroll(state.workflow, node.scrollTop, node.clientHeight, node.scrollHeight);
    syncFollowUi();
});
document.querySelector(".runtime-inspector")?.addEventListener("toggle", (event) => {
    const inspector = event.currentTarget;
    if (!inspector)
        return;
    const resolved = resolveRuntimeContextState({
        userIntent: contextUserIntent,
        isWideViewport: window.matchMedia(WIDE_CONTEXT_MEDIA).matches,
        isMobileViewport: mobileNavigationViewport(),
        hasSelectedSession: !!state.workflow?.selectedSessionId,
        workspaceView,
    });
    if (inspector.open !== resolved.visible) {
        inspector.open = resolved.visible;
    }
});
document.addEventListener("keydown", (event) => {
    const shell = el("runtime-console");
    if (!event.isComposing && !event.altKey && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k" && shell && !shell.hidden) {
        event.preventDefault();
        focusProjectNavigation();
        return;
    }
    const inspector = document.querySelector(".runtime-inspector");
    if (event.key === "Escape") {
        if (closeComposerOptions(true)) {
            event.preventDefault();
            return;
        }
        if (closeAppearanceMenus(true)) {
            event.preventDefault();
            return;
        }
        if (closeTopbarMore(true)) {
            event.preventDefault();
            return;
        }
        if (inspector?.open && !shell?.classList.contains("context-docked")) {
            event.preventDefault();
            closeRuntimeInspector(true);
            return;
        }
        if (shell?.classList.contains("context-docked") && inspector?.open && inspector.contains(document.activeElement)) {
            event.preventDefault();
            closeRuntimeInspector(true, true);
            return;
        }
        if (shell?.classList.contains("mobile-nav-open")) {
            event.preventDefault();
            setMobileNavigationOpen(false, true);
        }
        return;
    }
    if (event.key !== "Tab" || !shell?.classList.contains("mobile-nav-open"))
        return;
    const sidebar = el("runtime-sidebar");
    if (!sidebar)
        return;
    const focusable = visibleFocusableElements(sidebar);
    if (!focusable.length)
        return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
    }
    else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
    }
});
window.addEventListener("resize", syncResponsiveNavigation, { passive: true });
document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
        stopWindowAuto();
        return;
    }
    if (!token)
        return;
    refreshAutoSurfaces();
    if (workspaceView === "windows") {
        void refreshWindows(true);
        startWindowAuto();
    }
});
const syncSystemAppearance = () => {
    if (appearancePreference(document.documentElement.dataset.theme) === "system")
        applyAppearance("system", false);
};
if (typeof appearanceMedia.addEventListener === "function")
    appearanceMedia.addEventListener("change", syncSystemAppearance);
else
    appearanceMedia.addListener(syncSystemAppearance);
captureStaticUiSources();
applyLanguage(loadLanguagePreference(), false, false);
applyAppearance(loadAppearancePreference(), false);
applyWorkspaceView(loadWorkspaceViewPreference(), false);
syncAckComposer();
window.addEventListener("pagehide", () => {
    saveCurrentDraft();
    detachCommunicationEndpointsBestEffort();
    token = "";
    abortAll();
    resetCommunicationSurface();
    stopAuto();
    stopWindowAuto();
});
lock("", false);
const rememberedRuntimeCredential = loadRememberedRuntimeCredential();
if (rememberedRuntimeCredential) {
    setText("runtime-token-error", tr("Restoring this tab…"));
    connectRuntimeCredential(rememberedRuntimeCredential, true);
}
