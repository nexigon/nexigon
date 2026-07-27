//! Fail-closed terminal child preparation and execution.
//!
//! Everything that may allocate, consult NSS, or take a process-global lock is
//! completed before `forkpty`. The child path itself uses only raw Linux syscalls,
//! fixed-size stack operations, and `_exit`.

use std::ffi::CStr;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use anyhow::Context;
use anyhow::bail;
use nix::libc;
use nix::unistd::User;

const CHILD_EXIT_SET_GROUPS: i32 = 120;
const CHILD_EXIT_SET_GID: i32 = 121;
const CHILD_EXIT_VERIFY_GID: i32 = 122;
const CHILD_EXIT_SET_UID: i32 = 123;
const CHILD_EXIT_VERIFY_UID: i32 = 124;
const CHILD_EXIT_VERIFY_GROUPS: i32 = 125;
const CHILD_EXIT_CHDIR: i32 = 126;
const CHILD_EXIT_EXEC: i32 = 127;

/// Environment supplied to the terminal executable.
///
/// Deliberately do not inherit the agent environment: it can contain deployment
/// tokens, proxy credentials, tracing headers, and other daemon-only secrets.
const TERMINAL_PATH: &[u8] = b"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const TERMINAL_TERM: &[u8] = b"xterm-256color";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialMode {
    SetAndVerify,
    VerifyOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Credentials {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessCredentials {
    uids: [libc::uid_t; 3],
    gids: [libc::gid_t; 3],
    groups: Vec<libc::gid_t>,
}

/// Fully allocated child state. The pointer arrays refer to buffers owned by the
/// adjacent `CString` vectors; those heap allocations remain stable when this
/// structure is moved.
pub(super) struct PreparedChild {
    credentials: Credentials,
    credential_mode: CredentialMode,
    cwd: CString,
    executable: CString,
    argv: Vec<CString>,
    argv_pointers: Vec<*const libc::c_char>,
    environment: Vec<CString>,
    environment_pointers: Vec<*const libc::c_char>,
    group_verification_buffer: Vec<libc::gid_t>,
}

/// Build the production login-shell invocation before forking.
pub(super) fn prepare(user: &User, shell: &str, arg0: &str) -> anyhow::Result<PreparedChild> {
    let argv = vec![CString::new(arg0).context("terminal shell argument contains a NUL byte")?];
    prepare_with_argv(user, shell, argv)
}

fn prepare_with_argv(
    user: &User,
    executable: &str,
    argv: Vec<CString>,
) -> anyhow::Result<PreparedChild> {
    if !Path::new(executable).is_absolute() {
        bail!("terminal executable must be an absolute path");
    }
    if argv.is_empty() {
        bail!("terminal executable requires argv[0]");
    }

    let executable =
        CString::new(executable).context("terminal executable path contains a NUL byte")?;
    let cwd = CString::new(user.dir.as_os_str().as_bytes())
        .context("terminal home directory contains a NUL byte")?;
    let username =
        CString::new(user.name.as_bytes()).context("terminal username contains a NUL byte")?;
    let credentials = credentials_for_user(user, &username)?;
    let current = read_process_credentials()?;
    let credential_mode = credential_mode(&current, &credentials)?;
    let environment = minimal_environment(&username, &cwd, &executable)?;

    let mut prepared = PreparedChild {
        group_verification_buffer: vec![0; credentials.groups.len()],
        credentials,
        credential_mode,
        cwd,
        executable,
        argv,
        argv_pointers: Vec::new(),
        environment,
        environment_pointers: Vec::new(),
    };
    prepared.argv_pointers = pointers_with_null(&prepared.argv);
    prepared.environment_pointers = pointers_with_null(&prepared.environment);
    Ok(prepared)
}

fn credentials_for_user(user: &User, username: &CStr) -> anyhow::Result<Credentials> {
    let mut groups = nix::unistd::getgrouplist(username, user.gid)
        .context("failed to resolve terminal user's supplementary groups")?
        .into_iter()
        .map(|group| group.as_raw())
        .collect::<Vec<_>>();
    groups.push(user.gid.as_raw());
    groups.sort_unstable();
    groups.dedup();
    Ok(Credentials {
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        groups,
    })
}

fn read_process_credentials() -> anyhow::Result<ProcessCredentials> {
    let mut syscalls = LinuxChildSyscalls;
    let mut uids = [0; 3];
    let mut gids = [0; 3];

    // SAFETY: These raw syscalls write to the supplied fixed-size arrays.
    if !unsafe { syscalls.read_res_uid(&mut uids) } {
        return Err(std::io::Error::last_os_error()).context("failed to read process user IDs");
    }
    // SAFETY: These raw syscalls write to the supplied fixed-size arrays.
    if !unsafe { syscalls.read_res_gid(&mut gids) } {
        return Err(std::io::Error::last_os_error()).context("failed to read process group IDs");
    }
    // SAFETY: A null buffer with length zero asks Linux for the required size.
    let count = unsafe { syscalls.group_count() }
        .ok_or_else(std::io::Error::last_os_error)
        .context("failed to read supplementary group count")?;
    let mut groups = vec![0; count];
    if count > 0 {
        // SAFETY: `groups` has exactly `count` writable entries.
        if !unsafe { syscalls.read_groups(groups.as_mut_ptr(), groups.len()) } {
            return Err(std::io::Error::last_os_error())
                .context("failed to read supplementary groups");
        }
    }
    groups.sort_unstable();
    groups.dedup();
    Ok(ProcessCredentials { uids, gids, groups })
}

fn credential_mode(
    current: &ProcessCredentials,
    target: &Credentials,
) -> anyhow::Result<CredentialMode> {
    if current.uids[1] == 0 {
        return Ok(CredentialMode::SetAndVerify);
    }

    if current.uids != [target.uid; 3] {
        bail!(
            "agent user IDs do not exactly match requested terminal user {}",
            target.uid
        );
    }
    if current.gids != [target.gid; 3] {
        bail!(
            "agent group IDs do not exactly match requested terminal group {}",
            target.gid
        );
    }
    if current.groups != target.groups {
        bail!("agent supplementary groups do not match requested terminal user");
    }
    Ok(CredentialMode::VerifyOnly)
}

fn minimal_environment(username: &CStr, home: &CStr, shell: &CStr) -> anyhow::Result<Vec<CString>> {
    Ok(vec![
        environment_entry(b"HOME", home.to_bytes())?,
        environment_entry(b"USER", username.to_bytes())?,
        environment_entry(b"LOGNAME", username.to_bytes())?,
        environment_entry(b"SHELL", shell.to_bytes())?,
        environment_entry(b"TERM", TERMINAL_TERM)?,
        environment_entry(b"PATH", TERMINAL_PATH)?,
    ])
}

fn environment_entry(name: &[u8], value: &[u8]) -> anyhow::Result<CString> {
    let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
    entry.extend_from_slice(name);
    entry.push(b'=');
    entry.extend_from_slice(value);
    CString::new(entry).context("terminal environment value contains a NUL byte")
}

fn pointers_with_null(values: &[CString]) -> Vec<*const libc::c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildFailure {
    SetGroups,
    SetGid,
    VerifyGid,
    SetUid,
    VerifyUid,
    VerifyGroups,
    Chdir,
    Exec,
}

impl ChildFailure {
    const fn exit_code(self) -> i32 {
        match self {
            Self::SetGroups => CHILD_EXIT_SET_GROUPS,
            Self::SetGid => CHILD_EXIT_SET_GID,
            Self::VerifyGid => CHILD_EXIT_VERIFY_GID,
            Self::SetUid => CHILD_EXIT_SET_UID,
            Self::VerifyUid => CHILD_EXIT_VERIFY_UID,
            Self::VerifyGroups => CHILD_EXIT_VERIFY_GROUPS,
            Self::Chdir => CHILD_EXIT_CHDIR,
            Self::Exec => CHILD_EXIT_EXEC,
        }
    }

    const fn message(self) -> &'static [u8] {
        match self {
            Self::SetGroups => b"nexigon-agent: unable to set terminal supplementary groups\n",
            Self::SetGid => b"nexigon-agent: unable to set terminal group ID\n",
            Self::VerifyGid => b"nexigon-agent: terminal group ID verification failed\n",
            Self::SetUid => b"nexigon-agent: unable to set terminal user ID\n",
            Self::VerifyUid => b"nexigon-agent: terminal user ID verification failed\n",
            Self::VerifyGroups => b"nexigon-agent: terminal group verification failed\n",
            Self::Chdir => b"nexigon-agent: unable to enter terminal home directory\n",
            Self::Exec => b"nexigon-agent: unable to execute terminal shell\n",
        }
    }
}

/// Child operations are separated for deterministic failure injection. The
/// production implementation below consists exclusively of raw Linux syscalls.
trait ChildSyscalls {
    unsafe fn set_groups(&mut self, groups: *const libc::gid_t, count: usize) -> bool;
    unsafe fn set_res_gid(&mut self, gid: libc::gid_t) -> bool;
    unsafe fn read_res_gid(&mut self, gids: &mut [libc::gid_t; 3]) -> bool;
    unsafe fn set_res_uid(&mut self, uid: libc::uid_t) -> bool;
    unsafe fn read_res_uid(&mut self, uids: &mut [libc::uid_t; 3]) -> bool;
    unsafe fn group_count(&mut self) -> Option<usize>;
    unsafe fn read_groups(&mut self, groups: *mut libc::gid_t, count: usize) -> bool;
    unsafe fn change_directory(&mut self, path: *const libc::c_char) -> bool;
    unsafe fn execute(
        &mut self,
        executable: *const libc::c_char,
        argv: *const *const libc::c_char,
        environment: *const *const libc::c_char,
    );
}

struct LinuxChildSyscalls;

impl ChildSyscalls for LinuxChildSyscalls {
    unsafe fn set_groups(&mut self, groups: *const libc::gid_t, count: usize) -> bool {
        // SAFETY: Direct Linux syscall; `groups` has `count` entries.
        unsafe { libc::syscall(libc::SYS_setgroups, count, groups) == 0 }
    }

    unsafe fn set_res_gid(&mut self, gid: libc::gid_t) -> bool {
        // SAFETY: Direct Linux syscall with scalar arguments.
        unsafe { libc::syscall(libc::SYS_setresgid, gid, gid, gid) == 0 }
    }

    unsafe fn read_res_gid(&mut self, gids: &mut [libc::gid_t; 3]) -> bool {
        // SAFETY: Direct Linux syscall writes one value to each valid pointer.
        unsafe {
            let gids = gids.as_mut_ptr();
            libc::syscall(libc::SYS_getresgid, gids, gids.add(1), gids.add(2)) == 0
        }
    }

    unsafe fn set_res_uid(&mut self, uid: libc::uid_t) -> bool {
        // SAFETY: Direct Linux syscall with scalar arguments.
        unsafe { libc::syscall(libc::SYS_setresuid, uid, uid, uid) == 0 }
    }

    unsafe fn read_res_uid(&mut self, uids: &mut [libc::uid_t; 3]) -> bool {
        // SAFETY: Direct Linux syscall writes one value to each valid pointer.
        unsafe {
            let uids = uids.as_mut_ptr();
            libc::syscall(libc::SYS_getresuid, uids, uids.add(1), uids.add(2)) == 0
        }
    }

    unsafe fn group_count(&mut self) -> Option<usize> {
        // SAFETY: A null buffer with length zero requests the current count.
        let count =
            unsafe { libc::syscall(libc::SYS_getgroups, 0usize, ptr::null_mut::<libc::gid_t>()) };
        usize::try_from(count).ok()
    }

    unsafe fn read_groups(&mut self, groups: *mut libc::gid_t, count: usize) -> bool {
        // SAFETY: Direct Linux syscall; `groups` has `count` writable entries.
        unsafe { libc::syscall(libc::SYS_getgroups, count, groups) == count as libc::c_long }
    }

    unsafe fn change_directory(&mut self, path: *const libc::c_char) -> bool {
        // SAFETY: Direct Linux syscall with a valid NUL-terminated path.
        unsafe { libc::syscall(libc::SYS_chdir, path) == 0 }
    }

    unsafe fn execute(
        &mut self,
        executable: *const libc::c_char,
        argv: *const *const libc::c_char,
        environment: *const *const libc::c_char,
    ) {
        // SAFETY: All pointer arrays and C strings were prepared before the fork
        // and remain owned by `PreparedChild` for the duration of this call.
        unsafe {
            libc::syscall(libc::SYS_execve, executable, argv, environment);
        }
    }
}

/// Run the checked child setup. On success `execve` replaces the process and this
/// function never returns. Any returned value is a fail-closed setup error.
unsafe fn configure_and_exec<S: ChildSyscalls>(
    prepared: &mut PreparedChild,
    syscalls: &mut S,
) -> Result<(), ChildFailure> {
    if prepared.credential_mode == CredentialMode::SetAndVerify {
        // SAFETY: The group vector was allocated before fork and remains alive.
        if !unsafe {
            syscalls.set_groups(
                prepared.credentials.groups.as_ptr(),
                prepared.credentials.groups.len(),
            )
        } {
            return Err(ChildFailure::SetGroups);
        }
        // SAFETY: Scalar raw syscall wrapper.
        if !unsafe { syscalls.set_res_gid(prepared.credentials.gid) } {
            return Err(ChildFailure::SetGid);
        }
    }

    let mut actual_gids = [0; 3];
    // SAFETY: The syscall writes to a fixed-size stack array.
    if !unsafe { syscalls.read_res_gid(&mut actual_gids) }
        || actual_gids != [prepared.credentials.gid; 3]
    {
        return Err(ChildFailure::VerifyGid);
    }

    if prepared.credential_mode == CredentialMode::SetAndVerify {
        // SAFETY: Scalar raw syscall wrapper. This occurs only after GID setup.
        if !unsafe { syscalls.set_res_uid(prepared.credentials.uid) } {
            return Err(ChildFailure::SetUid);
        }
    }

    let mut actual_uids = [0; 3];
    // SAFETY: The syscall writes to a fixed-size stack array.
    if !unsafe { syscalls.read_res_uid(&mut actual_uids) }
        || actual_uids != [prepared.credentials.uid; 3]
    {
        return Err(ChildFailure::VerifyUid);
    }

    // SAFETY: A null group buffer with length zero requests the current count.
    let Some(group_count) = (unsafe { syscalls.group_count() }) else {
        return Err(ChildFailure::VerifyGroups);
    };
    if group_count != prepared.credentials.groups.len() {
        return Err(ChildFailure::VerifyGroups);
    }
    if group_count > 0 {
        // SAFETY: The verification buffer was allocated to the target group count
        // before the fork.
        if !unsafe {
            syscalls.read_groups(
                prepared.group_verification_buffer.as_mut_ptr(),
                prepared.group_verification_buffer.len(),
            )
        } || !same_group_set(
            &prepared.group_verification_buffer,
            &prepared.credentials.groups,
        ) {
            return Err(ChildFailure::VerifyGroups);
        }
    }

    // SAFETY: `cwd` is a live NUL-terminated C string.
    if !unsafe { syscalls.change_directory(prepared.cwd.as_ptr()) } {
        return Err(ChildFailure::Chdir);
    }

    // SAFETY: Executable, argv, and environment storage all remain live. A
    // successful syscall never returns.
    unsafe {
        syscalls.execute(
            prepared.executable.as_ptr(),
            prepared.argv_pointers.as_ptr(),
            prepared.environment_pointers.as_ptr(),
        );
    }
    Err(ChildFailure::Exec)
}

fn same_group_set(actual: &[libc::gid_t], expected: &[libc::gid_t]) -> bool {
    actual.len() == expected.len()
        && expected.iter().all(|expected_group| {
            actual
                .iter()
                .any(|actual_group| actual_group == expected_group)
        })
}

/// Enter the production child path. This function never returns and therefore
/// never runs Rust destructors in the post-fork process.
pub(super) unsafe fn enter(prepared: &mut PreparedChild) -> ! {
    let mut syscalls = LinuxChildSyscalls;
    // SAFETY: The caller is the single-threaded `forkpty` child and `prepared`
    // owns every buffer referenced by the raw syscall arguments.
    let failure = match unsafe { configure_and_exec(prepared, &mut syscalls) } {
        Ok(()) => ChildFailure::Exec,
        Err(failure) => failure,
    };
    let message = failure.message();
    // SAFETY: `write` and `_exit` are async-signal-safe; the message is static.
    unsafe {
        libc::syscall(
            libc::SYS_write,
            libc::STDERR_FILENO,
            message.as_ptr(),
            message.len(),
        );
        libc::_exit(failure.exit_code());
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;

    use nix::sys::wait::WaitStatus;
    use nix::unistd::ForkResult;
    use nix::unistd::Uid;

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Operation {
        SetGroups,
        SetGid,
        ReadGid,
        SetUid,
        ReadUid,
        GroupCount,
        ReadGroups,
        Chdir,
        Exec,
    }

    struct FakeSyscalls {
        fail_on: Option<Operation>,
        calls: Vec<Operation>,
        uids: [libc::uid_t; 3],
        gids: [libc::gid_t; 3],
        groups: Vec<libc::gid_t>,
    }

    impl FakeSyscalls {
        fn root(fail_on: Option<Operation>) -> Self {
            Self {
                fail_on,
                calls: Vec::new(),
                uids: [0; 3],
                gids: [0; 3],
                groups: vec![0],
            }
        }

        fn fails(&mut self, operation: Operation) -> bool {
            self.calls.push(operation);
            self.fail_on == Some(operation)
        }
    }

    impl ChildSyscalls for FakeSyscalls {
        unsafe fn set_groups(&mut self, groups: *const libc::gid_t, count: usize) -> bool {
            if self.fails(Operation::SetGroups) {
                return false;
            }
            // SAFETY: Tests pass a valid target group buffer.
            self.groups = unsafe { std::slice::from_raw_parts(groups, count) }.to_vec();
            true
        }

        unsafe fn set_res_gid(&mut self, gid: libc::gid_t) -> bool {
            if self.fails(Operation::SetGid) {
                return false;
            }
            self.gids = [gid; 3];
            true
        }

        unsafe fn read_res_gid(&mut self, gids: &mut [libc::gid_t; 3]) -> bool {
            if self.fails(Operation::ReadGid) {
                return false;
            }
            *gids = self.gids;
            true
        }

        unsafe fn set_res_uid(&mut self, uid: libc::uid_t) -> bool {
            if self.fails(Operation::SetUid) {
                return false;
            }
            self.uids = [uid; 3];
            true
        }

        unsafe fn read_res_uid(&mut self, uids: &mut [libc::uid_t; 3]) -> bool {
            if self.fails(Operation::ReadUid) {
                return false;
            }
            *uids = self.uids;
            true
        }

        unsafe fn group_count(&mut self) -> Option<usize> {
            if self.fails(Operation::GroupCount) {
                return None;
            }
            Some(self.groups.len())
        }

        unsafe fn read_groups(&mut self, groups: *mut libc::gid_t, count: usize) -> bool {
            if self.fails(Operation::ReadGroups) || count != self.groups.len() {
                return false;
            }
            // SAFETY: Tests pass a buffer with exactly `count` writable entries.
            unsafe { std::slice::from_raw_parts_mut(groups, count) }.copy_from_slice(&self.groups);
            true
        }

        unsafe fn change_directory(&mut self, _path: *const libc::c_char) -> bool {
            !self.fails(Operation::Chdir)
        }

        unsafe fn execute(
            &mut self,
            _executable: *const libc::c_char,
            _argv: *const *const libc::c_char,
            _environment: *const *const libc::c_char,
        ) {
            self.fails(Operation::Exec);
        }
    }

    fn current_user() -> User {
        User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("current user must exist")
    }

    fn cstrings(values: &[&str]) -> Vec<CString> {
        values
            .iter()
            .map(|value| CString::new(*value).unwrap())
            .collect()
    }

    fn switched_test_child() -> PreparedChild {
        let user = current_user();
        let mut prepared = prepare_with_argv(&user, "/bin/true", cstrings(&["true"])).unwrap();
        prepared.credentials = Credentials {
            uid: 1001,
            gid: 1002,
            groups: vec![1002, 1003],
        };
        prepared.credential_mode = CredentialMode::SetAndVerify;
        prepared.group_verification_buffer = vec![0; prepared.credentials.groups.len()];
        prepared
    }

    #[test]
    fn every_credential_and_directory_failure_prevents_exec() {
        let cases = [
            (Operation::SetGroups, ChildFailure::SetGroups),
            (Operation::SetGid, ChildFailure::SetGid),
            (Operation::ReadGid, ChildFailure::VerifyGid),
            (Operation::SetUid, ChildFailure::SetUid),
            (Operation::ReadUid, ChildFailure::VerifyUid),
            (Operation::GroupCount, ChildFailure::VerifyGroups),
            (Operation::ReadGroups, ChildFailure::VerifyGroups),
            (Operation::Chdir, ChildFailure::Chdir),
        ];

        for (operation, expected_failure) in cases {
            let mut prepared = switched_test_child();
            let mut syscalls = FakeSyscalls::root(Some(operation));
            // SAFETY: Fake syscalls operate entirely in the test process.
            let failure = unsafe { configure_and_exec(&mut prepared, &mut syscalls) };
            assert_eq!(failure, Err(expected_failure), "fault at {operation:?}");
            assert!(
                !syscalls.calls.contains(&Operation::Exec),
                "exec reached after fault at {operation:?}"
            );
        }
    }

    #[test]
    fn credential_mismatches_prevent_exec() {
        let cases = [
            (
                [1001; 3],
                [55; 3],
                vec![1002, 1003],
                ChildFailure::VerifyGid,
            ),
            (
                [55; 3],
                [1002; 3],
                vec![1002, 1003],
                ChildFailure::VerifyUid,
            ),
            ([1001; 3], [1002; 3], vec![1002], ChildFailure::VerifyGroups),
        ];

        for (uids, gids, groups, expected_failure) in cases {
            let mut prepared = switched_test_child();
            prepared.credential_mode = CredentialMode::VerifyOnly;
            let mut syscalls = FakeSyscalls {
                fail_on: None,
                calls: Vec::new(),
                uids,
                gids,
                groups,
            };

            // SAFETY: Fake syscalls operate entirely in the test process.
            assert_eq!(
                unsafe { configure_and_exec(&mut prepared, &mut syscalls) },
                Err(expected_failure)
            );
            assert!(!syscalls.calls.contains(&Operation::Exec));
        }
    }

    #[test]
    fn an_exec_return_is_a_failure() {
        let mut prepared = switched_test_child();
        let mut syscalls = FakeSyscalls::root(None);
        // SAFETY: Fake syscalls operate entirely in the test process.
        assert_eq!(
            unsafe { configure_and_exec(&mut prepared, &mut syscalls) },
            Err(ChildFailure::Exec)
        );
        assert_eq!(
            syscalls
                .calls
                .iter()
                .filter(|operation| **operation == Operation::Exec)
                .count(),
            1
        );
    }

    #[test]
    fn non_root_process_must_exactly_match_requested_credentials() {
        let target = Credentials {
            uid: 1000,
            gid: 1000,
            groups: vec![10, 1000],
        };
        let matching = ProcessCredentials {
            uids: [1000; 3],
            gids: [1000; 3],
            groups: vec![10, 1000],
        };
        assert_eq!(
            credential_mode(&matching, &target).unwrap(),
            CredentialMode::VerifyOnly
        );

        let mut mismatch = matching.clone();
        mismatch.uids[2] = 0;
        assert!(credential_mode(&mismatch, &target).is_err());
        let mut mismatch = matching.clone();
        mismatch.gids[0] = 5;
        assert!(credential_mode(&mismatch, &target).is_err());
        let mut mismatch = matching;
        mismatch.groups.pop();
        assert!(credential_mode(&mismatch, &target).is_err());
    }

    #[test]
    fn actual_non_root_process_cannot_prepare_another_user() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let Some(user) = User::from_name("daemon").unwrap() else {
            return;
        };
        if user.uid == nix::unistd::geteuid() {
            return;
        }

        assert!(prepare_with_argv(&user, "/bin/sh", cstrings(&["sh"])).is_err());
    }

    #[test]
    fn root_always_resets_credentials() {
        let current = ProcessCredentials {
            uids: [1000, 0, 0],
            gids: [1000, 0, 0],
            groups: vec![0, 1000],
        };
        let target = Credentials {
            uid: 1001,
            gid: 1001,
            groups: vec![1001],
        };
        assert_eq!(
            credential_mode(&current, &target).unwrap(),
            CredentialMode::SetAndVerify
        );
    }

    fn run_child_and_capture(mut prepared: PreparedChild) -> (i32, Vec<u8>) {
        let (read_end, write_end) = nix::unistd::pipe().unwrap();
        let read_fd = read_end.as_raw_fd();
        let write_fd = write_end.as_raw_fd();

        // SAFETY: The child calls only raw descriptor syscalls followed by the
        // production async-signal-safe child entry path.
        match unsafe { nix::unistd::fork() }.unwrap() {
            ForkResult::Child => unsafe {
                if libc::dup2(write_fd, libc::STDOUT_FILENO) == -1 {
                    libc::_exit(119);
                }
                if libc::dup2(write_fd, libc::STDERR_FILENO) == -1 {
                    libc::_exit(119);
                }
                libc::close(read_fd);
                libc::close(write_fd);
                enter(&mut prepared);
            },
            ForkResult::Parent { child } => {
                drop(write_end);
                let mut output = Vec::new();
                File::from(read_end).read_to_end(&mut output).unwrap();
                let status = nix::sys::wait::waitpid(child, None).unwrap();
                let WaitStatus::Exited(_, code) = status else {
                    panic!("child did not exit normally: {status:?}");
                };
                (code, output)
            }
        }
    }

    #[test]
    fn exec_receives_only_the_documented_environment() {
        let user = current_user();
        let prepared = prepare_with_argv(&user, "/usr/bin/env", cstrings(&["env"])).unwrap();
        let expected = prepared
            .environment
            .iter()
            .map(|entry| entry.to_bytes().to_vec())
            .collect::<Vec<_>>();

        let (code, output) = run_child_and_capture(prepared);
        assert_eq!(code, 0);
        let actual = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn chdir_failure_has_deterministic_exit_and_never_executes() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("shell-executed");
        let command = format!("touch {}", marker.display());
        let user = current_user();
        let mut prepared =
            prepare_with_argv(&user, "/bin/sh", cstrings(&["sh", "-c", command.as_str()])).unwrap();
        prepared.cwd = CString::new("/definitely/not/a/real/nexigon/home").unwrap();

        let (code, _) = run_child_and_capture(prepared);
        assert_eq!(code, CHILD_EXIT_CHDIR);
        assert!(!marker.exists());
    }

    #[test]
    fn exec_failure_has_deterministic_exit() {
        let user = current_user();
        let prepared = prepare_with_argv(
            &user,
            "/definitely/not/a/real/nexigon/shell",
            cstrings(&["missing-shell"]),
        )
        .unwrap();

        let (code, output) = run_child_and_capture(prepared);
        assert_eq!(code, CHILD_EXIT_EXEC);
        assert_eq!(output, ChildFailure::Exec.message());
    }

    /// Run with root privileges. An unprivileged Linux builder can use a user
    /// namespace with subordinate-ID mappings. The repository's
    /// `scripts/test-terminal-privileges.sh` wrapper handles both cases.
    #[test]
    #[ignore = "requires root or a user namespace with mapped non-root IDs"]
    fn root_switches_to_exact_non_root_credentials() {
        assert_eq!(nix::unistd::geteuid(), Uid::from_raw(0));
        let user = User::from_name("daemon")
            .unwrap()
            .expect("the root integration test requires the daemon account");
        assert!(!user.uid.is_root());

        let prepared = prepare_with_argv(
            &user,
            "/bin/sh",
            cstrings(&["sh", "-c", "id -u; id -g; id -G"]),
        )
        .unwrap();
        let expected_groups = prepared.credentials.groups.clone();
        let expected_uid = prepared.credentials.uid;
        let expected_gid = prepared.credentials.gid;

        let (code, output) = run_child_and_capture(prepared);
        assert_eq!(code, 0);
        let output = String::from_utf8(output).unwrap();
        let mut lines = output.lines();
        assert_eq!(
            lines.next().unwrap().parse::<libc::uid_t>().unwrap(),
            expected_uid
        );
        assert_eq!(
            lines.next().unwrap().parse::<libc::gid_t>().unwrap(),
            expected_gid
        );
        let mut actual_groups = lines
            .next()
            .unwrap()
            .split_ascii_whitespace()
            .map(|group| group.parse::<libc::gid_t>().unwrap())
            .collect::<Vec<_>>();
        actual_groups.sort_unstable();
        actual_groups.dedup();
        assert_eq!(actual_groups, expected_groups);
        assert!(lines.next().is_none());
    }

    #[test]
    fn target_primary_group_is_always_in_supplementary_set() {
        let user = current_user();
        let username = CString::new(user.name.as_bytes()).unwrap();
        let credentials = credentials_for_user(&user, &username).unwrap();
        assert!(credentials.groups.contains(&credentials.gid));
    }
}
