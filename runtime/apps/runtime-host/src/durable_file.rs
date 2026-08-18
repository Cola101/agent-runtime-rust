use crate::LocalRuntimeError;
use std::io::Write as _;
use std::path::Path;

trait DurableReplaceIo {
    type File;

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;
    fn create_file(&self, path: &Path) -> std::io::Result<Self::File>;
    fn write_all(&self, file: &mut Self::File, body: &[u8]) -> std::io::Result<()>;
    fn sync_file(&self, file: &Self::File) -> std::io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
    fn sync_directory(&self, path: &Path) -> std::io::Result<()>;
}

struct StdDurableReplaceIo;

impl DurableReplaceIo for StdDurableReplaceIo {
    type File = std::fs::File;

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn create_file(&self, path: &Path) -> std::io::Result<Self::File> {
        std::fs::File::create(path)
    }

    fn write_all(&self, file: &mut Self::File, body: &[u8]) -> std::io::Result<()> {
        file.write_all(body)
    }

    fn sync_file(&self, file: &Self::File) -> std::io::Result<()> {
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }

    fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        std::fs::File::open(path)?.sync_all()?;
        Ok(())
    }
}

/// One writer at a time per record, within this process.
///
/// The staging file is named from the target path, so two `replace` calls on
/// one record share a `.json.partial`: one renames it away while the other
/// still expects it, and the loser's `rename` fails with ENOENT. A caller sees
/// "storage is unavailable" about a state root that is perfectly fine.
///
/// Serialised rather than renamed. The `.json.partial` suffix is load-bearing:
/// recovery and retention find a half-written record by that exact name
/// (`embedded.rs`, `event_archive.rs`, `retention.rs`, and `lib.rs`), so making
/// it unique per writer would have to change every one of those, and would
/// leave a stale file per crashed write instead of the one that is currently
/// overwritten in place.
///
/// Sharded by path rather than one global lock, because two records have no
/// reason to wait for each other; and by hash rather than by a map keyed on the
/// path, because a map would grow with every record this process ever wrote.
/// Contention within a shard is between writers of the same file, which is
/// exactly what has to be serialised anyway.
///
/// This is process-local. Two *processes* writing one state root is already
/// refused elsewhere -- the state root carries a single-owner lock -- so this
/// is the scope the problem actually has.
const REPLACE_SHARDS: usize = 64;

fn replace_gates() -> &'static [std::sync::Mutex<()>; REPLACE_SHARDS] {
    static GATES: std::sync::OnceLock<[std::sync::Mutex<()>; REPLACE_SHARDS]> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| std::array::from_fn(|_| std::sync::Mutex::new(())))
}

fn replace_shard(path: &Path) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    usize::try_from(hasher.finish() % REPLACE_SHARDS as u64).unwrap_or(0)
}

fn replace_with_io<I: DurableReplaceIo>(
    io: &I,
    path: &Path,
    body: &[u8],
) -> Result<(), LocalRuntimeError> {
    let _writing = replace_gates()[replace_shard(path)]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let parent = path
        .parent()
        .ok_or_else(|| LocalRuntimeError::StateRoot("durable state path has no parent".into()))?;
    io.create_dir_all(parent)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let staging = path.with_extension("json.partial");
    let mut file = io
        .create_file(&staging)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    io.write_all(&mut file, body)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    io.sync_file(&file)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    drop(file);
    io.rename(&staging, path)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    io.sync_directory(parent)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))
}

pub(crate) fn replace(path: &Path, body: &[u8]) -> Result<(), LocalRuntimeError> {
    replace_with_io(&StdDurableReplaceIo, path, body)
}

fn rename_with_io<I: DurableReplaceIo>(
    io: &I,
    from: &Path,
    to: &Path,
) -> Result<(), LocalRuntimeError> {
    let from_parent = from.parent().ok_or_else(|| {
        LocalRuntimeError::StateRoot("durable rename source has no parent".into())
    })?;
    let to_parent = to.parent().ok_or_else(|| {
        LocalRuntimeError::StateRoot("durable rename target has no parent".into())
    })?;
    io.rename(from, to)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    io.sync_directory(to_parent)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    if from_parent != to_parent {
        io.sync_directory(from_parent)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    }
    Ok(())
}

pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), LocalRuntimeError> {
    rename_with_io(&StdDurableReplaceIo, from, to)
}

fn remove_with_io<I: DurableReplaceIo>(io: &I, path: &Path) -> Result<(), LocalRuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| LocalRuntimeError::StateRoot("durable removal path has no parent".into()))?;
    match io.remove_file(path) {
        Ok(()) => {}
        // Already gone is the state the caller asked for. Every other failure
        // is uncertainty about whether it is gone, and must reach the caller.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
    }
    // Unlinking is a change to the directory, and an unsynced directory can
    // still list a file that is gone. A removal that has not been committed to
    // its namespace is not a removal.
    io.sync_directory(parent)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))
}

/// Removes a durable file and commits that removal to its directory.
pub(crate) fn remove(path: &Path) -> Result<(), LocalRuntimeError> {
    remove_with_io(&StdDurableReplaceIo, path)
}

/// Two writers, one path, at the same time.
///
/// The staging file is named from the target path, so two `replace` calls on
/// one record use the same `.json.partial`: one renames it away while the
/// other still expects it, and the second `rename` fails with ENOENT. That
/// reaches a caller as "Session storage is unavailable" -- the state root is
/// fine, and the answer names the wrong thing entirely.
///
/// This is not hypothetical concurrency. Nine call sites persist a Session
/// record and only the projection path takes a gate, so a Run committing its
/// own Turn and a reader projecting the same branch race by construction. The
/// `grpc_session_contract` test failed this way in 4 of 25 runs.
#[test]
fn two_writers_on_one_path_do_not_lose_each_other_s_staging_file() {
    let home = tempfile::tempdir().expect("temp dir");
    let path = home.path().join("record.json");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for writer in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            let failures = failures.clone();
            scope.spawn(move || {
                let body = format!("{{\"writer\":{writer}}}").into_bytes();
                barrier.wait();
                for _ in 0..40 {
                    if let Err(error) = replace(&path, &body) {
                        failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(error.to_string());
                    }
                }
            });
        }
    });

    let failures = failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        failures.is_empty(),
        "concurrent writers to one record must not fail: {failures:?}",
    );
    // And what is on disk is one of the bodies, whole -- not a mixture.
    let held = std::fs::read_to_string(&path).expect("the record is readable");
    assert!(
        (0..8).any(|writer| held == format!("{{\"writer\":{writer}}}")),
        "a torn or empty record was left behind: {held:?}",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingIo {
        operations: RefCell<Vec<&'static str>>,
        fail_on: Option<&'static str>,
        missing: bool,
    }

    impl RecordingIo {
        fn record(&self, operation: &'static str) -> std::io::Result<()> {
            self.operations.borrow_mut().push(operation);
            if self.fail_on == Some(operation) {
                return Err(std::io::Error::other(format!(
                    "injected {operation} failure"
                )));
            }
            Ok(())
        }
    }

    impl DurableReplaceIo for RecordingIo {
        type File = ();

        fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            self.record("create_dir_all")
        }

        fn create_file(&self, _path: &Path) -> std::io::Result<Self::File> {
            self.record("create_file")?;
            Ok(())
        }

        fn write_all(&self, _file: &mut Self::File, _body: &[u8]) -> std::io::Result<()> {
            self.record("write_all")
        }

        fn sync_file(&self, _file: &Self::File) -> std::io::Result<()> {
            self.record("sync_file")
        }

        fn rename(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
            self.record("rename")
        }

        fn remove_file(&self, _path: &Path) -> std::io::Result<()> {
            if self.missing {
                self.operations.borrow_mut().push("remove_file");
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            self.record("remove_file")
        }

        fn sync_directory(&self, _path: &Path) -> std::io::Result<()> {
            self.record("sync_directory")
        }
    }

    #[test]
    fn replacement_syncs_the_new_file_before_rename_and_the_parent_afterward() {
        let io = RecordingIo::default();

        replace_with_io(&io, Path::new("state/session.json"), b"new state").expect("replacement");

        assert_eq!(
            *io.operations.borrow(),
            [
                "create_dir_all",
                "create_file",
                "write_all",
                "sync_file",
                "rename",
                "sync_directory",
            ],
            "rename alone is atomic but not a durable commit"
        );
    }

    #[test]
    fn file_sync_failure_is_returned_before_the_commit_rename() {
        let io = RecordingIo {
            fail_on: Some("sync_file"),
            ..RecordingIo::default()
        };

        let error = replace_with_io(&io, Path::new("state/session.json"), b"new state")
            .expect_err("file sync uncertainty must fail closed");

        assert!(matches!(error, LocalRuntimeError::StateRoot(_)));
        assert_eq!(
            *io.operations.borrow(),
            ["create_dir_all", "create_file", "write_all", "sync_file"],
            "an unsynced staging file must never replace committed state"
        );
    }

    #[test]
    fn parent_sync_failure_is_returned_after_the_visible_rename() {
        let io = RecordingIo {
            fail_on: Some("sync_directory"),
            ..RecordingIo::default()
        };

        let error = replace_with_io(&io, Path::new("state/session.json"), b"new state")
            .expect_err("directory durability uncertainty must reach the caller");

        assert!(matches!(error, LocalRuntimeError::StateRoot(_)));
        assert_eq!(
            *io.operations.borrow(),
            [
                "create_dir_all",
                "create_file",
                "write_all",
                "sync_file",
                "rename",
                "sync_directory",
            ]
        );
    }

    #[test]
    fn same_directory_rename_syncs_its_namespace_commit() {
        let io = RecordingIo::default();

        rename_with_io(
            &io,
            Path::new("state/active.json"),
            Path::new("state/archive.json"),
        )
        .expect("rename");

        assert_eq!(*io.operations.borrow(), ["rename", "sync_directory"]);
    }

    #[test]
    fn removal_commits_the_unlink_to_its_directory() {
        let io = RecordingIo::default();

        remove_with_io(&io, Path::new("state/session.json")).expect("removal");

        assert_eq!(
            *io.operations.borrow(),
            ["remove_file", "sync_directory"],
            "an unsynced directory can still list a file that is gone"
        );
    }

    #[test]
    fn removing_what_is_already_gone_is_the_state_the_caller_asked_for() {
        let io = RecordingIo {
            missing: true,
            ..RecordingIo::default()
        };

        remove_with_io(&io, Path::new("state/session.json")).expect("absent is removed");

        assert_eq!(
            *io.operations.borrow(),
            ["remove_file"],
            "there is no namespace change to commit"
        );
    }

    #[test]
    fn removal_failure_is_returned_rather_than_reported_as_removed() {
        let io = RecordingIo {
            fail_on: Some("remove_file"),
            ..RecordingIo::default()
        };

        let error = remove_with_io(&io, Path::new("state/session.json"))
            .expect_err("an unremoved file must not be reported as removed");

        assert!(matches!(error, LocalRuntimeError::StateRoot(_)));
        assert_eq!(*io.operations.borrow(), ["remove_file"]);
    }

    #[test]
    fn removal_directory_sync_failure_reaches_the_caller() {
        let io = RecordingIo {
            fail_on: Some("sync_directory"),
            ..RecordingIo::default()
        };

        let error = remove_with_io(&io, Path::new("state/session.json"))
            .expect_err("uncommitted removal is uncertainty, not success");

        assert!(matches!(error, LocalRuntimeError::StateRoot(_)));
        assert_eq!(*io.operations.borrow(), ["remove_file", "sync_directory"]);
    }

    #[test]
    fn cross_directory_rename_syncs_target_then_source_namespaces() {
        let io = RecordingIo::default();

        rename_with_io(
            &io,
            Path::new("active/item.json"),
            Path::new("archive/item.json"),
        )
        .expect("rename");

        assert_eq!(
            *io.operations.borrow(),
            ["rename", "sync_directory", "sync_directory"]
        );
    }
}
