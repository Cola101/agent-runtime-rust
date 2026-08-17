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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingIo {
        operations: RefCell<Vec<&'static str>>,
        fail_on: Option<&'static str>,
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
}
