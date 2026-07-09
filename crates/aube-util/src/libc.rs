//! Host libc detection (glibc vs musl) shared by native-binding
//! selection (aube-resolver) and Node runtime downloads (aube-runtime).
//!
//! Detection is a *runtime* question, not a compile-time one: aube
//! ships static-musl Linux binaries, so `cfg!(target_env = "musl")`
//! reflects the toolchain that built aube, never the host. And the
//! mere *presence* of musl's loader on disk is not a signal either —
//! Debian/Ubuntu's `musl` package (common for cross-compile dev)
//! drops `/lib/ld-musl-<arch>.so.1` alongside the system glibc
//! loader, which historically caused musl false-positives and
//! unloadable `*-musl` artifacts on glibc hosts.

use std::path::{Path, PathBuf};

/// The host's libc in npm vocabulary: `"glibc"` / `"musl"` on Linux,
/// `""` elsewhere (npm only sets `libc` on Linux packages).
///
/// Authoritative signal is `/proc/self/maps`: the dynamic linker that
/// loaded the running process is always mmap'd into it, so whichever
/// of `ld-musl-*` / `ld-linux-*` appears there is the libc the host
/// actually runs. Static binaries (like aube's own Linux builds) map
/// no loader, so a directory scan of the standard loader locations is
/// kept as a fallback — checking glibc *first* so a dual-loader host
/// still resolves correctly. Cached once per process.
pub fn detect_linux_libc() -> &'static str {
    use std::sync::OnceLock;
    static CACHE: OnceLock<&'static str> = OnceLock::new();
    CACHE.get_or_init(|| {
        if std::env::consts::OS != "linux" {
            return "";
        }
        if let Ok(maps) = std::fs::read_to_string("/proc/self/maps")
            && let Some(libc) = libc_from_maps(&maps)
        {
            return libc;
        }
        let glibc_dirs = [
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/lib/x86_64-linux-gnu"),
            PathBuf::from("/lib/aarch64-linux-gnu"),
        ];
        libc_from_loader_scan(&glibc_dirs, Path::new("/lib")).unwrap_or("glibc")
    })
}

/// Classify libc from the contents of `/proc/self/maps`. `None` when
/// no dynamic loader is mapped (static binary, stripped rootfs).
fn libc_from_maps(maps: &str) -> Option<&'static str> {
    if maps.contains("/ld-musl-") {
        return Some("musl");
    }
    if maps.contains("/ld-linux") {
        return Some("glibc");
    }
    None
}

/// Fallback loader scan for hosts without procfs evidence. Checks
/// `glibc_dirs` for `ld-linux*` before checking `musl_dir` for
/// `ld-musl-*`, so a host with both loaders resolves to glibc.
/// `None` when neither loader is found.
fn libc_from_loader_scan(glibc_dirs: &[PathBuf], musl_dir: &Path) -> Option<&'static str> {
    for dir in glibc_dirs {
        if dir_has_loader_prefix(dir, "ld-linux") {
            return Some("glibc");
        }
    }
    if dir_has_loader_prefix(musl_dir, "ld-musl-") {
        return Some("musl");
    }
    None
}

fn dir_has_loader_prefix(dir: &Path, prefix: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_with_musl_loader_classifies_musl() {
        let maps = "7f0000000000-7f0000001000 r-xp 00000000 08:01 42 /lib/ld-musl-x86_64.so.1\n";
        assert_eq!(libc_from_maps(maps), Some("musl"));
    }

    #[test]
    fn maps_with_glibc_loader_classifies_glibc() {
        let maps =
            "7f0000000000-7f0000001000 r-xp 00000000 08:01 42 /usr/lib64/ld-linux-x86-64.so.2\n";
        assert_eq!(libc_from_maps(maps), Some("glibc"));
    }

    #[test]
    fn maps_without_any_loader_is_inconclusive() {
        // A static binary maps no dynamic loader at all.
        let maps = "7f0000000000-7f0000001000 r-xp 00000000 08:01 42 /usr/bin/aube\n";
        assert_eq!(libc_from_maps(maps), None);
    }

    #[test]
    fn scan_prefers_glibc_when_both_loaders_are_present() {
        let tmp = tempfile::tempdir().unwrap();
        let glibc_dir = tmp.path().join("glibc");
        let musl_dir = tmp.path().join("musl");
        std::fs::create_dir_all(&glibc_dir).unwrap();
        std::fs::create_dir_all(&musl_dir).unwrap();
        std::fs::write(glibc_dir.join("ld-linux-x86-64.so.2"), b"").unwrap();
        std::fs::write(musl_dir.join("ld-musl-x86_64.so.1"), b"").unwrap();
        assert_eq!(
            libc_from_loader_scan(&[glibc_dir], &musl_dir),
            Some("glibc")
        );
    }

    #[test]
    fn scan_reports_musl_when_only_the_musl_loader_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let glibc_dir = tmp.path().join("glibc");
        let musl_dir = tmp.path().join("musl");
        std::fs::create_dir_all(&glibc_dir).unwrap();
        std::fs::create_dir_all(&musl_dir).unwrap();
        std::fs::write(musl_dir.join("ld-musl-aarch64.so.1"), b"").unwrap();
        assert_eq!(libc_from_loader_scan(&[glibc_dir], &musl_dir), Some("musl"));
    }

    #[test]
    fn scan_with_no_loaders_is_inconclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let glibc_dir = tmp.path().join("glibc");
        let musl_dir = tmp.path().join("musl");
        std::fs::create_dir_all(&glibc_dir).unwrap();
        std::fs::create_dir_all(&musl_dir).unwrap();
        assert_eq!(libc_from_loader_scan(&[glibc_dir], &musl_dir), None);
    }

    #[test]
    fn detect_returns_npm_vocabulary_for_this_host() {
        let libc = detect_linux_libc();
        if cfg!(target_os = "linux") {
            assert!(libc == "glibc" || libc == "musl");
        } else {
            assert_eq!(libc, "");
        }
    }
}
