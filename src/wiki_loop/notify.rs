//! Notification dispatch via the herdr CLI (`herdr notification show <title> [--body <body>]`).

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

pub fn notify(title: &str, body: Option<&str>) -> Result<bool> {
    let Some(herdr) = which_bin("herdr") else {
        return Ok(false);
    };
    let mut cmd = Command::new(&herdr);
    cmd.args(["notification", "show", title]);
    if let Some(body) = body {
        cmd.args(["--body", body]);
    }
    let output = cmd.output()?;
    Ok(output.status.success())
}

/// `which` without adding a dependency: scan PATH (mirrors `src/herdr.rs` subprocess style).
pub fn which_bin(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.exists().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    #[test]
    fn which_finds_known_binary_or_fails_cleanly() {
        assert!(super::which_bin("sh").is_some(), "sh is always on PATH");
        assert!(super::which_bin("definitely-not-a-binary-xyz").is_none());
    }
}
