use std::collections::BTreeMap;
use std::time::Duration;

use landlock::{
    ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreated, RulesetStatus, Scope,
};
use rustix::process::{
    DumpableBehavior, Resource, Rlimit, Signal, getrlimit, set_dumpable_behavior,
    set_parent_process_death_signal, setrlimit,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

use crate::helper::MAX_MESSAGE;
use crate::{Error, MAX_PIXELS};

const MEMORY: u64 = 1 << 30;

/// The prefix of a line the helper writes to stderr that is not a failure.
pub const NOTICE: &str = "vigil-scene: notice: ";

/// What the helper does to itself before it reads its input: CPU time just
/// past the deadline, 1 GiB of address space, no descriptors beyond stdio,
/// no file written past `output` bytes plus room for a message, no core
/// dumps and no children.
/// It dies with the thread that started it and can never gain privileges.
/// A soft or hard limit already lower is kept. Where the kernel has
/// Landlock, it also denies itself every file and network access. Landlock
/// is best effort, and a kernel without it is returned as a notice.
pub fn confine(deadline: Duration, output: u64) -> Result<Option<String>, Error> {
    set_parent_process_death_signal(Some(Signal::KILL))
        .map_err(|e| Error::Helper(format!("tying the helper to its caller: {e}")))?;
    let ruleset = landlock_ruleset();
    let cpu = u64::try_from(deadline.as_millis().div_ceil(1000))
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    for (resource, want) in [
        (Resource::Cpu, cpu),
        (Resource::As, MEMORY),
        (Resource::Nofile, 3),
        (Resource::Fsize, output.min(MAX_PIXELS * 4) + MAX_MESSAGE),
        (Resource::Core, 0),
        (Resource::Nproc, 0),
    ] {
        let had = getrlimit(resource);
        let lower = |limit: Option<u64>| Some(limit.map_or(want, |l| l.min(want)));
        setrlimit(
            resource,
            Rlimit {
                current: lower(had.current),
                maximum: lower(had.maximum),
            },
        )
        .map_err(|e| Error::Helper(format!("limiting {resource:?}: {e}")))?;
    }
    let notice = match ruleset.and_then(RulesetCreated::restrict_self) {
        Ok(status) if status.ruleset != RulesetStatus::NotEnforced => None,
        Ok(_) => Some("this kernel has no Landlock".to_owned()),
        Err(e) => Some(format!("Landlock: {e}")),
    };
    rustix::thread::set_no_new_privs(true)
        .map_err(|e| Error::Helper(format!("confining the helper: {e}")))?;
    Ok(notice)
}

/// What the helper does once it holds all its input and before it parses a
/// byte of it, so a bug in a parser or a decoder gains nothing beyond the
/// drawing itself. The process stops being dumpable, so neither ptrace nor
/// /proc/<pid>/mem reaches into it. A seccomp filter, which is required,
/// kills the process at any system call outside what drawing, writing the
/// result and dying need: it cannot open a file, reach a socket, signal or
/// trace another process, or map executable memory. Landlock, applied by
/// `confine`, is the second layer. A panic or a fault once the filter is on
/// is the input's doing, so it ends the process as a refusal or by its own
/// signal, never by SIGSYS, which says the allowlist has a gap. A panic
/// before then is the helper's own fault and exits 101.
pub fn lock_down() -> Result<(), Error> {
    vigil_secret::die_of_faults()
        .map_err(|e| Error::Helper(format!("readying the helper to die of faults: {e}")))?;
    set_dumpable_behavior(DumpableBehavior::NotDumpable)
        .map_err(|e| Error::Helper(format!("making the helper undumpable: {e}")))?;
    let filter = allowlist().map_err(|e| Error::Helper(format!("building the filter: {e}")))?;
    seccompiler::apply_filter_all_threads(&filter)
        .map_err(|e| Error::Helper(format!("installing the filter: {e}")))?;
    vigil_secret::crash_plainly(Error::Refused(String::new()).exit_code());
    Ok(())
}

// The ruleset is a descriptor, so it is made before RLIMIT_NOFILE leaves
// room for none, and applied once the limits are set.
fn landlock_ruleset() -> Result<RulesetCreated, landlock::RulesetError> {
    let abi = ABI::V9;
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .handle_access(AccessNet::from_all(abi))?
        .scope(Scope::from_all(abi))?
        .create()
}

// Derived from strace of every helper job, the refusals, an allocation
// failure under RLIMIT_AS and a stack overflow: after stdin's EOF they make
// only brk, mmap, munmap, write and exit_group, and an abort adds getpid,
// gettid and tgkill. The rest are what glibc and std may also reach for on
// another version. Nothing here takes a path, a socket or another pid, or
// makes a page executable.
const ANY_ARGS: [i64; 15] = [
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_brk,
    libc::SYS_madvise,
    libc::SYS_futex,
    libc::SYS_clock_gettime,
    libc::SYS_getrandom,
    libc::SYS_rt_sigreturn,
    libc::SYS_rt_sigprocmask,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

fn allowlist() -> Result<BpfProgram, seccompiler::BackendError> {
    let only = |conditions| Ok::<_, seccompiler::BackendError>(vec![SeccompRule::new(conditions)?]);
    let arg = |index, op, value| SeccompCondition::new(index, SeccompCmpArgLen::Dword, op, value);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ANY_ARGS.into_iter().map(|nr| (nr, Vec::new())).collect();
    let exec = libc::PROT_EXEC as u64;
    rules.insert(
        libc::SYS_mmap,
        only(vec![arg(2, SeccompCmpOp::MaskedEq(exec), 0)?])?,
    );
    rules.insert(
        libc::SYS_tgkill,
        only(vec![
            arg(0, SeccompCmpOp::Eq, u64::from(std::process::id()))?,
            arg(2, SeccompCmpOp::Eq, libc::SIGABRT as u64)?,
        ])?,
    );
    SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()
}
