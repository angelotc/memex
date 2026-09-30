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
    if bin.is_empty() {
        return None;
    }
    if bin.contains('/') || PathBuf::from(bin).is_absolute() {
        let p = PathBuf::from(bin);
        return is_executable_file(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn which_finds_known_binary_or_fails_cleanly() {
        assert!(super::which_bin("sh").is_some(), "sh is always on PATH");
        assert!(super::which_bin("definitely-not-a-binary-xyz").is_none());
        assert!(super::which_bin("").is_none());
    }

    #[test]
    fn which_requires_an_executable_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let executable = tmp.path().join("executable");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write executable");
        let non_executable = tmp.path().join("non-executable");
        fs::write(&non_executable, "not executable").expect("write non-executable");
        let directory = tmp.path().join("directory");
        fs::create_dir(&directory).expect("create directory");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("make executable");
            fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644))
                .expect("make non-executable");
            assert_eq!(
                super::which_bin(executable.to_str().unwrap()),
                Some(executable)
            );
            assert!(super::which_bin(non_executable.to_str().unwrap()).is_none());
        }
        #[cfg(not(unix))]
        {
            assert_eq!(
                super::which_bin(executable.to_str().unwrap()),
                Some(executable)
            );
            assert_eq!(
                super::which_bin(non_executable.to_str().unwrap()),
                Some(non_executable)
            );
        }
        assert!(super::which_bin(directory.to_str().unwrap()).is_none());
    }
}
