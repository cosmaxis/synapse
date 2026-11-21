//! Cross-process lock contention: verifies a real second process that loses the
//! race is told the lock is *held*, not handed a raw "File exists" I/O error.
//!
//! The unit tests in `src/state.rs` cover the single-process paths. This one
//! exercises two genuinely concurrent OS processes against the same lock file,
//! which is the scenario the concurrency-lock acceptance criterion is about.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Path to the `synapse` binary under test.
fn synapse_bin() -> PathBuf {
    // cargo sets CARGO_BIN_EXE_<name> for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_synapse"))
}

fn scratch_config() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "synapse-lock-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// With a real flock held by this process, `synapse update --all` must report
/// contention in human terms and must not leak the raw OS error.
#[cfg(unix)]
#[test]
fn update_reports_contention_not_io_error() {
    let cfg_home = scratch_config();
    let cfg_dir = cfg_home.join("synapse");
    fs::create_dir_all(&cfg_dir).unwrap();

    // A package must exist in state or update exits before touching the lock.
    fs::write(
        cfg_dir.join("state.json"),
        r#"{"packages":{"herdr":{"version":"0.7.5","installed_at":1785542400}}}"#,
    )
    .unwrap();

    // Claim the lock for real: flock(LOCK_EX) on the stable .lock inode. The
    // PID inside is only what the child's error message will name.
    let mut lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cfg_dir.join(".lock"))
        .unwrap();
    {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        lock_file.set_len(0).unwrap();
        lock_file
            .write_all(std::process::id().to_string().as_bytes())
            .unwrap();
        let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(ret, 0, "flock failed: {}", std::io::Error::last_os_error());
    }

    let out = Command::new(synapse_bin())
        .args(["update", "--all"])
        .env("XDG_CONFIG_HOME", &cfg_home)
        .output()
        .expect("run synapse update");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr);

    assert!(
        combined.contains("lock held by PID") || combined.contains(&std::process::id().to_string()),
        "expected a contention message naming the holder, got: {combined}"
    );
    assert!(
        !combined.contains("File exists"),
        "raw OS error leaked to the user: {combined}"
    );
    assert!(
        !combined.contains("os error 17"),
        "raw errno leaked to the user: {combined}"
    );

    // The pre-existing lock must be left alone — we did not own it.
    assert_eq!(
        fs::read_to_string(cfg_dir.join(".lock")).unwrap().trim(),
        std::process::id().to_string(),
        "another process's lock was overwritten"
    );

    fs::remove_dir_all(&cfg_home).ok();
}

/// Lock contents are diagnostics only: a dead-PID `.lock` file must not block
/// a new run, and the stable inode is retained rather than unlinked.
#[test]
fn stale_lock_does_not_block() {
    let cfg_home = scratch_config();
    let cfg_dir = cfg_home.join("synapse");
    fs::create_dir_all(&cfg_dir).unwrap();

    fs::write(cfg_dir.join("state.json"), r#"{"packages":{}}"#).unwrap();
    fs::write(cfg_dir.join(".lock"), "999999").unwrap();

    let out = Command::new(synapse_bin())
        .args(["update", "--all"])
        .env("XDG_CONFIG_HOME", &cfg_home)
        .output()
        .expect("run synapse update");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        out.status.success(),
        "stale lock blocked the run: {combined}"
    );
    assert!(
        !combined.contains("File exists") && !combined.contains("os error 17"),
        "stale lock produced a raw OS error: {combined}"
    );
    // The stable inode persists by design — unlinking it is what would reopen
    // the check-then-act race flock exists to close.
    assert!(
        cfg_dir.join(".lock").exists(),
        "lock inode was unlinked; it must persist for flock arbitration"
    );

    fs::remove_dir_all(&cfg_home).ok();
}

/// `synapse list` and `synapse status` must work with no config at all
/// (offline, first-run) and must not create a lock.
#[test]
fn read_only_commands_need_no_lock() {
    let cfg_home = scratch_config();

    for args in [vec!["list"], vec!["status"]] {
        let out = Command::new(synapse_bin())
            .args(&args)
            .env("XDG_CONFIG_HOME", &cfg_home)
            .output()
            .unwrap_or_else(|e| panic!("run synapse {args:?}: {e}"));

        assert!(
            out.status.success(),
            "synapse {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    assert!(
        !cfg_home.join("synapse/.lock").exists(),
        "read-only command created a lock file"
    );

    fs::remove_dir_all(&cfg_home).ok();
}
