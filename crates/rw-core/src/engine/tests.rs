#![cfg(test)]
#![allow(clippy::expect_used)]

mod attachments;
mod budget;
mod cancellation;
mod close;
mod command_copy;
mod command_lifetime;
mod commands;
mod compaction;
mod compaction_recovery;
mod completion_hooks;
mod context;
mod context_reads;
mod context_source_commit;
mod control_admission;
mod deferred_children;
mod diff_approval;
mod doom_loop;
mod event_batches;
pub(crate) mod fixtures;
mod generation_lifetime;
mod history_compaction;
mod hooks;
mod model_preparation;
mod model_selection;
mod modes;
mod mutation_checkpoints;
mod navigation;
mod permissions;
mod persistence;
mod plan;
mod plugin_capability;
mod plugin_tools;
mod protocol_control;
mod provider_usage;
mod questions;
mod reasoning;
mod recovery;
mod redaction;
mod retained_inputs;
mod rewind;
mod shell;
mod snapshots;
mod startup;
mod subagents;
mod subscription;
mod titles;
mod todos;
mod tool_admission;
mod tool_order;
mod tool_result_admission;
mod ui_actions;

mod citations;

mod context_allocation;
mod context_cache;

mod family_controls;
