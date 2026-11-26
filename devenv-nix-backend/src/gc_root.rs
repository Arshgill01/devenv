//! GC roots for the C-Nix store.
//!
//! A GC root is a symlink outside the store that points at a store path.
//! Nix registers the link as an indirect root, so the path survives store
//! garbage collection.

use std::path::Path;

use miette::{Result, WrapErr, miette};
use nix_bindings_store::path::StorePath;
use nix_bindings_store::store::Store;

use crate::anyhow_ext::AnyhowToMiette;

/// What was found at the GC root path before the root was registered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GcRootOutcome {
    /// No entry existed.
    Created,
    /// A symlink already pointed at the store path.
    Unchanged,
    /// A symlink pointed at another store path.
    Replaced,
    /// The entry was not a symlink into the store.
    Invalid,
}

impl GcRootOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Unchanged => "unchanged",
            Self::Replaced => "replaced",
            Self::Invalid => "invalid",
        }
    }
}

/// Point `gc_root` at `store_path` and register it with Nix.
///
/// `store_path` may be the real path of a relocated store. It is mapped back
/// to the logical store path before it is parsed (devenv #2499).
pub(crate) fn ensure_gc_root(
    store: &mut Store,
    gc_root: &Path,
    store_path: &str,
) -> Result<GcRootOutcome> {
    let (storedir, logical_path, parsed_path) = logical_store_path(store, store_path)?;

    let outcome = match gc_root.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GcRootOutcome::Created,
        Err(error) => return Err(miette!("Failed to inspect existing GC root: {}", error)),
        Ok(metadata) if !metadata.file_type().is_symlink() => GcRootOutcome::Invalid,
        Ok(_) => match std::fs::read_link(gc_root) {
            Ok(target) if target == Path::new(&logical_path) => GcRootOutcome::Unchanged,
            Ok(target) if is_in_store(&target, &storedir) => GcRootOutcome::Replaced,
            Ok(_) => GcRootOutcome::Invalid,
            // Another process removed the symlink after symlink_metadata saw it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GcRootOutcome::Created,
            Err(error) => return Err(miette!("Failed to read existing GC root: {}", error)),
        },
    };

    // Nix atomically replaces symlinks into the store; remove only entries it refuses.
    if outcome == GcRootOutcome::Invalid {
        match std::fs::remove_file(gc_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(miette!("Failed to remove existing GC root: {}", error)),
        }
    }
    store
        .add_perm_root(&parsed_path, gc_root)
        .to_miette()
        .wrap_err("Failed to create GC root")?;
    Ok(outcome)
}

/// The same test as Nix's `isInStore`: strictly below the store directory.
fn is_in_store(path: &Path, storedir: &str) -> bool {
    path.strip_prefix(storedir)
        .is_ok_and(|rest| !rest.as_os_str().is_empty())
}

/// Map a possibly `real_path`-translated store path to its logical form and
/// parse it. Returns the store directory, the logical path, and the parsed path.
fn logical_store_path(store: &mut Store, path: &str) -> Result<(String, String, StorePath)> {
    let storedir = store
        .get_storedir()
        .to_miette()
        .wrap_err("Failed to get store directory")?;
    let logical = logical_store_path_str(&storedir, path)
        .ok_or_else(|| miette!("store path '{}' has no basename", path))?;
    let parsed = store
        .parse_store_path(&logical)
        .to_miette()
        .wrap_err("Failed to parse store path")?;
    Ok((storedir, logical, parsed))
}

/// Build the logical `<storedir>/<basename>` store-path string from a path that
/// may have been `real_path`-translated for a relocated/chroot store.
///
/// `Store::parse_store_path` only accepts the logical form (e.g.
/// `/nix/store/<hash>-<name>`), but cached results hold the *real* path
/// returned by `Store::real_path`, which differs for a relocated store
/// (e.g. `/srv/nix/store/<hash>-<name>`). The basename is identical between the
/// two forms. Returns `None` if the path has no basename.
fn logical_store_path_str(storedir: &str, path: &str) -> Option<String> {
    let basename = Path::new(path).file_name()?.to_str()?;
    Some(format!("{}/{}", storedir.trim_end_matches('/'), basename))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use nix_bindings_store::store::Store;

    use super::{GcRootOutcome, ensure_gc_root, is_in_store, logical_store_path_str};

    #[test]
    fn ensure_gc_root_on_a_real_store_treats_a_vanished_symlink_as_create() {
        crate::gc_boehm::init();
        crate::gc_boehm::register_current_thread().expect("register gc thread");

        let mut store = Store::open(None, []).expect("open nix store");
        let storedir = store.get_storedir().expect("store dir");
        let store_path = first_store_object(&storedir);
        let dir = tempfile::tempdir().expect("tempdir");
        let gc_root = dir.path().join("devenv-gc-root");

        let created = ensure_gc_root(&mut store, &gc_root, &store_path).expect("create gc root");
        assert_eq!(created, GcRootOutcome::Created);
        assert_eq!(
            std::fs::read_link(&gc_root).expect("gc root symlink"),
            Path::new(&store_path)
        );

        let unchanged = ensure_gc_root(&mut store, &gc_root, &store_path).expect("keep gc root");
        assert_eq!(unchanged, GcRootOutcome::Unchanged);

        let stop = Arc::new(AtomicBool::new(false));
        let mut deleters = Vec::new();
        for _ in 0..8 {
            let stop_flag = Arc::clone(&stop);
            let root = gc_root.clone();
            deleters.push(std::thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&root);
                }
            }));
        }

        // The window is the two syscalls inside ensure_gc_root. A build that
        // still returned the read error failed inside a loop of this size.
        let mut read_error = None;
        for _ in 0..100_000 {
            if let Err(error) = ensure_gc_root(&mut store, &gc_root, &store_path) {
                let message = error.to_string();
                if message.contains("Failed to read existing GC root") {
                    read_error = Some(message);
                    break;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        for deleter in deleters {
            deleter.join().expect("deleter");
        }
        assert!(
            read_error.is_none(),
            "vanished symlink still failed the read: {read_error:?}"
        );
    }

    fn first_store_object(storedir: &str) -> String {
        std::fs::read_dir(storedir)
            .expect("read store directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.starts_with('.'))
            })
            .expect("nix store contains a path")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn logical_store_path_str_recovers_relocated_path() {
        let name = "rdd4pnr4x9rqc9wgbibhngv217w2xvxl-bash-interactive-5.2p26";
        let logical = format!("/nix/store/{name}");

        assert_eq!(
            logical_store_path_str("/nix/store", &format!("/srv/nix/store/{name}")).as_deref(),
            Some(logical.as_str())
        );
        assert_eq!(
            logical_store_path_str("/nix/store/", &logical).as_deref(),
            Some(logical.as_str())
        );
        assert_eq!(logical_store_path_str("/nix/store", "/"), None);
    }

    #[test]
    fn is_in_store_requires_a_path_below_the_store_dir() {
        assert!(is_in_store(Path::new("/nix/store/abc-x"), "/nix/store"));
        assert!(!is_in_store(Path::new("/nix/store"), "/nix/store"));
        assert!(!is_in_store(Path::new("/nix/storefoo/abc-x"), "/nix/store"));
        assert!(!is_in_store(Path::new("../store/abc-x"), "/nix/store"));
    }
}
