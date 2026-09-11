use super::common::OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION;
use serde_json::{json, Value};

pub fn computer_list_windows_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose desktop is observed."},
            "limit": {"type": "integer", "minimum": 1, "description": "Optional bounded window count; values above 64 are accepted and clamped to 64."}
        },
        "required": ["client_id"]
    })
}

pub fn computer_list_displays_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose full displays are observed."},
            "limit": {"type": "integer", "minimum": 1, "description": "Optional bounded display count; defaults to 16 and values above 16 are accepted and clamped to 16."}
        },
        "required": ["client_id"]
    })
}

pub fn computer_list_applications_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact macOS or Windows Runner client_id whose installed applications are discovered."},
            "limit": {"type": "integer", "minimum": 1, "description": "Optional bounded application count; defaults to 64 and values above 64 are accepted and clamped to 64."}
        },
        "required": ["client_id"]
    })
}

pub fn computer_launch_application_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact macOS or Windows Runner client_id that produced the application_id."},
            "application_id": {"type": "string", "pattern": "^application_[0-9a-f]{32}$", "maxLength": 128, "description": "Fresh opaque process-local application_id returned by computer_list_applications."}
        },
        "required": ["client_id", "application_id"]
    })
}

pub fn computer_accessibility_status_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose Accessibility trust is queried without prompting."}
        },
        "required": ["client_id"]
    })
}

pub fn computer_accessibility_tree_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose desktop is inspected."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local surface_id returned by computer_list_windows."},
            "max_depth": {"type": "integer", "minimum": 0, "description": "Maximum AX descendant depth; values above 8 are accepted and clamped to 8."},
            "max_nodes": {"type": "integer", "minimum": 1, "description": "Maximum semantic AX elements returned; values above 256 are accepted and clamped to 256."}
        },
        "required": ["client_id", "surface_id"]
    })
}

pub fn computer_find_elements_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose Accessibility surface is searched."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local surface_id returned by computer_list_windows."},
            "role": {"type": "string", "minLength": 1, "maxLength": 256, "description": "Optional exact Accessibility role match."},
            "subrole": {"type": "string", "minLength": 1, "maxLength": 256, "description": "Optional exact Accessibility subrole match."},
            "label": {"type": "string", "minLength": 1, "maxLength": 256, "description": "Optional case-sensitive literal substring matched only against title, description, or placeholder; AXValue is never searched."},
            "focused": {"type": "boolean", "description": "Optional exact focused-state match; unknown/null state does not match."},
            "enabled": {"type": "boolean", "description": "Optional exact enabled-state match; unknown/null state does not match."},
            "limit": {"type": "integer", "minimum": 1, "description": "Maximum matching elements returned; defaults to 8 and values above 32 are accepted and clamped to 32."}
        },
        "required": ["client_id", "surface_id"]
    })
}

pub fn computer_element_state_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose observed element is revalidated."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque process-local surface_id that owns the element."},
            "element_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact ephemeral element_id returned by computer_accessibility_tree or computer_find_elements."}
        },
        "required": ["client_id", "surface_id", "element_id"]
    })
}

pub fn computer_activate_window_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose already-observed window is activated."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque process-local surface_id returned by computer_list_windows."}
        },
        "required": ["client_id", "surface_id"]
    })
}

pub fn computer_control_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose macOS Accessibility element is controlled."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque surface_id used to obtain the element."},
            "element_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local element_id returned by computer_accessibility_tree or computer_find_elements."},
            "action": {"type": "string", "enum": ["press", "focus"], "description": "Bounded control action. CU-AX2 supports only press and focus."}
        },
        "required": ["client_id", "surface_id", "element_id", "action"]
    })
}

pub fn computer_scroll_to_element_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose macOS Accessibility or Windows UIA element is scrolled into view."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque surface_id used to obtain the element."},
            "element_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local element_id returned by computer_accessibility_tree or computer_find_elements."}
        },
        "required": ["client_id", "surface_id", "element_id"]
    })
}

pub fn computer_key_input_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose already-focused macOS or Windows window receives the key."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque process-local surface_id that must still be the frontmost focused window."},
            "key": {"type": "string", "enum": ["enter", "escape", "tab", "arrow_up", "arrow_down", "arrow_left", "arrow_right", "page_up", "page_down", "home", "end"], "description": "Closed navigation/action key vocabulary. Ordinary text must use computer_input_text."},
            "modifiers": {"type": "array", "maxItems": 4, "uniqueItems": true, "items": {"type": "string", "enum": ["shift", "control", "option", "command"]}, "description": "Optional bounded modifier set for this call only. On Windows, option maps to Alt and command fails closed before input."}
        },
        "required": ["client_id", "surface_id", "key"]
    })
}

pub fn computer_read_clipboard_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact macOS or Windows Runner whose global plain Unicode-text clipboard is observed."}
        },
        "required": ["client_id"]
    })
}

pub fn computer_write_clipboard_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact macOS or Windows Runner whose global clipboard is replaced with plain Unicode text."},
            "text": {"type": "string", "minLength": 1, "maxLength": 16384, "description": "Unicode text replacement. Runtime enforces non-empty, NUL-free UTF-8 of at most 16 KiB."}
        },
        "required": ["client_id", "text"]
    })
}

pub fn computer_input_text_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose already-focused macOS Accessibility text element is mutated."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact opaque surface_id used to obtain the element."},
            "element_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local element_id returned by computer_accessibility_tree or computer_find_elements."},
            "text": {"type": "string", "minLength": 1, "maxLength": 2048, "description": "Caller text written verbatim with AXValue. Runtime enforces a 2048-byte UTF-8 ceiling and rejects NUL; the target must already be focused and empty."}
        },
        "required": ["client_id", "surface_id", "element_id", "text"]
    })
}

pub fn computer_snapshot_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose desktop is observed."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local surface_id returned by computer_list_windows."},
            "region": {
                "type": "object",
                "additionalProperties": false,
                "description": "Optional rectangle in the revalidated surface coordinate space. It must fit fully inside the exact surface.",
                "properties": {
                    "x": {"type": "integer", "minimum": 0, "maximum": 4294967295u64},
                    "y": {"type": "integer", "minimum": 0, "maximum": 4294967295u64},
                    "width": {"type": "integer", "minimum": 1, "maximum": 4294967295u64},
                    "height": {"type": "integer", "minimum": 1, "maximum": 4294967295u64}
                },
                "required": ["x", "y", "width", "height"]
            },
            "max_width": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output width. Never upscales."},
            "max_height": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output height. Never upscales."}
        },
        "required": ["client_id", "surface_id"]
    })
}

pub fn computer_snapshot_display_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id that produced the display_id."},
            "display_id": {"type": "string", "pattern": "^display_[0-9a-f]{32}$", "maxLength": 128, "description": "Fresh opaque process-local display_id returned by computer_list_displays."},
            "max_width": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output width. Never upscales."},
            "max_height": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output height. Never upscales."}
        },
        "required": ["client_id", "display_id"]
    })
}

pub fn computer_pointer_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id that produced the display snapshot."},
            "display_id": {"type": "string", "pattern": "^display_[0-9a-f]{32}$", "maxLength": 128, "description": "Exact opaque process-local display_id bound to snapshot_generation."},
            "snapshot_generation": {"type": "integer", "minimum": 1, "maximum": 4294967295u64, "description": "Latest unspent successful full-display snapshot generation for this display."},
            "x": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Display-local source-space x coordinate; must be less than the bound snapshot source_width."},
            "y": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Display-local source-space y coordinate; must be less than the bound snapshot source_height."}
        },
        "required": ["client_id", "display_id", "snapshot_generation", "x", "y"]
    })
}

pub fn computer_save_snapshot_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project": {"type": "string", "minLength": 1, "description": "Target project that will receive the create-only snapshot artifact."},
            "path": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "Project-relative artifact path. The first version is create-only and never overwrites an existing file."},
            "client_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Exact Runner client_id whose desktop is observed."},
            "surface_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Opaque process-local surface_id returned by computer_list_windows."},
            "region": {
                "type": "object",
                "additionalProperties": false,
                "description": "Optional rectangle in the revalidated surface coordinate space. It must fit fully inside the exact surface.",
                "properties": {
                    "x": {"type": "integer", "minimum": 0, "maximum": 4294967295u64},
                    "y": {"type": "integer", "minimum": 0, "maximum": 4294967295u64},
                    "width": {"type": "integer", "minimum": 1, "maximum": 4294967295u64},
                    "height": {"type": "integer", "minimum": 1, "maximum": 4294967295u64}
                },
                "required": ["x", "y", "width", "height"]
            },
            "max_width": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output width. Never upscales."},
            "max_height": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Optional upper bound on encoded output height. Never upscales."},
            "session_id": {"type": "string", "minLength": 1, "description": OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION}
        },
        "required": ["project", "path", "client_id", "surface_id"]
    })
}
