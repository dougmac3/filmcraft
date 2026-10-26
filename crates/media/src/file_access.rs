//! The folders an agent's commands may read and write.
//!
//! A host that lets an agent drive the editor (Fossa Direct's Timeline desk) runs each of its
//! requests inside [`confine`] with an [`AutomationRoots`]. While it is in force, every path a
//! command reads goes through [`check_read`] and every path it writes through [`check_write`]:
//! the file layer checks each file it opens (the engine's `FsServices`, the media readers, the
//! encoders' outputs, `filmcraft_format::atomic_write`), and so do the engine's other direct reads
//! and writes (LUTs, label presets, shortcuts, Project Manager, proxies, settings paths).
//!
//! - The path is made absolute against the working directory, as the file system reads it. Its
//!   links are followed: for a file that doesn't exist yet, the links of its nearest existing
//!   folder, the rest of the path being plain names (`..` below a missing folder is refused, and so
//!   is a link that leads nowhere).
//! - The result must lie inside one of the roots of its kind, compared by whole components
//!   (`/work2` isn't inside `/work`), without regard to case on Windows.
//! - Read and write access are separate: a root given alone grants only its own kind of access.
//!
//! Roots are in force on a thread inside [`confine`], and on a job's thread when the job runs in
//! the scope it was started in ([`ScopeHandle`]). Outside them (what the person at the keyboard
//! does, the app's own folders) nothing is checked.
//!
//! A path is checked, then opened: a process that can change the folders in between (swapping in
//! a link) can still win that race. The roots keep an agent to the files it was given; they don't
//! guard against other programs on the computer.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Read access or write access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// The folders automation may read and write; a kind with no root grants no access of its kind.
#[derive(Debug)]
pub struct AutomationRoots {
    read: Vec<Root>,
    write: Vec<Root>,
}

#[derive(Debug)]
struct Root {
    /// The folder made absolute, links followed: what paths are compared with.
    resolved: PathBuf,
    /// The same without Windows' `\\?\` prefix, for people.
    shown: String,
}

impl Root {
    /// The folder at `path`, which must exist.
    fn open(path: &Path, access: Access) -> Result<Self, String> {
        let kind = match access {
            Access::Read => "read",
            Access::Write => "write",
        };
        let resolved = std::fs::canonicalize(path)
            .and_then(|p| if p.is_dir() { Ok(p) } else { Err(std::io::ErrorKind::NotADirectory.into()) })
            .map_err(|e| format!("cannot open the automation {kind} root `{}`: {e}", path.display()))?;
        let full = resolved.to_string_lossy().into_owned();
        let shown = match full.strip_prefix(r"\\?\UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => full.strip_prefix(r"\\?\").unwrap_or(&full).to_string(),
        };
        Ok(Self { resolved, shown })
    }
}

impl AutomationRoots {
    /// The roots `read` and `write`, which must be existing folders (one given twice counts once).
    /// Neither kind given: an agent may read and write nothing.
    pub fn new(read: &[&Path], write: &[&Path]) -> Result<Arc<Self>, String> {
        let open = |paths: &[&Path], access: Access| -> Result<Vec<Root>, String> {
            let mut roots: Vec<Root> = Vec::with_capacity(paths.len());
            for path in paths {
                let root = Root::open(path, access)?;
                if !roots.iter().any(|r| r.resolved == root.resolved) {
                    roots.push(root);
                }
            }
            Ok(roots)
        };
        Ok(Arc::new(Self { read: open(read, Access::Read)?, write: open(write, Access::Write)? }))
    }

    /// May the file or folder at `path` be read (or written)? It must lie inside one of the roots
    /// of that kind. `Err` says why not.
    pub fn check(&self, path: &str, access: Access) -> Result<(), String> {
        let (roots, kind) = match access {
            Access::Read => (&self.read, "read"),
            Access::Write => (&self.write, "write"),
        };
        if roots.is_empty() {
            return Err(format!("automation filesystem access is not granted: {kind} authority is absent: {path}"));
        }
        let resolved = resolve(path)?;
        if roots.iter().any(|r| inside(&resolved, &r.resolved)) {
            return Ok(());
        }
        let shown: Vec<&str> = roots.iter().map(|r| r.shown.as_str()).collect();
        let plural = if shown.len() == 1 { "root" } else { "roots" };
        Err(format!("automation path rejected: outside the {kind} {plural} {}: {path}", shown.join(", ")))
    }
}

/// Where the roots are in force on a thread ([`confine`]).
#[derive(Clone, Debug)]
enum Scope {
    Confined(Arc<AutomationRoots>),
    Unconfined,
}

thread_local! {
    /// This thread's scope; `None`: nothing is checked.
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Puts the scope it replaced back when dropped (also when `f` panics).
struct Restore(Option<Scope>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        SCOPE.with(|s| *s.borrow_mut() = previous);
    }
}

fn with_scope<T>(scope: Option<Scope>, f: impl FnOnce() -> T) -> T {
    let _restore = Restore(SCOPE.with(|s| s.replace(scope)));
    f()
}

/// Run `f` with `roots` in force on this thread.
pub fn confine<T>(roots: &Arc<AutomationRoots>, f: impl FnOnce() -> T) -> T {
    with_scope(Some(Scope::Confined(roots.clone())), f)
}

/// Run `f` with no roots in force on this thread: what the person at the keyboard does.
pub fn unconfined<T>(f: impl FnOnce() -> T) -> T {
    with_scope(Some(Scope::Unconfined), f)
}

/// This thread's scope, to carry to work it hands to another thread ([`ScopeHandle::run`]).
#[derive(Clone, Debug)]
pub struct ScopeHandle(Option<Scope>);

impl ScopeHandle {
    /// The scope of the calling thread.
    pub fn capture() -> Self {
        Self(SCOPE.with(|s| s.borrow().clone()))
    }

    /// Run `f` in the captured scope.
    pub fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        with_scope(self.0.clone(), f)
    }
}

/// The roots in force on this thread, if any.
pub fn current() -> Option<Arc<AutomationRoots>> {
    match SCOPE.with(|s| s.borrow().clone()) {
        Some(Scope::Confined(r)) => Some(r),
        _ => None,
    }
}

/// May `path` be read here? Always, without roots in force.
pub fn check_read(path: &str) -> Result<(), String> {
    current().map_or(Ok(()), |r| r.check(path, Access::Read))
}

/// May `path` be written here? Always, without roots in force.
pub fn check_write(path: &str) -> Result<(), String> {
    current().map_or(Ok(()), |r| r.check(path, Access::Write))
}

/// [`check_read`] as an I/O error (permission denied), for file operations.
pub fn io_read(path: &str) -> std::io::Result<()> {
    check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
}

/// [`check_write`] as an I/O error (permission denied), for file operations.
pub fn io_write(path: &str) -> std::io::Result<()> {
    check_write(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
}

/// `path` made absolute with its links followed; for a path that doesn't exist (yet), its nearest
/// existing folder's links followed and the rest appended.
fn resolve(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("automation path rejected: path is empty".into());
    }
    // Checked as given: making `...\NUL` absolute gives the device path `\\.\NUL` on Windows.
    if cfg!(windows) && Path::new(path).components().any(|c| matches!(c, Component::Normal(n) if is_device_name(&n.to_string_lossy()))) {
        return Err(format!("automation path rejected: reserved device names are not allowed: {path}"));
    }
    let absolute = std::path::absolute(path).map_err(|e| format!("automation path rejected: cannot resolve {path}: {e}"))?;
    // The names below the nearest existing folder, last first.
    let mut missing: Vec<OsString> = vec![];
    let mut at = absolute.as_path();
    loop {
        match std::fs::canonicalize(at) {
            Ok(mut found) => {
                found.extend(missing.iter().rev());
                return Ok(found);
            }
            Err(e) => match std::fs::symlink_metadata(at) {
                Ok(m) if m.file_type().is_symlink() => return Err(format!("automation path rejected: a link that leads nowhere: {path}")),
                Ok(_) => return Err(format!("automation path rejected: cannot resolve {path}: {e}")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("automation path rejected: cannot resolve {path}: {e}")),
            },
        }
        match (at.components().next_back(), at.parent()) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            (Some(Component::ParentDir), _) => {
                return Err(format!("automation path rejected: parent traversal below a folder that doesn't exist: {path}"));
            }
            _ => return Err(format!("automation path rejected: cannot resolve {path}: {}", std::io::Error::from(std::io::ErrorKind::NotFound))),
        }
    }
}

/// Is `path` (resolved) `root` or inside it, component by component (without regard to case on
/// Windows, whose file systems ignore it)?
fn inside(path: &Path, root: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|r| path.next().is_some_and(|p| same_component(p, r)))
}

fn same_component(a: Component, b: Component) -> bool {
    if a == b {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    match (a.as_os_str().to_str(), b.as_os_str().to_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => false,
    }
}

/// A Windows device name (`CON`, `NUL`, `COM1`... also with an extension), which names no file.
fn is_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |prefix: &str| stem.strip_prefix(prefix).is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"));
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: PathBuf) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn roots_scopes_and_traversal() {
        let base = std::env::temp_dir().join(format!("fc-file-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["in/sub", "out", "outside", "in2"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let roots = AutomationRoots::new(&[&base.join("in")], &[&base.join("out")]).unwrap();
        assert_eq!(roots.check(&s(base.join("in/sub/new.fcproj")), Access::Read), Ok(()));
        assert_eq!(roots.check(&s(base.join("out/new/deeper/x.mp4")), Access::Write), Ok(()));
        assert!(roots.check(&s(base.join("outside/x")), Access::Read).unwrap_err().contains("outside the read root"));
        assert!(roots.check(&s(base.join("in/x")), Access::Write).unwrap_err().contains("outside the write root"));
        assert!(roots.check(&s(base.join("in2/x")), Access::Read).is_err(), "in2 is not inside in");
        let sep = std::path::MAIN_SEPARATOR;
        assert!(roots.check(&format!("{}{sep}..{sep}outside{sep}x", s(base.join("in"))), Access::Read).is_err());
        assert!(roots.check("", Access::Read).is_err());
        assert!(AutomationRoots::new(&[&base.join("nope")], &[]).is_err());
        let none = AutomationRoots::new(&[], &[]).unwrap();
        assert!(none.check(&s(base.join("in/x")), Access::Read).unwrap_err().contains("read authority is absent"));
        #[cfg(windows)]
        assert!(roots.check(&s(base.join("out/NUL")), Access::Write).unwrap_err().contains("reserved device names"));

        let outside = s(base.join("outside/x"));
        assert_eq!(check_read(&outside), Ok(()), "nothing is confined by default");
        confine(&roots, || {
            assert!(check_read(&outside).is_err());
            assert_eq!(unconfined(|| check_read(&outside)), Ok(()));
            assert!(check_read(&outside).is_err(), "restored after unconfined");
            let handle = ScopeHandle::capture();
            let o = outside.clone();
            let there = std::thread::spawn(move || (check_read(&o).is_ok(), handle.run(|| check_read(&o).is_ok()))).join().unwrap();
            assert_eq!(there, (true, false), "another thread is confined only in the captured scope");
        });
        assert_eq!(check_read(&outside), Ok(()), "restored after confine");
        let _ = std::fs::remove_dir_all(base);
    }
}
