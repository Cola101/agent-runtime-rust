use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProcessResourceError {
    #[error("cgroup already exists and cannot be adopted: {0}")]
    GroupAlreadyExists(PathBuf),
    #[error("cgroup does not exist: {0}")]
    GroupMissing(PathBuf),
    #[error("unsafe cgroup controller file: {0}")]
    UnsafeControllerFile(PathBuf),
    #[error("malformed cgroup controller value: {0}")]
    MalformedControllerValue(String),
    #[error("cgroup I/O failed: {0}")]
    Io(String),
}

pub(crate) struct LinuxCgroupV2Root {
    path: PathBuf,
    directory: File,
}

pub(crate) struct LinuxCgroupV2Group {
    path: PathBuf,
    directory: File,
}

impl LinuxCgroupV2Root {
    pub(crate) fn open(path: &Path) -> Result<Self, ProcessResourceError> {
        validate_delegated_root(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
            let directory = options
                .open(path)
                .map_err(|error| ProcessResourceError::Io(error.to_string()))?;
            Ok(Self {
                path: path.to_path_buf(),
                directory,
            })
        }
        #[cfg(not(unix))]
        {
            Err(ProcessResourceError::Io(
                "cgroup directory handles require a Unix host".into(),
            ))
        }
    }

    pub(crate) fn open_group(
        &self,
        group_name: &str,
    ) -> Result<LinuxCgroupV2Group, ProcessResourceError> {
        if !is_session_group_name(group_name) {
            return Err(ProcessResourceError::UnsafeControllerFile(
                self.path.join(group_name),
            ));
        }
        let path = self.path.join(group_name);
        let directory = open_relative_directory(&self.directory, group_name, &path)?;
        Ok(LinuxCgroupV2Group { path, directory })
    }
}

pub(crate) fn configure_linux_cgroup_v2_group(
    group: &LinuxCgroupV2Group,
    max_memory_bytes: Option<u64>,
    max_processes: Option<u32>,
) -> Result<(), ProcessResourceError> {
    let settings = [
        (
            "memory.max",
            max_memory_bytes.map_or_else(|| "max\n".into(), |value| format!("{value}\n")),
        ),
        ("memory.oom.group", "1\n".into()),
        (
            "pids.max",
            max_processes.map_or_else(|| "max\n".into(), |value| format!("{value}\n")),
        ),
        ("cgroup.max.depth", "0\n".into()),
        ("cgroup.max.descendants", "0\n".into()),
    ];
    let opened = settings
        .into_iter()
        .map(|(name, value)| {
            open_relative_controller(&group.directory, name, &group.path.join(name), true)
                .map(|file| (file, value))
        })
        .collect::<Result<Vec<_>, ProcessResourceError>>()?;
    for (file, value) in opened {
        write_controller_value(&file, value.as_bytes())?;
    }
    Ok(())
}

fn open_relative_directory(
    parent: &File,
    name: &str,
    diagnostic_path: &Path,
) -> Result<File, ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let name = CString::new(name).map_err(|_| {
            ProcessResourceError::UnsafeControllerFile(diagnostic_path.to_path_buf())
        })?;
        // SAFETY: `parent` is an open directory, `name` is NUL-terminated and
        // the returned descriptor is immediately owned by `File`.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::NotFound {
                Err(ProcessResourceError::GroupMissing(
                    diagnostic_path.to_path_buf(),
                ))
            } else {
                Err(ProcessResourceError::Io(error.to_string()))
            };
        }
        // SAFETY: `openat` returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    #[cfg(not(unix))]
    {
        let _ = (parent, name);
        Err(ProcessResourceError::UnsafeControllerFile(
            diagnostic_path.to_path_buf(),
        ))
    }
}

fn create_relative_directory(
    parent: &File,
    name: &str,
    diagnostic_path: &Path,
) -> Result<(), ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;

        let name = CString::new(name).map_err(|_| {
            ProcessResourceError::UnsafeControllerFile(diagnostic_path.to_path_buf())
        })?;
        // SAFETY: `parent` is an open directory and `name` is a single
        // validated, NUL-terminated path component.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Err(ProcessResourceError::GroupAlreadyExists(
                diagnostic_path.to_path_buf(),
            ))
        } else {
            Err(ProcessResourceError::Io(error.to_string()))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (parent, name);
        Err(ProcessResourceError::UnsafeControllerFile(
            diagnostic_path.to_path_buf(),
        ))
    }
}

fn remove_relative_directory(
    parent: &File,
    name: &str,
    diagnostic_path: &Path,
) -> Result<(), ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;

        let name = CString::new(name).map_err(|_| {
            ProcessResourceError::UnsafeControllerFile(diagnostic_path.to_path_buf())
        })?;
        // SAFETY: `parent` is an open directory and `name` is a single
        // validated path component. `AT_REMOVEDIR` cannot unlink files or
        // recursively remove attacker-created contents.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(ProcessResourceError::Io(error.to_string()))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (parent, name);
        Err(ProcessResourceError::UnsafeControllerFile(
            diagnostic_path.to_path_buf(),
        ))
    }
}

fn open_relative_controller(
    group: &File,
    name: &str,
    diagnostic_path: &Path,
    write: bool,
) -> Result<File, ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let name = CString::new(name).map_err(|_| {
            ProcessResourceError::UnsafeControllerFile(diagnostic_path.to_path_buf())
        })?;
        let access = if write {
            libc::O_WRONLY
        } else {
            libc::O_RDONLY
        };
        // SAFETY: `group` is an open directory, `name` is NUL-terminated and
        // the returned descriptor is immediately owned by `File`.
        let fd = unsafe {
            libc::openat(
                group.as_raw_fd(),
                name.as_ptr(),
                access | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ELOOP) {
                Err(ProcessResourceError::UnsafeControllerFile(
                    diagnostic_path.to_path_buf(),
                ))
            } else {
                Err(ProcessResourceError::Io(error.to_string()))
            };
        }
        // SAFETY: `openat` returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !file
            .metadata()
            .map_err(|error| ProcessResourceError::Io(error.to_string()))?
            .is_file()
        {
            return Err(ProcessResourceError::UnsafeControllerFile(
                diagnostic_path.to_path_buf(),
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (group, name, write);
        Err(ProcessResourceError::UnsafeControllerFile(
            diagnostic_path.to_path_buf(),
        ))
    }
}

pub(crate) fn prepare_linux_cgroup_v2_root(
    root: &LinuxCgroupV2Root,
    group_name: &str,
    max_memory_bytes: Option<u64>,
    max_processes: Option<u32>,
) -> Result<LinuxCgroupV2Group, ProcessResourceError> {
    if !is_session_group_name(group_name) {
        return Err(ProcessResourceError::UnsafeControllerFile(
            root.path.join(group_name),
        ));
    }
    let path = root.path.join(group_name);
    create_relative_directory(&root.directory, group_name, &path)?;
    let group = match root.open_group(group_name) {
        Ok(group) => group,
        Err(error) => {
            let _ = remove_relative_directory(&root.directory, group_name, &path);
            return Err(error);
        }
    };
    if let Err(error) = configure_linux_cgroup_v2_group(&group, max_memory_bytes, max_processes) {
        drop(group);
        let _ = remove_relative_directory(&root.directory, group_name, &path);
        return Err(error);
    }
    Ok(group)
}

pub(crate) fn remove_linux_cgroup_v2_group_root(
    root: &LinuxCgroupV2Root,
    group_name: &str,
) -> Result<(), ProcessResourceError> {
    if !is_session_group_name(group_name) {
        return Err(ProcessResourceError::UnsafeControllerFile(
            root.path.join(group_name),
        ));
    }
    remove_relative_directory(&root.directory, group_name, &root.path.join(group_name))
}

fn write_current_process_membership(file: &File) -> Result<(), ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        write_current_process_membership_fd(file.as_raw_fd())
            .map_err(|error| ProcessResourceError::Io(error.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(ProcessResourceError::Io(
            "cgroup membership requires a Unix host".into(),
        ))
    }
}

pub(crate) fn install_linux_cgroup_membership_group(
    command: &mut tokio::process::Command,
    group: &LinuxCgroupV2Group,
) -> Result<(), ProcessResourceError> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let membership = open_relative_controller(
            &group.directory,
            "cgroup.procs",
            &group.path.join("cgroup.procs"),
            true,
        )?;
        // SAFETY: the closure runs after fork and before exec. The controller
        // descriptor is already anchored to the opened cgroup directory and
        // is closed automatically when exec succeeds.
        unsafe {
            command.pre_exec(move || write_current_process_membership_fd(membership.as_raw_fd()));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (command, group);
        Err(ProcessResourceError::Io(
            "cgroup membership requires a Unix host".into(),
        ))
    }
}

#[cfg(unix)]
fn write_current_process_membership_fd(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let bytes = b"0\n";
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: `fd` is an open controller descriptor captured by the
        // pre-exec closure and the remaining two-byte slice is valid.
        let result =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let count = usize::try_from(result)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        if count == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        written = written
            .checked_add(count)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
    }
    Ok(())
}

pub(crate) fn parse_cpu_usage_micros(bytes: &[u8]) -> Result<u64, ProcessResourceError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| ProcessResourceError::MalformedControllerValue(error.to_string()))?;
    let mut usage = None;
    for line in value.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if name != "usage_usec" {
            continue;
        }
        let raw = fields.next().ok_or_else(|| {
            ProcessResourceError::MalformedControllerValue("usage_usec has no value".into())
        })?;
        if fields.next().is_some() || usage.is_some() {
            return Err(ProcessResourceError::MalformedControllerValue(
                "usage_usec must occur exactly once with one value".into(),
            ));
        }
        usage =
            Some(raw.parse::<u64>().map_err(|error| {
                ProcessResourceError::MalformedControllerValue(error.to_string())
            })?);
    }
    usage.ok_or_else(|| {
        ProcessResourceError::MalformedControllerValue("usage_usec is missing".into())
    })
}

pub(crate) fn read_linux_cgroup_cpu_usage_micros_group(
    group: &LinuxCgroupV2Group,
) -> Result<u64, ProcessResourceError> {
    let bytes = read_relative_controller_value(group, "cpu.stat")?;
    parse_cpu_usage_micros(&bytes)
}

pub(crate) fn read_linux_cgroup_populated_group(
    group: &LinuxCgroupV2Group,
) -> Result<bool, ProcessResourceError> {
    let bytes = read_relative_controller_value(group, "cgroup.events")?;
    parse_cgroup_populated(&bytes)
}

pub(crate) fn kill_linux_cgroup_v2_group(
    group: &LinuxCgroupV2Group,
) -> Result<(), ProcessResourceError> {
    let file = open_relative_controller(
        &group.directory,
        "cgroup.kill",
        &group.path.join("cgroup.kill"),
        true,
    )?;
    write_controller_value(&file, b"1\n")
}

fn read_relative_controller_value(
    group: &LinuxCgroupV2Group,
    name: &str,
) -> Result<Vec<u8>, ProcessResourceError> {
    let mut file = open_relative_controller(&group.directory, name, &group.path.join(name), false)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| ProcessResourceError::Io(error.to_string()))?;
    Ok(bytes)
}

fn parse_cgroup_populated(bytes: &[u8]) -> Result<bool, ProcessResourceError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| ProcessResourceError::MalformedControllerValue(error.to_string()))?;
    let mut populated = None;
    for line in value.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if name != "populated" {
            continue;
        }
        let raw = fields.next().ok_or_else(|| {
            ProcessResourceError::MalformedControllerValue("populated has no value".into())
        })?;
        if fields.next().is_some() || populated.is_some() {
            return Err(ProcessResourceError::MalformedControllerValue(
                "populated must occur exactly once with one value".into(),
            ));
        }
        populated = Some(match raw {
            "0" => false,
            "1" => true,
            _ => {
                return Err(ProcessResourceError::MalformedControllerValue(
                    "populated must be 0 or 1".into(),
                ));
            }
        });
    }
    populated.ok_or_else(|| {
        ProcessResourceError::MalformedControllerValue("populated is missing".into())
    })
}

fn validate_delegated_root(path: &Path) -> Result<(), ProcessResourceError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| ProcessResourceError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(ProcessResourceError::UnsafeControllerFile(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn is_session_group_name(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("session-") else {
        return false;
    };
    Uuid::parse_str(uuid).is_ok_and(|parsed| parsed.to_string() == uuid)
}

fn write_controller_value(file: &File, value: &[u8]) -> Result<(), ProcessResourceError> {
    let mut file = file;
    file.write_all(value)
        .map_err(|error| ProcessResourceError::Io(error.to_string()))?;
    file.flush()
        .map_err(|error| ProcessResourceError::Io(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn fake_cgroup_files(group: &std::path::Path) {
        fs::create_dir(group).unwrap();
        for name in [
            "memory.max",
            "memory.oom.group",
            "pids.max",
            "cgroup.max.depth",
            "cgroup.max.descendants",
            "cgroup.procs",
            "cgroup.kill",
            "cpu.stat",
            "cgroup.events",
        ] {
            fs::write(group.join(name), b"").unwrap();
        }
    }

    fn open_fake_group(delegated_root: &Path, group_name: &str) -> LinuxCgroupV2Group {
        LinuxCgroupV2Root::open(delegated_root)
            .unwrap()
            .open_group(group_name)
            .unwrap()
    }

    #[test]
    fn cgroup_limits_are_written_exactly_without_creating_controller_files() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000010";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        let group = open_fake_group(root.path(), group_name);

        configure_linux_cgroup_v2_group(&group, Some(268_435_456), Some(17)).unwrap();

        assert_eq!(
            fs::read_to_string(group_path.join("memory.max")).unwrap(),
            "268435456\n"
        );
        assert_eq!(
            fs::read_to_string(group_path.join("memory.oom.group")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(group_path.join("pids.max")).unwrap(),
            "17\n"
        );
        assert_eq!(
            fs::read_to_string(group_path.join("cgroup.max.depth")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(group_path.join("cgroup.max.descendants")).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn cgroup_controller_symlink_is_rejected_without_touching_its_target() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000011";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        let outside = root.path().join("outside");
        fs::write(&outside, b"unchanged\n").unwrap();
        fs::remove_file(group_path.join("memory.max")).unwrap();
        symlink(&outside, group_path.join("memory.max")).unwrap();
        let group = open_fake_group(root.path(), group_name);

        let error = configure_linux_cgroup_v2_group(&group, Some(268_435_456), Some(17))
            .expect_err("a controller symlink was accepted");

        assert!(matches!(
            error,
            ProcessResourceError::UnsafeControllerFile(_)
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged\n");
    }

    #[test]
    fn membership_writer_uses_the_kernel_current_process_token() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000012";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        let group = open_fake_group(root.path(), group_name);
        let membership = group_path.join("cgroup.procs");
        let file =
            open_relative_controller(&group.directory, "cgroup.procs", &membership, true).unwrap();

        write_current_process_membership(&file).unwrap();

        assert_eq!(fs::read_to_string(membership).unwrap(), "0\n");
    }

    #[tokio::test]
    async fn spawned_child_joins_the_cgroup_before_exec() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000013";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        let group = open_fake_group(root.path(), group_name);
        let mut command = tokio::process::Command::new("/usr/bin/true");

        install_linux_cgroup_membership_group(&mut command, &group).unwrap();
        let status = command.spawn().unwrap().wait().await.unwrap();

        assert!(status.success());
        assert_eq!(
            fs::read_to_string(group_path.join("cgroup.procs")).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn failed_cgroup_preparation_removes_the_new_empty_group() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000001";
        let group = root.path().join(group_name);

        let opened_root = LinuxCgroupV2Root::open(root.path()).unwrap();
        let error = match prepare_linux_cgroup_v2_root(
            &opened_root,
            group_name,
            Some(268_435_456),
            Some(17),
        ) {
            Ok(_) => panic!("ordinary filesystem unexpectedly exposed cgroup controllers"),
            Err(error) => error,
        };

        assert!(matches!(error, ProcessResourceError::Io(_)));
        assert!(!group.exists(), "failed preparation leaked an empty group");
    }

    #[test]
    fn cgroup_preparation_never_takes_over_a_preexisting_group() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000002";
        let group = root.path().join(group_name);
        fake_cgroup_files(&group);
        fs::write(group.join("memory.max"), b"unchanged\n").unwrap();

        let opened_root = LinuxCgroupV2Root::open(root.path()).unwrap();
        let error = match prepare_linux_cgroup_v2_root(
            &opened_root,
            group_name,
            Some(268_435_456),
            Some(17),
        ) {
            Ok(_) => panic!("an existing cgroup was silently adopted"),
            Err(error) => error,
        };

        assert!(matches!(error, ProcessResourceError::GroupAlreadyExists(_)));
        assert_eq!(
            fs::read_to_string(group.join("memory.max")).unwrap(),
            "unchanged\n"
        );
    }

    #[test]
    fn cpu_stat_parser_reads_total_tree_usage_and_rejects_duplicates() {
        assert_eq!(
            parse_cpu_usage_micros(b"usage_usec 42001\nuser_usec 30000\nsystem_usec 12001\n")
                .unwrap(),
            42_001
        );
        assert!(matches!(
            parse_cpu_usage_micros(b"usage_usec 1\nusage_usec 2\n"),
            Err(ProcessResourceError::MalformedControllerValue(_))
        ));
        assert!(matches!(
            parse_cpu_usage_micros(b"user_usec 1\n"),
            Err(ProcessResourceError::MalformedControllerValue(_))
        ));
    }

    #[test]
    fn cgroup_observation_reads_total_cpu_and_populated_state() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000014";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        fs::write(
            group_path.join("cpu.stat"),
            b"usage_usec 42001\nuser_usec 30000\nsystem_usec 12001\n",
        )
        .unwrap();
        fs::write(group_path.join("cgroup.events"), b"populated 1\nfrozen 0\n").unwrap();
        let group = open_fake_group(root.path(), group_name);

        assert_eq!(
            read_linux_cgroup_cpu_usage_micros_group(&group).unwrap(),
            42_001
        );
        assert!(read_linux_cgroup_populated_group(&group).unwrap());

        fs::write(group_path.join("cgroup.events"), b"populated 0\nfrozen 0\n").unwrap();
        assert!(!read_linux_cgroup_populated_group(&group).unwrap());
    }

    #[test]
    fn cgroup_observation_rejects_ambiguous_or_symlinked_state() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000015";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        fs::write(
            group_path.join("cgroup.events"),
            b"populated 1\npopulated 0\n",
        )
        .unwrap();
        let group = open_fake_group(root.path(), group_name);

        assert!(matches!(
            read_linux_cgroup_populated_group(&group),
            Err(ProcessResourceError::MalformedControllerValue(_))
        ));

        let outside = root.path().join("outside-events");
        fs::write(&outside, b"populated 1\n").unwrap();
        fs::remove_file(group_path.join("cgroup.events")).unwrap();
        symlink(&outside, group_path.join("cgroup.events")).unwrap();

        assert!(matches!(
            read_linux_cgroup_populated_group(&group),
            Err(ProcessResourceError::UnsafeControllerFile(_))
        ));
    }

    #[test]
    fn cgroup_kill_writes_the_kernel_tree_trigger_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let group_name = "session-00000000-0000-0000-0000-000000000016";
        let group_path = root.path().join(group_name);
        fake_cgroup_files(&group_path);
        let group = open_fake_group(root.path(), group_name);

        kill_linux_cgroup_v2_group(&group).unwrap();
        assert_eq!(
            fs::read_to_string(group_path.join("cgroup.kill")).unwrap(),
            "1\n"
        );

        let outside = root.path().join("outside-kill");
        fs::write(&outside, b"unchanged\n").unwrap();
        fs::remove_file(group_path.join("cgroup.kill")).unwrap();
        symlink(&outside, group_path.join("cgroup.kill")).unwrap();

        assert!(matches!(
            kill_linux_cgroup_v2_group(&group),
            Err(ProcessResourceError::UnsafeControllerFile(_))
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged\n");
    }

    #[test]
    fn opened_delegated_root_cannot_be_redirected_by_path_replacement() {
        let container = tempfile::tempdir().unwrap();
        let delegated_root = container.path().join("delegated");
        let moved_root = container.path().join("delegated-original");
        let group_name = "session-00000000-0000-0000-0000-000000000003";
        let original_group = delegated_root.join(group_name);
        fs::create_dir(&delegated_root).unwrap();
        fake_cgroup_files(&original_group);
        let root = LinuxCgroupV2Root::open(&delegated_root).unwrap();

        fs::rename(&delegated_root, &moved_root).unwrap();
        fs::create_dir(&delegated_root).unwrap();
        let replacement_group = delegated_root.join(group_name);
        fake_cgroup_files(&replacement_group);
        fs::write(replacement_group.join("memory.max"), b"replacement\n").unwrap();

        let group = root.open_group(group_name).unwrap();
        configure_linux_cgroup_v2_group(&group, Some(268_435_456), Some(17)).unwrap();

        assert_eq!(
            fs::read_to_string(moved_root.join(group_name).join("memory.max")).unwrap(),
            "268435456\n"
        );
        assert_eq!(
            fs::read_to_string(replacement_group.join("memory.max")).unwrap(),
            "replacement\n",
            "controller access escaped to the replacement delegated root"
        );
    }

    #[test]
    fn opened_group_keeps_observation_and_termination_on_the_original_directory() {
        let root_dir = tempfile::tempdir().unwrap();
        let delegated_root = root_dir.path().join("delegated");
        let group_name = "session-00000000-0000-0000-0000-000000000004";
        let original_group = delegated_root.join(group_name);
        let moved_group = delegated_root.join("moved-group");
        fs::create_dir(&delegated_root).unwrap();
        fake_cgroup_files(&original_group);
        fs::write(original_group.join("cpu.stat"), b"usage_usec 42001\n").unwrap();
        fs::write(original_group.join("cgroup.events"), b"populated 1\n").unwrap();
        let root = LinuxCgroupV2Root::open(&delegated_root).unwrap();
        let group = root.open_group(group_name).unwrap();

        fs::rename(&original_group, &moved_group).unwrap();
        fake_cgroup_files(&original_group);
        fs::write(original_group.join("cpu.stat"), b"usage_usec 7\n").unwrap();
        fs::write(original_group.join("cgroup.events"), b"populated 0\n").unwrap();
        fs::write(original_group.join("cgroup.kill"), b"replacement\n").unwrap();

        assert_eq!(
            read_linux_cgroup_cpu_usage_micros_group(&group).unwrap(),
            42_001
        );
        assert!(read_linux_cgroup_populated_group(&group).unwrap());
        kill_linux_cgroup_v2_group(&group).unwrap();

        assert_eq!(
            fs::read_to_string(moved_group.join("cgroup.kill")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(original_group.join("cgroup.kill")).unwrap(),
            "replacement\n",
            "termination escaped to the replacement cgroup directory"
        );
    }

    #[tokio::test]
    async fn opened_group_keeps_pre_exec_membership_on_the_original_directory() {
        let root_dir = tempfile::tempdir().unwrap();
        let delegated_root = root_dir.path().join("delegated");
        let group_name = "session-00000000-0000-0000-0000-000000000005";
        let original_group = delegated_root.join(group_name);
        let moved_group = delegated_root.join("moved-group");
        fs::create_dir(&delegated_root).unwrap();
        fake_cgroup_files(&original_group);
        let root = LinuxCgroupV2Root::open(&delegated_root).unwrap();
        let group = root.open_group(group_name).unwrap();

        fs::rename(&original_group, &moved_group).unwrap();
        fake_cgroup_files(&original_group);
        fs::write(original_group.join("cgroup.procs"), b"replacement\n").unwrap();
        let mut command = tokio::process::Command::new("/usr/bin/true");

        install_linux_cgroup_membership_group(&mut command, &group).unwrap();
        let status = command.spawn().unwrap().wait().await.unwrap();

        assert!(status.success());
        assert_eq!(
            fs::read_to_string(moved_group.join("cgroup.procs")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(original_group.join("cgroup.procs")).unwrap(),
            "replacement\n",
            "membership escaped to the replacement cgroup directory"
        );
    }

    #[test]
    fn opened_root_prepares_and_rolls_back_only_beneath_the_original_directory() {
        let container = tempfile::tempdir().unwrap();
        let delegated_root = container.path().join("delegated");
        let moved_root = container.path().join("delegated-original");
        let group_name = "session-00000000-0000-0000-0000-000000000006";
        fs::create_dir(&delegated_root).unwrap();
        let root = LinuxCgroupV2Root::open(&delegated_root).unwrap();

        fs::rename(&delegated_root, &moved_root).unwrap();
        fs::create_dir(&delegated_root).unwrap();
        let replacement_group = delegated_root.join(group_name);
        fake_cgroup_files(&replacement_group);
        fs::write(replacement_group.join("memory.max"), b"replacement\n").unwrap();

        let error =
            match prepare_linux_cgroup_v2_root(&root, group_name, Some(268_435_456), Some(17)) {
                Ok(_) => panic!("ordinary filesystem unexpectedly exposed cgroup controllers"),
                Err(error) => error,
            };

        assert!(matches!(error, ProcessResourceError::Io(_)));
        assert!(
            !moved_root.join(group_name).exists(),
            "failed preparation leaked a group under the opened root"
        );
        assert_eq!(
            fs::read_to_string(replacement_group.join("memory.max")).unwrap(),
            "replacement\n",
            "preparation or rollback escaped to the replacement delegated root"
        );
    }

    #[test]
    fn terminal_cleanup_is_fd_relative_and_idempotent() {
        let container = tempfile::tempdir().unwrap();
        let delegated_root = container.path().join("delegated");
        let moved_root = container.path().join("delegated-original");
        let group_name = "session-00000000-0000-0000-0000-000000000007";
        fs::create_dir(&delegated_root).unwrap();
        fs::create_dir(delegated_root.join(group_name)).unwrap();
        let root = LinuxCgroupV2Root::open(&delegated_root).unwrap();

        fs::rename(&delegated_root, &moved_root).unwrap();
        fs::create_dir(&delegated_root).unwrap();
        fs::create_dir(delegated_root.join(group_name)).unwrap();

        remove_linux_cgroup_v2_group_root(&root, group_name).unwrap();
        remove_linux_cgroup_v2_group_root(&root, group_name).unwrap();

        assert!(
            !moved_root.join(group_name).exists(),
            "terminal cleanup left the original empty group behind"
        );
        assert!(
            delegated_root.join(group_name).exists(),
            "terminal cleanup escaped to the replacement delegated root"
        );
    }
}
