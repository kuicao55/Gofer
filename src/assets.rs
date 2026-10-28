//! Embedded runtime assets.
//!
//! Release binaries are self-contained: the notification scripts, demo/lock
//! bookers and the panel's static files are compiled into the binary and
//! materialised into the state directory at startup. A panel update therefore
//! only has to replace the binary; the next start writes the matching assets.
//!
//! `GOFER_SCRIPTS_DIR` / `GOFER_WEB_DIR` remain supported as explicit overrides
//! (useful for development or custom scripts).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SCRIPTS: &[(&str, &[u8])] = &[
    (
        "wechat-notify-fast.py",
        include_bytes!("../scripts/wechat-notify-fast.py"),
    ),
    ("xhs-notify.py", include_bytes!("../scripts/xhs-notify.py")),
    (
        "gofer-seat-demo.py",
        include_bytes!("../scripts/gofer-seat-demo.py"),
    ),
    (
        "gofer-booker.py",
        include_bytes!("../scripts/gofer-booker.py"),
    ),
    (
        "THIRD_PARTY_NOTICES.md",
        include_bytes!("../scripts/THIRD_PARTY_NOTICES.md"),
    ),
];

const WEB: &[(&str, &[u8])] = &[
    ("css/app.css", include_bytes!("../web/css/app.css")),
    ("js/htmx.min.js", include_bytes!("../web/js/htmx.min.js")),
    ("favicon.svg", include_bytes!("../web/favicon.svg")),
];

/// Materialise embedded assets and point the resolver functions at them unless
/// the caller provided explicit overrides.
pub fn materialize() -> Result<()> {
    if std::env::var_os("GOFER_SCRIPTS_DIR").is_some()
        && std::env::var_os("GOFER_WEB_DIR").is_some()
    {
        return Ok(());
    }

    let base = state_runtime_dir();
    let (scripts, web) = materialize_into(&base)?;
    if std::env::var_os("GOFER_SCRIPTS_DIR").is_none() {
        std::env::set_var("GOFER_SCRIPTS_DIR", &scripts);
    }
    if std::env::var_os("GOFER_WEB_DIR").is_none() {
        std::env::set_var("GOFER_WEB_DIR", &web);
    }
    tracing::debug!("embedded assets materialized at {}", base.display());
    Ok(())
}

/// Write all embedded files under `base/scripts` and `base/web`.
pub fn materialize_into(base: &Path) -> Result<(PathBuf, PathBuf)> {
    let scripts = base.join("scripts");
    let web = base.join("web");
    write_files(&scripts, SCRIPTS, 0o755)?;
    write_files(&web, WEB, 0o644)?;
    Ok((scripts, web))
}

fn write_files(base: &Path, files: &[(&str, &[u8])], mode: u32) -> Result<()> {
    for (name, bytes) in files {
        let path = base.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建内置资源目录失败: {}", parent.display()))?;
        }
        let needs_write = match std::fs::read(&path) {
            Ok(existing) => existing != *bytes,
            Err(_) => true,
        };
        if needs_write {
            std::fs::write(&path, bytes)
                .with_context(|| format!("写入内置资源失败: {}", path.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

fn state_runtime_dir() -> PathBuf {
    crate::server::default_state_dir()
        .join("runtime")
        .join(format!("v{}", crate::server::VERSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_all_embedded_files() {
        let dir = tempfile::tempdir().unwrap();
        let (scripts, web) = materialize_into(dir.path()).unwrap();
        for (name, bytes) in SCRIPTS {
            let path = scripts.join(name);
            assert!(path.is_file(), "missing script {name}");
            assert_eq!(std::fs::read(&path).unwrap(), *bytes);
        }
        for (name, bytes) in WEB {
            let path = web.join(name);
            assert!(path.is_file(), "missing web asset {name}");
            assert_eq!(std::fs::read(&path).unwrap(), *bytes);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(scripts.join("gofer-seat-demo.py"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
        }
    }
}
