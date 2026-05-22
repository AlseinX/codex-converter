//! Live integration tests: Codex CLI → proxy → upstream Anthropic API.
//!
//! Each test starts its own in-process proxy via `tokio::spawn` and runs
//! `codex exec` through it. Credentials come from `$HOME/.claude/settings.json`
//! or environment variables.
//!
//! Run with: `cargo test --test integrations --test-threads=1`

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use codex_conv::config::AppConfig;
use codex_conv::router::{AppState, build_router};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

fn get_credential(settings_key: &str, env_name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name)
        && !v.is_empty()
    {
        return Some(v);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".claude/settings.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&content).ok()?;
    settings
        .get("env")
        .and_then(|e| e.get(settings_key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn api_key() -> String {
    get_credential("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN")
        .expect("ANTHROPIC_AUTH_TOKEN not found in ~/.claude/settings.json or env")
}

fn base_url() -> String {
    get_credential("ANTHROPIC_BASE_URL", "ANTHROPIC_BASE_URL")
        .expect("ANTHROPIC_BASE_URL not found in ~/.claude/settings.json or env")
}

// ---------------------------------------------------------------------------
// In-process proxy (one per test)
// ---------------------------------------------------------------------------

/// Isolated codex home directory, created once and reused across tests.
/// Lives at `tests/.codex-test-home/` (gitignored).
fn codex_test_home() -> &'static PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let dir = PathBuf::from(manifest_dir).join("tests/.codex-test-home");
        std::fs::create_dir_all(&dir).expect("failed to create codex test home dir");

        // Write auth.json with the API key.
        let key = api_key();
        let auth = serde_json::json!({ "OPENAI_API_KEY": key });
        std::fs::write(dir.join("auth.json"), auth.to_string()).expect("failed to write auth.json");

        // Write minimal config.toml.
        std::fs::write(
            dir.join("config.toml"),
            r#"[model_providers.custom]
name = "test"

[features]
"#,
        )
        .expect("failed to write config.toml");

        // Write models_cache.json with apply_patch_tool_type set so that Codex
        // registers the ApplyPatchHandler.  Without this, Codex defaults to
        // apply_patch_tool_type: None and the handler is never registered,
        // causing "unsupported custom tool call: apply_patch".
        let model =
            std::env::var("CODEX_CONV_TEST_MODEL").unwrap_or_else(|_| "glm-5.1".to_string());
        let models_cache = serde_json::json!({
            "fetched_at": "2099-01-01T00:00:00Z",
            "client_version": "0.130.0",
            "models": [{
                "slug": model,
                "display_name": model,
                "description": "test model via codex-converter proxy",
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 0,
                "upgrade": null,
                "base_instructions": "",
                "supports_reasoning_summaries": false,
                "support_verbosity": false,
                "default_verbosity": null,
                "apply_patch_tool_type": "freeform",
                "truncation_policy": {"mode": "bytes", "limit": 10000},
                "supports_parallel_tool_calls": false,
                "supports_image_detail_original": false,
                "context_window": 200000,
                "max_context_window": 200000,
                "experimental_supported_tools": [],
            }]
        });
        std::fs::write(
            dir.join("models_cache.json"),
            serde_json::to_string_pretty(&models_cache).unwrap(),
        )
        .expect("failed to write models_cache.json");

        dir
    })
}

/// Start the proxy on a random port. Returns the listening address.
/// The server lives as long as the returned `JoinHandle` is not dropped.
async fn start_proxy() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind proxy listener");
    let addr = listener.local_addr().unwrap();

    let config = AppConfig::default();
    let state = AppState {
        config,
        catalog: None,
    };
    let app = build_router(state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {e}");
        }
    });

    (addr, handle)
}

// ---------------------------------------------------------------------------
// Codex exec helper
// ---------------------------------------------------------------------------

async fn codex_exec(
    proxy_addr: SocketAddr,
    prompt: &str,
    workdir: Option<&std::path::Path>,
) -> Vec<String> {
    // codex sends POST to base_url + "/responses".
    // Proxy route expects /https/<host>/responses, so base_url = http://proxy/https/<host>.
    let upstream = base_url();
    let host = upstream
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let proxy_base = format!("http://{}/https/{host}", proxy_addr);

    let key = api_key();
    let model = std::env::var("CODEX_CONV_TEST_MODEL").unwrap_or_else(|_| "glm-5.1".to_string());
    let codex_home = codex_test_home();

    let tmpdir;
    let cwd = match workdir {
        Some(p) => p.as_os_str().to_owned(),
        None => {
            tmpdir = tempfile::tempdir().expect("tempdir");
            std::process::Command::new("git")
                .args(["init"])
                .current_dir(tmpdir.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git init failed");
            tmpdir.path().as_os_str().to_owned()
        }
    };

    let output = Command::new("codex")
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("-m")
        .arg(&model)
        .arg("-c")
        .arg("approval_policy=\"never\"")
        .arg("-c")
        .arg("sandbox_mode=\"danger-full-access\"")
        .arg("-c")
        .arg(format!("model_providers.custom.base_url=\"{proxy_base}\""))
        .arg("-c")
        .arg("model_provider=\"custom\"")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("-C")
        .arg(&cwd)
        .arg("--ephemeral")
        .arg(prompt)
        .env("CODEX_HOME", codex_home)
        .env("OPENAI_API_KEY", &key)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to spawn codex");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "codex exec failed ({}). stderr: {}\nstdout: {}",
            output.status,
            stderr.chars().take(3000).collect::<String>(),
            stdout.chars().take(5000).collect::<String>()
        );
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// JSONL helpers
// ---------------------------------------------------------------------------

fn parse_jsonl(lines: &[String]) -> Vec<serde_json::Value> {
    lines
        .iter()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn extract_agent_message(events: &[serde_json::Value]) -> Option<String> {
    events.iter().find_map(|ev| {
        if ev.get("type").and_then(|t| t.as_str()) != Some("item.completed") {
            return None;
        }
        let item = ev.get("item")?;
        if item.get("type").and_then(|t| t.as_str()) != Some("agent_message") {
            return None;
        }
        item.get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    })
}

fn has_item_type(events: &[serde_json::Value], ty: &str) -> bool {
    events.iter().any(|ev| {
        ev.get("type").and_then(|t| t.as_str()) == Some("item.completed")
            && ev
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                == Some(ty)
    })
}

/// Helper: extract all item.completed events from JSONL
fn extract_item_completed_events(events: &[serde_json::Value]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("item.completed"))
        .cloned()
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn simple_text_response() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(
        addr,
        "Reply with exactly the word PONG and nothing else.",
        None,
    )
    .await;
    let events = parse_jsonl(&lines);
    assert!(!events.is_empty(), "must receive JSONL events");

    let msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(
        msg.to_uppercase().contains("PONG"),
        "agent should say PONG, got: {msg}"
    );
}

#[tokio::test]
async fn exec_command_tool_call() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(addr, "Run the command: echo HELLO_TOOL_TEST", None).await;
    let events = parse_jsonl(&lines);
    assert!(
        has_item_type(&events, "command_execution"),
        "must have command_execution item"
    );
    let msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(
        msg.contains("HELLO_TOOL_TEST"),
        "agent should report command output, got: {msg}"
    );
}

/// Verify that the proxy correctly converts apply_patch tool calls from the
/// OpenAI Responses API format to the Anthropic Messages API format.
///
/// This test MUST produce a `file_change` JSONL event, which indicates that
/// Codex used its built-in `apply_patch` tool. If the result is a
/// `command_execution` event instead, it means Codex fell back to using
/// `exec_command` (e.g. `sed`/`echo`) to edit the file — this is a protocol
/// conversion failure. When apply_patch is not properly converted, Codex
/// detects the tool failure and retries file editing through an alternative
/// path. That fallback is evidence that our conversion pipeline broke the
/// tool call, and this test MUST fail in that case. This requirement is
/// non-negotiable and must never be relaxed.
#[tokio::test]
async fn apply_patch_tool_call() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let file_path = tmpdir.path().join("patch_target.txt");
    std::fs::write(&file_path, "Hello World\n").unwrap();

    let lines = codex_exec(
        addr,
        "Use apply_patch to replace 'World' with 'Universe' in patch_target.txt.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change), using exec_command is wrong"
    );

    let content = std::fs::read_to_string(&file_path).unwrap();
    assert!(
        content.contains("Universe"),
        "file should say Universe: {content}"
    );
    assert!(
        !content.contains("World"),
        "World should be gone: {content}"
    );
}

#[tokio::test]
async fn multi_turn_conversation() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(
        addr,
        "Run 'uname -a' and then tell me the kernel version number only.",
        None,
    )
    .await;
    let events = parse_jsonl(&lines);
    assert!(
        has_item_type(&events, "command_execution"),
        "must have command_execution"
    );
    let msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(!msg.is_empty(), "agent must respond");
}

#[tokio::test]
async fn developer_role_mapped_correctly() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(addr, "Say OK", None).await;
    let events = parse_jsonl(&lines);
    assert!(
        extract_agent_message(&events).is_some(),
        "must receive response (developer→user mapping works)"
    );
}

#[tokio::test]
async fn stream_termination_handled() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(addr, "Say hello", None).await;
    let events = parse_jsonl(&lines);
    let has_completed = events
        .iter()
        .any(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("turn.completed"));
    assert!(
        has_completed,
        "must have turn.completed (clean stream termination)"
    );
}

// ===========================================================================
// apply_patch retry scenario tests
// ===========================================================================

#[tokio::test]
async fn apply_patch_creates_new_file() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Create a new file called hello.txt with the content 'Hello World' using apply_patch.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change), using exec_command is wrong"
    );

    let content = std::fs::read_to_string(tmpdir.path().join("hello.txt")).unwrap();
    assert!(
        content.contains("Hello World"),
        "file should contain 'Hello World', got: {content}"
    );
}

#[tokio::test]
async fn apply_patch_deletes_file() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let file_path = tmpdir.path().join("to_delete.txt");
    std::fs::write(&file_path, "delete me\n").unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Delete the file to_delete.txt using apply_patch.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change)"
    );

    assert!(!file_path.exists(), "file should be deleted");
}

#[tokio::test]
async fn apply_patch_multiple_files() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmpdir.path().join("file_a.txt"), "AAA\n").unwrap();
    std::fs::write(tmpdir.path().join("file_b.txt"), "BBB\n").unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Use apply_patch to replace 'AAA' with 'XXX' in file_a.txt AND replace 'BBB' with 'YYY' in file_b.txt. Do both in a single response.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    // Must have at least one file_change (model may use multiple turns)
    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change)"
    );

    let a = std::fs::read_to_string(tmpdir.path().join("file_a.txt")).unwrap();
    let b = std::fs::read_to_string(tmpdir.path().join("file_b.txt")).unwrap();
    assert!(a.contains("XXX"), "file_a should contain XXX: {a}");
    assert!(b.contains("YYY"), "file_b should contain YYY: {b}");
}

#[tokio::test]
async fn apply_patch_complex_edit_no_fallback() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let content = "line one\nline two\nline three\nline four\nline five\n";
    std::fs::write(tmpdir.path().join("complex.txt"), content).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Use apply_patch to change the third line of complex.txt from 'line three' to 'LINE THREE'. Do not use exec_command or shell commands for file editing.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change), using exec_command is wrong"
    );

    let result = std::fs::read_to_string(tmpdir.path().join("complex.txt")).unwrap();
    assert!(
        result.contains("LINE THREE"),
        "should contain LINE THREE: {result}"
    );
    assert!(
        !result.contains("line three"),
        "should not contain 'line three': {result}"
    );
    // Verify surrounding content preserved
    assert!(
        result.contains("line one"),
        "surrounding content should be preserved"
    );
    assert!(
        result.contains("line five"),
        "surrounding content should be preserved"
    );
}

#[tokio::test]
async fn apply_patch_with_text_explanation() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmpdir.path().join("explain.txt"), "old value\n").unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Use apply_patch to replace 'old value' with 'new value' in explain.txt, then explain what you did.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    assert!(
        has_item_type(&events, "file_change"),
        "must have file_change"
    );
    let msg =
        extract_agent_message(&events).expect("must have agent_message explaining the change");
    assert!(!msg.is_empty(), "agent should explain what it did");

    let content = std::fs::read_to_string(tmpdir.path().join("explain.txt")).unwrap();
    assert!(
        content.contains("new value"),
        "file should contain 'new value': {content}"
    );
}

#[tokio::test]
async fn apply_patch_with_reasoning() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmpdir.path().join("think_patch.txt"), "original\n").unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Think carefully about the best way to change 'original' to 'modified' in think_patch.txt, then use apply_patch to do it.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    // If thinking is enabled, we may see reasoning items, but the key assertion
    // is that apply_patch works correctly even when reasoning is present
    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change)"
    );

    let content = std::fs::read_to_string(tmpdir.path().join("think_patch.txt")).unwrap();
    assert!(
        content.contains("modified"),
        "file should contain 'modified': {content}"
    );
}

#[tokio::test]
async fn apply_patch_transparent_retry_end_to_end() {
    let (addr, _proxy) = start_proxy().await;
    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmpdir.path().join("retry_test.txt"), "initial content\n").unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let lines = codex_exec(
        addr,
        "Use apply_patch to change 'initial content' to 'replaced content' in retry_test.txt.",
        Some(tmpdir.path()),
    )
    .await;
    let events = parse_jsonl(&lines);

    // Whether retry happened or not, the end result must be correct
    assert!(
        has_item_type(&events, "file_change"),
        "must use native apply_patch (file_change)"
    );

    // Stream must terminate cleanly
    let has_completed = events
        .iter()
        .any(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("turn.completed"));
    assert!(
        has_completed,
        "must have turn.completed (clean stream termination)"
    );

    let content = std::fs::read_to_string(tmpdir.path().join("retry_test.txt")).unwrap();
    assert!(
        content.contains("replaced content"),
        "file should contain 'replaced content': {content}"
    );
}

// ===========================================================================
// Error path and code path coverage tests
// ===========================================================================

#[tokio::test]
async fn failed_command_handled_gracefully() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(
        addr,
        "Run the command: cat /nonexistent_file_xyz_12345_abc",
        None,
    )
    .await;
    let events = parse_jsonl(&lines);
    // The command will fail, but codex should handle it gracefully
    assert!(
        has_item_type(&events, "command_execution"),
        "must have command_execution item"
    );
    let msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(
        !msg.is_empty(),
        "agent should respond even after command failure"
    );
}

#[tokio::test]
async fn multi_tool_calls_in_sequence() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(
        addr,
        "Run these two commands one after another: first 'echo FIRST_TOOL', then 'echo SECOND_TOOL'",
        None,
    )
    .await;
    let events = parse_jsonl(&lines);
    // Verify that command execution happened (the alternation merging path worked)
    assert!(
        has_item_type(&events, "command_execution"),
        "must have command_execution item"
    );
    // Verify the agent responded (the multi-turn conversation completed)
    let msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(!msg.is_empty(), "agent must respond after running commands");
}

/// Verify that the JSONL output from Codex CLI contains well-structured events.
///
/// Codex CLI transforms the proxy's SSE events (response.created, response.completed,
/// etc.) into its own JSONL format with event types like `turn.completed`,
/// `item.completed`, etc. We cannot observe raw SSE events in the JSONL output,
/// so this test validates the observable event structure instead.
#[tokio::test]
async fn response_completed_has_required_fields() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(addr, "Say hello", None).await;
    let events = parse_jsonl(&lines);

    // 1. Must have item.completed events (indicates proxy produced complete output items)
    let item_events = extract_item_completed_events(&events);
    assert!(
        !item_events.is_empty(),
        "must have item.completed events (proxy produced complete output)"
    );

    // 2. Each item.completed must have a structured "item" with required fields
    for ev in &item_events {
        assert!(
            ev.get("item").is_some(),
            "item.completed must contain 'item'"
        );
        let item = ev.get("item").unwrap();
        assert!(item.get("type").is_some(), "item must have type field");
    }

    // 3. Must have at least one agent_message item (the text response)
    let agent_msg = extract_agent_message(&events).expect("must have agent_message");
    assert!(
        !agent_msg.is_empty(),
        "agent_message text must not be empty"
    );

    // 4. Must have turn.completed (clean stream termination)
    let has_turn_completed = events
        .iter()
        .any(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("turn.completed"));
    assert!(
        has_turn_completed,
        "must have turn.completed (clean stream termination)"
    );
}

#[tokio::test]
async fn upstream_connection_error_handled() {
    let (addr, _proxy) = start_proxy().await;
    // Use a non-existent upstream host to trigger connection failure
    let proxy_base = format!("http://{}/https/nonexistent.invalid.example.com", addr);
    let key = api_key();
    let model = std::env::var("CODEX_CONV_TEST_MODEL").unwrap_or_else(|_| "glm-5.1".to_string());
    let codex_home = codex_test_home();

    let tmpdir = tempfile::tempdir().expect("tempdir");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmpdir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init failed");

    let output = Command::new("codex")
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("-m")
        .arg(&model)
        .arg("-c")
        .arg("approval_policy=\"never\"")
        .arg("-c")
        .arg("sandbox_mode=\"danger-full-access\"")
        .arg("-c")
        .arg(format!("model_providers.custom.base_url=\"{proxy_base}\""))
        .arg("-c")
        .arg("model_provider=\"custom\"")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("-C")
        .arg(tmpdir.path())
        .arg("--ephemeral")
        .arg("Say hello")
        .env("CODEX_HOME", codex_home)
        .env("OPENAI_API_KEY", &key)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to spawn codex");

    // codex may fail (non-zero exit) or may get an error event in JSONL
    // Either way, the proxy must not crash
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The proxy should not panic — if it did, the test process would hang or fail differently
    // We just verify that something was returned (even if it's an error)
    assert!(
        !stdout.is_empty() || !stderr.is_empty(),
        "proxy must return something (error or response), not hang"
    );
}

#[tokio::test]
async fn minimal_prompt_response() {
    let (addr, _proxy) = start_proxy().await;
    let lines = codex_exec(addr, "Hi", None).await;
    let events = parse_jsonl(&lines);
    assert!(!events.is_empty(), "must receive JSONL events");
    let has_completed = events
        .iter()
        .any(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("turn.completed"));
    assert!(has_completed, "must have turn.completed");
}
