#![cfg(target_os = "linux")]

#[path = "sandbox_smoke/cases.rs"]
mod cases;

#[tokio::test]
async fn governed_application_writes_and_reads_the_workspace() {
    cases::governed_application_writes_and_reads_the_workspace().await;
}

#[tokio::test]
async fn generation_bound_tools_are_ambient_in_danger_mode() {
    cases::generation_bound_tools_are_ambient_in_danger_mode().await;
}

#[tokio::test]
async fn hostile_relative_path_values_are_typed_rejections_and_never_executed() {
    cases::hostile_relative_path_values_are_typed_rejections_and_never_executed().await;
}

#[tokio::test]
async fn hostile_absolute_path_values_are_literal_strings_never_evaluated() {
    cases::hostile_absolute_path_values_are_literal_strings_never_evaluated().await;
}

#[test]
fn sandbox_settings_resolution_and_wire_precedence_are_deterministic() {
    cases::sandbox_settings_resolution_and_wire_precedence_are_deterministic();
}
