use std::io;

use crate::nix;
use crate::state;
/// `synapse doctor` — diagnose PATH and profile symlinks.
pub fn run() -> io::Result<()> {
    let mut issues = 0usize;

    // 1. Nix available and supported.
    match nix::detect() {
        nix::NixStatus::Supported(v) => println!("  ✓  nix {v}"),
        nix::NixStatus::TooOld(v) => {
            println!("  ✗  nix {v} is too old (need 2.24+)");
            issues += 1;
        }
        nix::NixStatus::Missing => {
            println!("  ✗  nix not found on PATH");
            issues += 1;
        }
    }

    // 2. Installed binaries reachable on PATH.
    let cfg = state::config_dir();
    let st = state::read(&cfg)?;
    for name in st.packages.keys() {
        let found = which(name);
        if found {
            println!("  ✓  {name} on PATH");
        } else {
            println!("  ✗  {name} not found on PATH (is Nix profile active?)");
            issues += 1;
        }
    }

    // `.lock` is a persistent flock inode — its existence is normal and
    // probing it would require creating it, which violates doctor's
    // lock-free contract. Contention surfaces on the next mutating command.

    // 3. Broken symlinks in Nix profile (best-effort).
    check_nix_profile_symlinks(&mut issues);

    if issues == 0 {
        println!("\nAll checks passed.");
    } else {
        println!("\n{issues} issue(s) found.");
    }

    Ok(())
}

fn which(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn check_nix_profile_symlinks(issues: &mut usize) {
    // Synapse installs into its own profile (honours SYNAPSE_PROFILE), not
    // ~/.nix-profile — broken links there are what matter for synapse runs.
    let Ok(profile) = crate::commands::update::synapse_profile() else {
        return;
    };
    let profile_bin = profile.join("bin");
    if !profile_bin.exists() {
        return; // Nix profile not present — not necessarily an error here
    }
    let rd = match std::fs::read_dir(&profile_bin) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(_) => {}
                Err(_) => {
                    println!("  ✗  broken symlink: {}", path.display());
                    *issues += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_binary() {
        assert!(
            which("true"),
            "`which`/`sh` must be available in the build/test environment"
        );
    }

    #[test]
    fn which_misses_nonexistent() {
        assert!(!which("synapse-totally-absent-binary-xyzzy"));
    }
}
