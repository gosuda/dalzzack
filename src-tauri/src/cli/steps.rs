//! Batch step execution engine — runs a JSON steps file to completion.
//!
//! Without a live instance it is self-contained: starts browser → runs all
//! steps → prints results → exits. `main` runs steps on a live instance itself.

use std::path::Path;

/// One step: a tool and its arguments.
#[derive(serde::Deserialize, Debug)]
pub struct Step {
    pub tool: String,
    pub args: Option<serde_json::Value>,
    /// The step as the window's toolbar shows it (`cli::command_label`). A step
    /// read from a file's `{"tool", "args"}` form has none, and then the toolbar
    /// shows the tool name.
    #[serde(default)]
    pub display: Option<String>,
}

/// Steps file format
#[derive(serde::Deserialize, Debug)]
struct StepsFile {
    #[allow(dead_code)]
    name: Option<String>,
    steps: Vec<Step>,
}

/// The JSON in a steps file, or in stdin for `-`, with $ENV_VAR references expanded.
pub fn read_steps(path: &Path) -> Result<serde_json::Value, String> {
    let content = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin())
    } else {
        std::fs::read_to_string(path)
    }
    .map_err(|e| format!("Cannot read steps file '{}': {}", path.display(), e))?;
    serde_json::from_str(&expand_env_vars(&content))
        .map_err(|e| format!("Invalid steps JSON: {}", e))
}

/// The steps of a `{"steps": [{"tool": ..., "args": ...}]}` document.
pub fn object_steps(value: serde_json::Value) -> Result<Vec<Step>, String> {
    let file: StepsFile =
        serde_json::from_value(value).map_err(|e| format!("Invalid steps JSON: {}", e))?;
    Ok(file.steps)
}

/// Run batch steps: start a Tauri WebView, execute steps, exit.
pub fn run_steps(instance_id: &str, steps: Vec<Step>) {
    if steps.is_empty() {
        println!(
            "{}",
            serde_json::json!({ "status": "completed", "message": "No steps to execute" })
        );
        return;
    }

    // Setup logging
    crate::setup_panic_hook();
    let _log_guard = crate::init_file_logging(Some(instance_id));

    tracing::info!(
        "=== dalzzack Batch Run Starting (id: {}, steps: {}) ===",
        instance_id,
        steps.len()
    );

    let id_clone = instance_id.to_string();
    let id_for_gui = instance_id.to_string();

    // Spawn step execution in a background thread (needs tokio runtime)
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(async {
            // Wait for WebView to be ready
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            // CDP setup
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                if let Err(e) = crate::setup_cdp().await {
                    tracing::warn!("[Steps] CDP setup failed: {}", e);
                }
            }

            // A batch run uses the same shared WebView2 profile a plain daemon
            // does. Saying so is what lets the shared-profile guard refuse a
            // `cookies` clear or `reset` step that would wipe every live
            // daemon's logins.
            crate::profile::set(crate::profile::Profile {
                kind: if cfg!(target_os = "windows") {
                    crate::profile::Kind::Shared
                } else {
                    crate::profile::Kind::Platform
                },
                path: if cfg!(target_os = "windows") {
                    crate::profile::default_webview_dir()
                } else {
                    None
                },
                disposable: false,
            });

            // The same always-on capture a daemon arms, so console-logs,
            // response-headers, nav-log and failed-requests steps have
            // something to read.
            #[cfg(target_os = "windows")]
            {
                use crate::recording::cdp::Mode;
                match crate::recording::cdp::enable_and_subscribe(Mode::Console).await {
                    Ok(()) => crate::console::set_transport(crate::console::Transport::Cdp),
                    Err(e) => {
                        tracing::warn!("[Steps] CDP capture unavailable: {e}");
                        crate::console::set_transport_error(
                            crate::console::Transport::Unavailable,
                            &e,
                        );
                    }
                }
            }

            let mut results = Vec::new();
            let mut failed = false;

            for (i, step) in steps.iter().enumerate() {
                let mut args = step.args.clone().unwrap_or(serde_json::json!({}));
                // Auto-inject "site" from instance_id if not present
                if let Some(obj) = args.as_object_mut() {
                    if !obj.contains_key("site") {
                        obj.insert(
                            "site".to_string(),
                            serde_json::Value::String(id_clone.clone()),
                        );
                    }
                }
                // The tool name only: step arguments can hold form values.
                tracing::info!("[Step {}/{}] {}", i + 1, steps.len(), step.tool);

                let label = step.display.clone().unwrap_or_else(|| step.tool.clone());
                let result = crate::activity::track(
                    label,
                    crate::tools::definitions::execute_tool(&step.tool, &args),
                )
                .await;
                let (step_result, step_failed) = step_report(i + 1, &step.tool, result);
                println!(
                    "{}",
                    serde_json::to_string(&step_result).unwrap_or_default()
                );
                results.push(step_result);
                if step_failed {
                    failed = true;
                    break; // Stop on first failure
                }
            }

            // Print summary
            let summary = serde_json::json!({
                "status": if failed { "failed" } else { "completed" },
                "steps_executed": results.len(),
                "steps_total": steps.len(),
            });
            println!("{}", serde_json::to_string(&summary).unwrap_or_default());

            // Exit after completion
            std::process::exit(if failed { 1 } else { 0 });
        });
    });

    // Run the Tauri GUI (blocks until window closes or process exits)
    crate::run_with_instance_id(crate::app::AppOptions {
        instance_id: Some(id_for_gui),
        ..Default::default()
    });
}

/// The output line of one step, and whether the run must stop. A tool reports
/// its own failure inside `Ok`, as `status: "failed"`.
fn step_report(
    step: usize,
    tool: &str,
    result: anyhow::Result<serde_json::Value>,
) -> (serde_json::Value, bool) {
    match result {
        Ok(output) => {
            let failed = output["status"] == "failed";
            let report = serde_json::json!({
                "step": step,
                "tool": tool,
                "status": if failed { "failed" } else { "completed" },
                "result": output,
            });
            (report, failed)
        }
        Err(e) => {
            let report = serde_json::json!({
                "step": step,
                "tool": tool,
                "status": "failed",
                "error": e.to_string(),
            });
            (report, true)
        }
    }
}

/// Expand $ENV_VAR and ${ENV_VAR} references in a string using environment variables.
fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' && i + 1 < len {
            if chars[i + 1] == '{' {
                // ${VAR} form
                if let Some(end) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[i + 2..i + 2 + end].iter().collect();
                    match std::env::var(&var_name) {
                        Ok(val) => result.push_str(&val),
                        Err(_) => {
                            // Keep original if not found
                            result.push('$');
                            result.push('{');
                            result.push_str(&var_name);
                            result.push('}');
                        }
                    }
                    i += 3 + end; // skip ${VAR}
                    continue;
                }
            } else if chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_' {
                // $VAR form
                let start = i + 1;
                let mut end = start;
                while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let var_name: String = chars[start..end].iter().collect();
                match std::env::var(&var_name) {
                    Ok(val) => result.push_str(&val),
                    Err(_) => {
                        result.push('$');
                        result.push_str(&var_name);
                    }
                }
                i = end;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steps_document_gives_tools_and_arguments() {
        let value = serde_json::json!({
            "name": "login",
            "steps": [{ "tool": "get_html" }, { "tool": "click", "args": { "x": 1 } }],
        });
        let steps = object_steps(value).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].tool, "get_html");
        assert_eq!(steps[1].args, Some(serde_json::json!({ "x": 1 })));
        assert!(object_steps(serde_json::json!({ "steps": 1 })).is_err());
    }

    #[test]
    fn a_failure_inside_a_tool_result_stops_the_run() {
        let failed = serde_json::json!({ "status": "failed", "error": "ELEMENT_NOT_FOUND" });
        let (report, stop) = step_report(2, "click_element", Ok(failed));
        assert!(stop);
        assert_eq!(report["status"], "failed");
        assert_eq!(report["result"]["error"], "ELEMENT_NOT_FOUND");

        let done = serde_json::json!({ "status": "completed" });
        let (report, stop) = step_report(3, "get_html", Ok(done));
        assert!(!stop);
        assert_eq!(report["status"], "completed");

        let (report, stop) = step_report(4, "navigate", Err(anyhow::anyhow!("boom")));
        assert!(stop);
        assert_eq!(report["error"], "boom");
    }

    #[test]
    fn test_expand_env_vars() {
        std::env::set_var("TEST_VAR", "hello");
        assert_eq!(expand_env_vars("$TEST_VAR world"), "hello world");
        assert_eq!(expand_env_vars("${TEST_VAR} world"), "hello world");
        assert_eq!(expand_env_vars("no vars here"), "no vars here");
        assert_eq!(
            expand_env_vars("$NONEXISTENT_VAR_12345"),
            "$NONEXISTENT_VAR_12345"
        );
        std::env::remove_var("TEST_VAR");
    }
}
