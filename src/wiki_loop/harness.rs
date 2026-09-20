//! Agent harness runner: executes a role's command with the prompt piped via stdin,
//! per-arg placeholder substitution, an optional JSON schema flag, and a hard timeout
//! (poll + kill; no async runtime in the CLI path). Children run in their own process
//! group so a timeout kills the wrapper CLI plus any grandchildren it spawned.

use super::config::RoleConfig;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Build the final argv for a role, substituting `{model}` / `{effort}` / `{schema}`.
pub fn build_args(role: &RoleConfig, schema_path: Option<&std::path::Path>) -> Result<Vec<String>> {
    if role.command.is_empty() {
        bail!("role command is empty; set [roles.*].command in wiki-loop.toml");
    }
    let schema_str = schema_path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut args: Vec<String> = role
        .command
        .iter()
        .map(|arg| {
            arg.replace("{model}", &role.model)
                .replace("{effort}", &role.effort)
                .replace("{schema}", &schema_str)
        })
        .collect();
    // Schema enforcement when the template didn't carry a {schema} placeholder.
    if role.json_schema
        && let Some(schema_path) = schema_path
    {
        let schema_arg = schema_path.to_string_lossy().into_owned();
        if !args.iter().any(|a| a == &schema_arg) {
            args.push("--json-schema".into());
            args.push(schema_arg);
        }
    }
    Ok(args)
}

/// Run a role and extract a structured JSON verdict, retrying once when the output has
/// the right shape problem but the wrong content — the observed failure mode is a
/// frontier model drifting into prose (schema enforcement is best-effort in the
/// harness), which a single re-ask reliably absorbs. Harness errors (spawn, timeout,
/// non-zero exit) are not retried: those are systematic, and retrying a timeout just
/// doubles the stall. The final error names the envelope status and keys so the
/// circuit breaker's message is diagnosable from `status` alone.
pub fn run_role_structured(
    role: &RoleConfig,
    prompt: &str,
    schema_path: Option<&std::path::Path>,
    timeout: Duration,
) -> Result<Value> {
    let mut last_err = None;
    for _ in 0..2 {
        let raw = run_role(role, prompt, schema_path, timeout)?;
        match extract_json_object(&raw) {
            Ok(v) if !is_leaked_envelope(&v) => return Ok(v),
            Ok(v) => {
                // Envelope fell through unwrapped: the model's answer held no JSON.
                let status = v.get("status").and_then(Value::as_str).unwrap_or("unknown");
                last_err = Some(anyhow::anyhow!(
                    "model output carried no JSON (harness status `{status}`, keys {:?})",
                    v.as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                ));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// An unwrapped `agy`/`claude` envelope: `extract_json_object` returned the envelope
/// itself because the answer field held no parseable object. Callers expecting a domain
/// schema would fail on it anyway — better to name and retry the shape problem.
fn is_leaked_envelope(v: &Value) -> bool {
    v.get("conversation_id").is_some() && v.get("response").is_some()
        || v.get("type").is_some() && v.get("result").is_some()
}

/// Run a role with `prompt` on stdin; return raw stdout. On timeout (or wait failure)
/// kills the child's whole process group — wrapper CLIs spawn grandchildren that must
/// not outlive the kill.
/// stdin is written and stdout/stderr are drained on background threads so a chatty
/// child can never fill a pipe and deadlock while we poll for completion.
pub fn run_role(
    role: &RoleConfig,
    prompt: &str,
    schema_path: Option<&std::path::Path>,
    timeout: Duration,
) -> Result<String> {
    let args = build_args(role, schema_path)?;
    let (program, rest) = args.split_first().expect("non-empty argv checked above");

    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning agent harness `{program}`"))?;

    // stdin: write prompt then drop (EOF) from a thread so large prompts never block.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(prompt.as_bytes());
            let _ = stdin.flush();
        });
    }
    // stdout/stderr: drain on threads to own buffers without deadlock.
    let stdout_handle = child.stdout.take().map(drain_to_buffer);
    let stderr_handle = child.stderr.take().map(drain_to_buffer);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(&mut child);
                    bail!(
                        "agent harness `{program}` timed out after {}s and was killed",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                kill_process_group(&mut child);
                return Err(e).context("waiting for agent harness");
            }
        }
    };

    let stdout_bytes = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr_bytes = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!(
            "agent harness `{program}` exited with {status}: {}",
            stderr.trim().chars().take(500).collect::<String>()
        );
    }
    Ok(stdout)
}

/// Kill the child and everything it spawned. `agy`/`claude` are wrapper CLIs that run
/// their own subprocesses, so SIGKILLing only the direct child would orphan those
/// grandchildren (and the memory they hold) on this swapless box. The child is spawned
/// as its own process-group leader, so a negative-pid kill covers the leader and every
/// group member; the wait reaps the leader. Best-effort: errors are ignored.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

fn drain_to_buffer<T: std::io::Read + Send + 'static>(
    mut pipe: T,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
        buf
    })
}

/// Extract a JSON object from model output. Handles: bare JSON, `claude --output-format json`
/// envelopes (`{"result": "..."}`), markdown fences, and surrounding prose.
pub fn extract_json_object(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("model emitted empty output");
    }
    // 1) Whole output is JSON.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return unwrap_envelope(v);
    }
    // 2) Fenced block.
    if let Some(idx) = trimmed.find("```") {
        let after = &trimmed[idx + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Ok(v) = serde_json::from_str::<Value>(after[..end].trim())
        {
            return unwrap_envelope(v);
        }
    }
    // 3) First `{` to last `}`.
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start < end
        && let Ok(v) = serde_json::from_str::<Value>(&trimmed[start..=end])
    {
        return unwrap_envelope(v);
    }
    bail!(
        "could not parse JSON from model output: {}",
        trimmed.chars().take(300).collect::<String>()
    )
}

fn unwrap_envelope(v: Value) -> Result<Value> {
    // claude/agy JSON envelopes wrap the textual answer in a `result` (claude) or
    // `response` (agy) field. Models often pad it with prose before a fenced block, so
    // probe any answer field that contains an object and fall back to the envelope
    // itself when there is nothing parseable inside.
    for key in ["result", "response"] {
        if let Some(text) = v.get(key).and_then(Value::as_str)
            && text.contains('{')
            && let Ok(inner) = extract_json_object_inner(text.trim())
        {
            return Ok(inner);
        }
    }
    Ok(v)
}

fn extract_json_object_inner(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    if let Some(idx) = trimmed.find("```") {
        let after = &trimmed[idx + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Ok(v) = serde_json::from_str::<Value>(after[..end].trim())
        {
            return Ok(v);
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start < end
        && let Ok(v) = serde_json::from_str::<Value>(&trimmed[start..=end])
    {
        return Ok(v);
    }
    bail!("no JSON object in inner result")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(args: &[&str]) -> RoleConfig {
        RoleConfig {
            command: args.iter().map(|s| s.to_string()).collect(),
            model: "m1".into(),
            effort: "low".into(),
            json_schema: false,
        }
    }

    #[test]
    fn substitutes_placeholders() {
        // No `-p` in the default agy argv (this agy build treats it as taking an
        // argument); stdin piping engages print mode.
        let r = role(&["agy", "--model", "{model}", "--effort", "{effort}"]);
        let args = build_args(&r, None).expect("args");
        assert_eq!(args, vec!["agy", "--model", "m1", "--effort", "low"]);
    }

    #[test]
    fn appends_schema_flag_when_configured() {
        let mut r = role(&["agy"]);
        r.json_schema = true;
        let args = build_args(&r, Some(std::path::Path::new("/tmp/s.json"))).expect("args");
        assert_eq!(args.last().unwrap(), "/tmp/s.json");
        assert_eq!(args[args.len() - 2], "--json-schema");
        // no duplicate when the template already references {schema}
        let mut r2 = role(&["agy", "--json-schema", "{schema}"]);
        r2.json_schema = true;
        let args2 = build_args(&r2, Some(std::path::Path::new("/tmp/s.json"))).expect("args");
        assert_eq!(args2.iter().filter(|a| *a == "/tmp/s.json").count(), 1);
    }

    #[test]
    fn extracts_bare_fenced_and_enveloped_json() {
        let v = extract_json_object("{\"patterns\": []}").expect("bare");
        assert!(v.get("patterns").is_some());

        let v = extract_json_object("```json\n{\"a\": 1}\n```").expect("fenced");
        assert_eq!(v["a"], 1);

        let envelope = r#"{"type":"result","result":"here you go:\n```json\n{\"b\": 2}\n```"}"#;
        let v = extract_json_object(envelope).expect("envelope");
        assert_eq!(v["b"], 2);

        // agy's envelope wraps the answer in `response`.
        let agy_envelope =
            r#"{"type":"response","response":"sure:\n{\"patterns\": [{\"action\": \"create\"}]}"}"#;
        let v = extract_json_object(agy_envelope).expect("agy envelope");
        assert!(v.get("patterns").is_some());

        let v = extract_json_object("prose before {\"c\": 3} prose after").expect("prose");
        assert_eq!(v["c"], 3);

        assert!(extract_json_object("totally not json").is_err());
    }

    #[test]
    fn runs_stub_harness_with_timeout() {
        let r = role(&["sh", "-c", "cat > /dev/null; echo '{\"ok\": true}'"]);
        let out = run_role(&r, "hello stdin", None, Duration::from_secs(30)).expect("run");
        assert!(out.contains("\"ok\""));

        let slow = role(&["sh", "-c", "sleep 30"]);
        let start = Instant::now();
        assert!(run_role(&slow, "", None, Duration::from_secs(1)).is_err());
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn structured_output_retries_once_on_envelope_drift() {
        // First call: agy envelope whose response is prose (the observed drift mode).
        // Second call: clean domain JSON. One retry must absorb it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("drift");
        std::fs::write(&marker, b"1").expect("marker");
        let script = format!(
            "cat > /dev/null; if [ -f {m} ]; then rm {m}; printf '%s' '{drift}'; else printf '%s' '{ok}'; fi",
            m = marker.display(),
            drift = r#"{"conversation_id":"abc","status":"SUCCESS","response":"I shall summarize these patterns in prose instead.","duration_seconds":1.0}"#,
            ok = r#"{"patterns": []}"#
        );
        let r = role(&["sh", "-c", &script]);
        let v =
            run_role_structured(&r, "prompt", None, Duration::from_secs(30)).expect("structured");
        assert!(v.get("patterns").is_some());
    }

    #[test]
    fn structured_output_fails_naming_status_after_persistent_drift() {
        let script = format!(
            "cat > /dev/null; printf '%s' '{drift}'",
            drift = r#"{"conversation_id":"abc","status":"MAX_TURNS","response":"ran out of turns","num_turns":8}"#
        );
        let r = role(&["sh", "-c", &script]);
        let err =
            run_role_structured(&r, "prompt", None, Duration::from_secs(30)).expect_err("fails");
        let msg = format!("{err:#}");
        assert!(msg.contains("no JSON"), "{msg}");
        assert!(msg.contains("MAX_TURNS"), "{msg}");
    }

    #[test]
    fn envelope_leak_detection_covers_agy_and_claude_shapes() {
        let agy = serde_json::json!({"conversation_id": "x", "response": "prose"});
        let claude = serde_json::json!({"type": "result", "result": "prose"});
        let domain = serde_json::json!({"patterns": []});
        // A claude envelope whose result legitimately IS the verdict object unwraps
        // before this check ever sees it; what remains is prose-only envelopes.
        assert!(is_leaked_envelope(&agy));
        assert!(is_leaked_envelope(&claude));
        assert!(!is_leaked_envelope(&domain));
    }

    #[test]
    fn timeout_kills_grandchildren() {
        // The role's `sh` spawns a background grandchild whose argv carries a unique
        // marker (dash has no `exec -a`, so the marker rides as the inner shell's $0
        // arg). The process-group kill must take the grandchild down along with the
        // wrapper; killing only the direct child would leave it running.
        let script = "sh -c 'sleep 30; true' wltest_grandchild & echo started; wait";
        let r = role(&["sh", "-c", script]);
        let start = Instant::now();
        assert!(run_role(&r, "", None, Duration::from_secs(1)).is_err());
        assert!(start.elapsed() < Duration::from_secs(10));

        // Poll /proc for up to 3s: no live process may still carry the marker.
        // (Zombies have an empty cmdline, so an unreaped corpse cannot false-positive.)
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let survivors = pids_whose_cmdline_contains("wltest_grandchild");
            if survivors.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "processes survived the group kill: {survivors:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Pids of live processes whose /proc/<pid>/cmdline contains `marker`
    /// (Linux-only in practice; yields nothing when /proc is absent).
    fn pids_whose_cmdline_contains(marker: &str) -> Vec<i32> {
        let marker = marker.as_bytes();
        let mut pids = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return pids;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline"))
                && cmdline.windows(marker.len()).any(|w| w == marker)
            {
                pids.push(pid);
            }
        }
        pids
    }
}
