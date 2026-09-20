//! Agent harness runner: executes a role's command with the prompt piped via stdin,
//! per-arg placeholder substitution, an optional JSON schema flag, and a hard timeout
//! (poll + kill; no async runtime in the CLI path).

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

/// Run a role with `prompt` on stdin; return raw stdout. Kills the child on timeout.
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

    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "agent harness `{program}` timed out after {}s and was killed",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
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
    // claude/agy JSON envelopes wrap the textual answer in a `result` field. Models often
    // pad it with prose before a fenced block, so probe any `result` that contains an
    // object and fall back to the envelope itself when there is nothing parseable inside.
    if let Some(text) = v.get("result").and_then(Value::as_str)
        && text.contains('{')
        && let Ok(inner) = extract_json_object_inner(text.trim())
    {
        return Ok(inner);
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
        let r = role(&["agy", "-p", "--model", "{model}", "--effort", "{effort}"]);
        let args = build_args(&r, None).expect("args");
        assert_eq!(args, vec!["agy", "-p", "--model", "m1", "--effort", "low"]);
    }

    #[test]
    fn appends_schema_flag_when_configured() {
        let mut r = role(&["agy", "-p"]);
        r.json_schema = true;
        let args = build_args(&r, Some(std::path::Path::new("/tmp/s.json"))).expect("args");
        assert_eq!(args.last().unwrap(), "/tmp/s.json");
        assert_eq!(args[args.len() - 2], "--json-schema");
        // no duplicate when the template already references {schema}
        let mut r2 = role(&["agy", "-p", "--json-schema", "{schema}"]);
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
}
