//! The fence on `dmem upgrade` for an install that a host installer manages.
//!
//! `dmem upgrade` jumps to the newest release whatever versions the installer pinned, and it
//! replaces the binary in place. On a managed install both are wrong: the installer owns the
//! version set and its rollback. So `dmem upgrade` refuses there, exits 3, and names the
//! installer's own command.
//!
//! An install is managed when either of two things says so:
//!
//! - the marker file [`MARKER`] sits in the directory of the real `dmem` binary (the path
//!   after every symlink is resolved). The installer writes it root-owned beside a root-owned
//!   binary, so it holds in any shell and for any user who cannot write there. **This is the
//!   fence that holds.**
//! - the environment variable [`ENV`] is set to `1`, `true`, `yes` or `on` (any case, spaces
//!   around it ignored). It reaches only the command it is set for: a service unit's
//!   `Environment=` line does not put it into an operator's shell, so it is a convenience
//!   and not the fence.
//!
//! The fence fails closed. When the program's own path cannot be found or resolved, or the
//! marker lookup fails with anything but "not found", the answer is a refusal, because an
//! upgrade that cannot tell is the one that must not happen. A marker that is a dangling
//! symlink, or a directory, counts as present.
//!
//! This module uses the standard library only, so its logic can be tested on its own.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The marker file an installer puts beside the real `dmem` binary. The hub uses the same name
/// in its install directory.
pub const MARKER: &str = ".daimon-managed";

/// The environment variable that marks one command as managed (see the module notes).
pub const ENV: &str = "DM_MANAGED";

/// The exit status of a refused upgrade, the one `daimon-hub update` uses for a refusal. Any
/// other failure of `dmem upgrade` exits 1.
pub const EXIT_REFUSED: i32 = 3;

/// Why `dmem upgrade` refuses before it does anything. It is a typed error so that `main` can
/// tell a refusal (exit 3) from a failure (exit 1).
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The install says it is managed; `why` names what said so.
    Managed(String),
    /// Whether it is managed could not be settled; `why` names what failed.
    Unknown(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Managed(why) => write!(
                f,
                "this install is managed by the daimon Expert installer ({why}): upgrade it with \
                 `daimon-expert upgrade`, not with `dmem upgrade`"
            ),
            Refused::Unknown(why) => write!(
                f,
                "cannot tell whether this install is managed by the daimon Expert installer \
                 ({why}), so it will not upgrade in place; on a managed install use \
                 `daimon-expert upgrade`"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// The fence: `Ok` only for an install nothing says is managed. `dmem upgrade` calls it before
/// anything else, so a refused upgrade makes no request and writes no file.
pub fn check() -> Result<(), Refused> {
    let env = std::env::var_os(ENV).and_then(|v| v.into_string().ok());
    decide(std::env::current_exe(), env.as_deref(), &|p| std::fs::symlink_metadata(p).map(|_| ()))
}

/// The decision, with its three inputs passed in. `exe` is what `std::env::current_exe` gave,
/// `env` the value of [`ENV`] and `probe` the look for a file: `Ok` when something is there
/// (also a symlink that points nowhere), an error of kind `NotFound` when nothing is.
pub fn decide(exe: io::Result<PathBuf>, env: Option<&str>, probe: &dyn Fn(&Path) -> io::Result<()>) -> Result<(), Refused> {
    if env.is_some_and(env_says_managed) {
        return Err(Refused::Managed(format!("{ENV} is set")));
    }
    let exe = exe.map_err(|e| Refused::Unknown(format!("cannot find the path of this program: {e}")))?;
    // The directory of the real file: a symlink to `dmem` elsewhere must not hide the marker,
    // and a marker beside a symlink is not the installer's.
    let real = std::fs::canonicalize(&exe).map_err(|e| Refused::Unknown(format!("cannot resolve {}: {e}", exe.display())))?;
    let dir = real.parent().ok_or_else(|| Refused::Unknown(format!("{} has no directory", real.display())))?;
    let marker = dir.join(MARKER);
    match probe(&marker) {
        Ok(()) => Err(Refused::Managed(format!("{MARKER} is in {}", dir.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Refused::Unknown(format!("cannot look for {}: {e}", marker.display()))),
    }
}

/// The spellings of "yes" that [`ENV`] accepts, as the hub's `DAIMON_HUB_MANAGED` does.
fn env_says_managed(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory, removed (and made writable again if a test closed it) on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let d = std::env::temp_dir().join(format!("dmfence-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            // the temporary directory itself may be a symlink (macOS): work in its real path
            Scratch(std::fs::canonicalize(&d).unwrap())
        }
        fn dir(&self, name: &str) -> PathBuf {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(rd) = std::fs::read_dir(&self.0) {
                    for e in rd.flatten() {
                        let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o755));
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn real_probe(p: &Path) -> io::Result<()> {
        std::fs::symlink_metadata(p).map(|_| ())
    }

    /// `decide` over a real file system with the real probe.
    fn run(exe: &Path, env: Option<&str>) -> Result<(), Refused> {
        decide(Ok(exe.to_path_buf()), env, &real_probe)
    }

    fn install(s: &Scratch, dir: &str) -> PathBuf {
        let exe = s.dir(dir).join("dmem");
        std::fs::write(&exe, b"not a real binary").unwrap();
        exe
    }

    #[test]
    fn an_install_with_no_marker_and_no_variable_is_not_fenced() {
        let s = Scratch::new();
        let exe = install(&s, "plain");
        assert_eq!(run(&exe, None), Ok(()));
    }

    #[test]
    fn the_marker_beside_the_binary_refuses() {
        let s = Scratch::new();
        let exe = install(&s, "managed");
        std::fs::write(exe.parent().unwrap().join(MARKER), b"").unwrap();
        let err = run(&exe, None).unwrap_err();
        assert!(matches!(err, Refused::Managed(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("daimon-expert upgrade"), "{text}");
        assert!(text.contains(MARKER), "{text}");
    }

    #[test]
    fn the_marker_of_another_directory_does_not_count() {
        let s = Scratch::new();
        let exe = install(&s, "plain");
        std::fs::write(s.dir("elsewhere").join(MARKER), b"").unwrap();
        assert_eq!(run(&exe, None), Ok(()));
    }

    #[test]
    fn the_variable_refuses_in_every_accepted_spelling() {
        let s = Scratch::new();
        let exe = install(&s, "plain");
        for v in ["1", "true", "TRUE", "True", "yes", "YES", "on", "On", " 1 ", "\ttrue\n", "  yes"] {
            let err = run(&exe, Some(v)).unwrap_err();
            assert!(matches!(err, Refused::Managed(_)), "{v:?}: {err:?}");
            assert!(err.to_string().contains(ENV), "{v:?}: {err}");
        }
    }

    #[test]
    fn other_values_of_the_variable_do_not_fence() {
        let s = Scratch::new();
        let exe = install(&s, "plain");
        for v in ["", " ", "0", "false", "no", "off", "2", "enabled", "y", "t", "truee", "yes please"] {
            assert_eq!(run(&exe, Some(v)), Ok(()), "{v:?}");
        }
    }

    #[test]
    fn the_variable_alone_decides_without_looking_at_the_file_system() {
        // the program's own path is not even needed
        let broken = io::Error::other("no path");
        assert!(matches!(decide(Err(broken), Some("1"), &|_| panic!("probed")), Err(Refused::Managed(_))));
    }

    #[test]
    fn a_binary_reached_through_a_symlink_is_judged_by_the_real_directory() {
        #[cfg(unix)]
        {
            let s = Scratch::new();
            let real = install(&s, "real");
            let link_dir = s.dir("link");
            let link = link_dir.join("dmem");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            // no marker anywhere: not fenced, through the link or not
            assert_eq!(run(&link, None), Ok(()));
            // the marker beside the real file fences the link
            std::fs::write(real.parent().unwrap().join(MARKER), b"").unwrap();
            assert!(matches!(run(&link, None), Err(Refused::Managed(_))));
            // and a marker beside the link only does not fence a plain install behind it
            let s2 = Scratch::new();
            let real2 = install(&s2, "real");
            let link2 = s2.dir("link").join("dmem");
            std::os::unix::fs::symlink(&real2, &link2).unwrap();
            std::fs::write(link2.parent().unwrap().join(MARKER), b"").unwrap();
            assert_eq!(run(&link2, None), Ok(()));
        }
    }

    #[test]
    fn a_symlink_marker_that_points_nowhere_still_counts() {
        #[cfg(unix)]
        {
            let s = Scratch::new();
            let exe = install(&s, "managed");
            let marker = exe.parent().unwrap().join(MARKER);
            std::os::unix::fs::symlink(s.0.join("no-such-target"), &marker).unwrap();
            assert!(!marker.exists(), "the link must dangle for this test");
            assert!(matches!(run(&exe, None), Err(Refused::Managed(_))));
        }
    }

    #[test]
    fn a_marker_that_is_a_directory_still_counts() {
        let s = Scratch::new();
        let exe = install(&s, "managed");
        std::fs::create_dir(exe.parent().unwrap().join(MARKER)).unwrap();
        assert!(matches!(run(&exe, None), Err(Refused::Managed(_))));
    }

    #[test]
    fn a_program_path_that_cannot_be_found_or_resolved_refuses() {
        let err = decide(Err(io::Error::other("no path")), None, &real_probe).unwrap_err();
        assert!(matches!(err, Refused::Unknown(_)), "{err:?}");
        let s = Scratch::new();
        let gone = s.dir("plain").join("dmem-that-is-not-there");
        let err = run(&gone, None).unwrap_err();
        assert!(matches!(err, Refused::Unknown(_)), "{err:?}");
        // a bare name has no directory to look in and does not resolve either
        let err = decide(Ok(PathBuf::from("dmem-not-in-the-working-directory")), None, &real_probe).unwrap_err();
        assert!(matches!(err, Refused::Unknown(_)), "{err:?}");
    }

    #[test]
    fn a_marker_lookup_that_fails_other_than_not_found_refuses() {
        let s = Scratch::new();
        let exe = install(&s, "plain");
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other, io::ErrorKind::InvalidData, io::ErrorKind::TimedOut] {
            let err = decide(Ok(exe.clone()), None, &|_| Err(io::Error::new(kind, "lookup failed"))).unwrap_err();
            assert!(matches!(err, Refused::Unknown(_)), "{kind:?}: {err:?}");
            assert!(err.to_string().contains("lookup failed"), "{err}");
        }
        // only "not found" means "no marker"
        assert_eq!(decide(Ok(exe), None, &|_| Err(io::Error::new(io::ErrorKind::NotFound, "gone"))), Ok(()));
    }

    #[test]
    fn a_directory_the_user_cannot_search_refuses_and_does_not_pass() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let s = Scratch::new();
            let exe = install(&s, "closed");
            let dir = exe.parent().unwrap().to_path_buf();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_dir(&dir).is_ok() {
                // a privileged user is not stopped by the mode: nothing to show here
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
                return;
            }
            let got = run(&exe, None);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(got, Err(Refused::Unknown(_))), "{got:?}");
        }
    }

    #[test]
    fn the_refusals_say_what_to_run_instead() {
        assert!(Refused::Managed("x".into()).to_string().contains("`daimon-expert upgrade`"));
        assert!(Refused::Unknown("x".into()).to_string().contains("`daimon-expert upgrade`"));
        assert_eq!(EXIT_REFUSED, 3);
    }

    // The installer's author reads the page below; it must keep naming what the code checks.
    #[test]
    fn the_page_for_installer_authors_names_what_the_code_checks() {
        let page = include_str!("../docs/managed-install.md");
        for needle in [MARKER, ENV, "daimon-expert upgrade", "exit status 3", "beside the real binary", "root-owned"] {
            assert!(page.contains(needle), "docs/managed-install.md does not mention {needle:?}");
        }
        for spelling in ["`1`", "`true`", "`yes`", "`on`"] {
            assert!(page.contains(spelling), "docs/managed-install.md does not list {spelling}");
        }
    }
}
