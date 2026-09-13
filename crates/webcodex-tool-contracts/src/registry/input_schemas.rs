mod agent_tasks;
mod artifacts;
#[cfg(feature = "workspace-checkpoints")]
mod checkpoints;
mod cleanup;
mod coding;
mod coding_agents;
mod common;
mod communication;
mod computer;
mod discovery;
mod files;
mod git;
mod goals;
mod hygiene;
mod jobs;
mod line_edits;
mod lsp;
mod memory;
mod patches;
mod plugins;
mod projects;
mod runner_config;
mod sessions;
mod skills;
mod ssh_resources;
mod text_edits;
mod validation;

pub use agent_tasks::{
    assign_agent_task_input_schema, complete_agent_task_attempt_input_schema,
    create_agent_task_input_schema, heartbeat_agent_task_attempt_input_schema,
    list_agent_tasks_input_schema, read_agent_task_input_schema,
    reconcile_agent_task_coding_run_input_schema, start_agent_task_attempt_input_schema,
    start_agent_task_coding_run_input_schema, start_agent_task_endpoint_continuation_input_schema,
};
pub use artifacts::{
    artifact_upload_abort_input_schema, artifact_upload_begin_input_schema,
    artifact_upload_chunk_input_schema, artifact_upload_finish_input_schema,
    export_project_artifact_input_schema, import_conversation_files_to_project_input_schema,
    read_project_artifact_input_schema, read_project_artifact_metadata_input_schema,
    save_project_artifact_input_schema,
};
#[cfg(feature = "workspace-checkpoints")]
pub use checkpoints::{
    checkpoint_create_input_schema, checkpoint_delete_input_schema, checkpoint_labels_schema,
    checkpoint_list_input_schema, checkpoint_restore_input_schema, checkpoint_show_input_schema,
    checkpoint_validation_schema,
};
pub use cleanup::{
    delete_project_files_input_schema, discard_untracked_input_schema,
    git_restore_paths_input_schema,
};
pub use coding::{finish_coding_task_input_schema, work_on_project_input_schema};
pub use coding_agents::{
    coding_agent_cancel_input_schema, coding_agent_observe_input_schema,
    coding_agent_start_input_schema,
};
pub use communication::{
    agent_continuation_bind_input_schema, agent_continuation_recover_endpoint_input_schema,
    agent_continuation_state_input_schema, agent_continuation_unbind_input_schema,
    agent_continuation_wake_acquire_input_schema, agent_continuation_wake_finish_input_schema,
    agent_continuation_wake_prepare_input_schema, attach_agent_endpoint_input_schema,
    bootstrap_agent_conversation_input_schema, consume_agent_deliveries_input_schema,
    consume_agent_wake_input_schema, create_agent_identity_input_schema,
    create_conversation_input_schema, detach_agent_endpoint_input_schema,
    list_agent_identities_input_schema, list_agent_inbox_input_schema,
    list_conversations_input_schema, post_conversation_message_input_schema,
    present_agent_continuation_input_schema, read_conversation_input_schema,
    update_agent_identity_input_schema,
};
pub use computer::{
    computer_accessibility_status_input_schema, computer_accessibility_tree_input_schema,
    computer_activate_window_input_schema, computer_control_input_schema,
    computer_element_state_input_schema, computer_find_elements_input_schema,
    computer_input_text_input_schema, computer_key_input_input_schema,
    computer_launch_application_input_schema, computer_list_applications_input_schema,
    computer_list_displays_input_schema, computer_list_windows_input_schema,
    computer_pointer_input_schema, computer_read_clipboard_input_schema,
    computer_save_snapshot_input_schema, computer_scroll_to_element_input_schema,
    computer_snapshot_display_input_schema, computer_snapshot_input_schema,
    computer_write_clipboard_input_schema,
};
#[cfg(any(test, feature = "root-test-support"))]
pub use discovery::ACCEPTED_FLATTENED_ARG_PREFERRED_ORDER;
pub use discovery::{accepted_flattened_args_for_spec, generic_tool_call_flattened_args_for_spec};
pub use discovery::{
    empty_input_schema, list_projects_input_schema, list_runners_input_schema,
    list_tools_input_schema, read_tool_trace_input_schema, runtime_status_input_schema,
    tool_manifest_input_schema,
};
pub use files::{
    list_project_files_input_schema, list_project_tracked_files_input_schema,
    project_overview_input_schema, read_files_input_schema, search_project_texts_input_schema,
};
pub use git::{
    git_commit_paths_input_schema, git_diff_hunks_input_schema, git_log_input_schema,
    git_review_summary_input_schema, git_status_input_schema, show_changes_input_schema,
};
pub use goals::{
    associate_goal_agent_task_input_schema, associate_goal_workflow_session_input_schema,
    create_goal_input_schema, get_goal_input_schema, goal_plan_state_input_schema,
    list_goals_input_schema, present_goal_plan_input_schema, update_goal_input_schema,
};
pub use hygiene::workspace_hygiene_check_input_schema;
pub use jobs::{
    list_jobs_input_schema, observe_jobs_input_schema, open_session_shell_input_schema,
    run_detached_process_input_schema, run_job_input_schema, run_process_input_schema,
    run_script_input_schema, run_shell_input_schema, session_shell_exec_input_schema,
    session_shell_identity_input_schema, stop_job_input_schema,
};
pub use line_edits::apply_text_edits_input_schema;
pub use lsp::{
    call_hierarchy_input_schema, document_diagnostics_input_schema, document_symbols_input_schema,
    find_references_input_schema, goto_definition_input_schema, hover_input_schema,
    lsp_status_input_schema, workspace_symbols_input_schema,
};
pub(super) use memory::{
    memory_delete_input_schema, memory_read_input_schema, memory_scope_list_input_schema,
    memory_scope_purge_input_schema, memory_search_input_schema, memory_set_input_schema,
};
pub use patches::{apply_patch_input_schema, apply_unified_diff_input_schema};
pub use plugins::plugin_tool_input_schema;
pub use projects::{
    create_project_input_schema, register_project_input_schema, unregister_project_input_schema,
};
pub use runner_config::{runner_config_check_input_schema, runner_config_reload_input_schema};
pub use sessions::{
    close_session_input_schema, complete_session_message_input_schema,
    get_session_assignment_input_schema, list_session_messages_input_schema,
    observe_session_messages_input_schema, post_session_message_input_schema,
    resolve_session_message_input_schema, session_discussion_summary_input_schema,
    session_execution_context_schema, session_guards_schema, session_handoff_summary_input_schema,
    session_lifecycle_schema, session_mode_schema, session_summary_input_schema,
    update_session_context_input_schema, validation_summary_input_schema,
};
pub(super) use skills::{
    skill_activate_input_schema, skill_install_input_schema, skill_list_input_schema,
    skill_read_file_input_schema, skill_remove_revision_input_schema, skill_versions_input_schema,
};
pub use ssh_resources::ssh_resource_input_schema;
pub use text_edits::write_project_file_input_schema;
pub use validation::{
    cargo_check_input_schema, cargo_fmt_input_schema, cargo_test_input_schema, go_test_input_schema,
};
