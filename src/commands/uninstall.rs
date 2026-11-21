use std::io;

use crate::state;

/// `synapse uninstall [package] [--all]`
///
/// Removes packages from Synapse's dedicated Nix profile and state. Store paths
/// remain available through older profile generations until garbage collection.
pub fn run(package: Option<&str>, all: bool) -> io::Result<()> {
    let cfg = state::config_dir();
    // Lock before reading state so target selection and the per-package
    // state::write observe one consistent snapshot.
    let _lock = state::acquire(&cfg).map_err(|e| io::Error::other(e.to_string()))?;
    let mut st = state::read(&cfg)?;

    let targets: Vec<String> = match (package, all) {
        (Some(name), false) => {
            if !st.packages.contains_key(name) {
                eprintln!("error: '{name}' is not installed");
                std::process::exit(1);
            }
            vec![name.to_string()]
        }
        // `--all` with an empty state still reaches shell cleanup below, so a
        // previous run that removed packages but failed cleanup can be retried.
        (None, true) => {
            let mut keys: Vec<_> = st.packages.keys().cloned().collect();
            keys.sort();
            keys
        }
        (Some(_), true) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--all cannot be combined with a package name",
            ));
        }
        // Bare `uninstall` must not implicitly mean --all: the CLI contract
        // requires --all for everything, and silently removing every managed
        // package is a footgun.
        (None, false) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nothing to uninstall: pass a package name or --all",
            ));
        }
    };

    let mut failures = Vec::new();
    if !targets.is_empty() {
        let nix_bin = crate::nix::resolve_bin()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "nix not found"))?;
        let flake_dir = crate::commands::update::locate_flake_dir();

        for name in &targets {
            let version = st
                .packages
                .get(name)
                .map(|r| r.version.clone())
                .unwrap_or_default();
            // The removed element's store path lets a failed state write
            // reinstall the exact closure rather than guessing generations.
            let prior_store_path = st
                .packages
                .get(name)
                .and_then(|record| record.store_path.clone());
            if let Err(error) = crate::commands::update::remove_profile_package(&nix_bin, name) {
                eprintln!("warning: could not remove {name} from the Nix profile: {error}");
                failures.push(format!("{name}: {error}"));
                continue;
            }
            st.remove_package(name);
            if let Err(error) = state::write(&cfg, &st) {
                let rollback = crate::commands::update::undo_profile_remove(
                    &nix_bin,
                    name,
                    prior_store_path.as_deref(),
                    &flake_dir,
                )
                .err();
                return Err(io::Error::new(
                    error.kind(),
                    match rollback {
                        Some(rollback) => {
                            format!("write state: {error}; profile rollback: {rollback}")
                        }
                        None => format!("write state: {error}"),
                    },
                ));
            }
            println!("  {name} {version} removed from the Synapse profile");
            let _ = crate::commands::log::append(&format!(
                "{} uninstalled {name} {version}",
                now_secs()
            ));
        }
    }

    // Only strip shell config once nothing is managed; otherwise the remaining
    // packages lose their PATH entry.
    if st.packages.is_empty() {
        match remove_shell_config() {
            Ok(removed) if !removed.is_empty() => {
                println!("\nRemoved Synapse block from:");
                for path in removed {
                    println!("  {}", path.display());
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("warning: could not clean shell config: {error}");
                failures.push(format!("shell config: {error}"));
            }
        }
    }

    println!("\nOlder profile generations retain rollback paths.");
    println!("Run `nix-collect-garbage -d` to reclaim them.");

    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "package uninstall failed: {}",
            failures.join("; ")
        )))
    }
}

/// Strip the managed marker block from every configured shell rc.
///
/// Returns the rc files that were actually modified.
fn remove_shell_config() -> io::Result<Vec<std::path::PathBuf>> {
    use crate::shell;

    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return Ok(Vec::new()),
    };

    let mut removed = Vec::new();
    for sh in shell::detect_shells(&home, std::env::var("SHELL").ok().as_deref()) {
        let rc = sh.rc_path(&home);
        let content = match std::fs::read_to_string(&rc) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };

        let stripped = shell::strip_block(&content);
        if stripped == content {
            continue;
        }

        // Write through to the resolved target: `read_to_string` follows a
        // symlinked rc, so renaming over `rc` itself would replace the link
        // with a regular file and leave the managed block in the dotfile
        // target. Canonicalizing keeps dotfile-manager setups intact.
        let target = std::fs::canonicalize(&rc).unwrap_or_else(|_| rc.clone());
        let tmp = target.with_extension("synapse-tmp");
        std::fs::write(&tmp, stripped.as_bytes())?;
        // Preserve the rc file's own permissions — a plain write+rename would
        // widen a private 0600 file to the umask default.
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, metadata.permissions());
        }
        std::fs::rename(&tmp, &target)?;
        removed.push(rc);
    }

    Ok(removed)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
