const TOKEN_KEY = "drop_token";

function getToken() {
  return localStorage.getItem(TOKEN_KEY) || "";
}

function setToken(token) {
  localStorage.setItem(TOKEN_KEY, token);
}

function clearToken() {
  localStorage.removeItem(TOKEN_KEY);
}

function requireToken() {
  if (!getToken()) {
    window.location.href = "/login";
    return false;
  }
  return true;
}

async function apiCall(url, options = {}) {
  const token = getToken();
  if (!token) {
    window.location.href = "/login";
    return null;
  }

  const headers = new Headers(options.headers || {});
  headers.set("Authorization", `Bearer ${token}`);

  const response = await fetch(url, { ...options, headers });
  if (response.status === 401) {
    clearToken();
    window.location.href = "/login";
    return null;
  }
  return response;
}

function escapeHtml(value) {
  const div = document.createElement("div");
  div.textContent = String(value ?? "");
  return div.innerHTML;
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
  return `${(bytes / 1073741824).toFixed(2)} GB`;
}

function fmtTime(timestampSeconds) {
  return new Date(timestampSeconds * 1000).toLocaleString();
}

async function deleteMsg(id) {
  if (!confirm("Delete this message?")) return;
  const response = await apiCall(`/api/messages/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (response?.ok) window.location.reload();
}

function copyText(text) {
  navigator.clipboard.writeText(text).then(() => alert("Copied!"));
}

Object.assign(window, {
  getToken,
  setToken,
  clearToken,
  requireToken,
  apiCall,
  escapeHtml,
  formatSize,
  fmtTime,
  deleteMsg,
  copyText,
});
