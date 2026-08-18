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

fn replace_with_io<I: DurableReplaceIo>(
    io: &I,
    path: &Path,
    body: &[u8],
) -> Result<(), LocalRuntimeError> {
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
