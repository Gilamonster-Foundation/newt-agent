//! Conversation ownership and platform-specific process-liveness probes.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{validate_record_id, ConversationStore};

/// A `live_owners` row (#1030) — a process that has a conversation open —
/// handed to the [`LivenessFn`] to decide whether it is still LIVE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOwner {
    /// Hostname of the owning process's machine.
    pub host: String,
    /// Kernel boot id at claim time. A different boot id on the same host means
    /// the machine rebooted, so every prior pid is gone (the claim is stale).
    pub boot_id: String,
    /// OS process id of the owner.
    pub pid: i64,
    /// The owner's writer fingerprint. Shared per machine (from `identity.pem`),
    /// so it is NOT a process-unique key — stored for provenance, not identity.
    pub writer_fingerprint: String,
    /// Claim-clock tick of the owner's last heartbeat — the freshness signal a
    /// cross-host / post-reboot liveness check falls back to.
    pub heartbeat_tick: i64,
}

/// The outcome of [`ConversationStore::claim`] (#1030 collision fix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This process now owns the conversation — a fresh claim, a re-affirmation
    /// of its own claim, or a reclaim of a stale (crashed/rebooted) owner.
    Claimed,
    /// A DIFFERENT, LIVE process owns it. The fields drive an honest message
    /// ("open in another newt, pid N on host H"); the caller must NOT attach.
    HeldBy { host: String, pid: i64 },
}

impl ConversationStore {
    /// #1030 collision fix: attempt to become the SINGLE live owner of `id`.
    /// Atomic (`BEGIN IMMEDIATE`): if the conversation is unclaimed, or its
    /// claim is our own, or its claim is STALE (the owner is not live — a
    /// crashed or rebooted process), this process takes ownership and returns
    /// [`Claimed`](ClaimOutcome::Claimed). If a DIFFERENT, LIVE process owns it,
    /// returns [`HeldBy`](ClaimOutcome::HeldBy) and writes nothing — the caller
    /// must not attach (attaching is exactly the turn-interleaving bug #1030
    /// fixes). Identity is `host`+`boot_id`+`pid`, never the (machine-shared)
    /// writer fingerprint.
    pub fn claim(&self, id: &str) -> anyhow::Result<ClaimOutcome> {
        // NOT `resolve_id`: a session claims its freshly-minted id at startup,
        // BEFORE the conversation row is lazily created on the first turn.
        // `live_owners` is keyed by the (globally-unique) conversation id with
        // no FK, so the exact id is all that is needed.
        validate_record_id(id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let existing = live_owner_row(&tx, id)?;
        if let Some(owner) = &existing {
            let is_ours =
                owner.host == self.host && owner.boot_id == self.boot_id && owner.pid == self.pid;
            if !is_ours && (self.liveness)(owner, now) {
                return Ok(ClaimOutcome::HeldBy {
                    host: owner.host.clone(),
                    pid: owner.pid,
                });
            }
            // Ours, or a stale remnant of a dead session → fall through and take it.
        }
        tx.execute(
            "INSERT OR REPLACE INTO live_owners
               (conversation_id, host, boot_id, pid, writer_fingerprint, heartbeat_tick)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                self.host,
                self.boot_id,
                self.pid,
                self.writer_fingerprint,
                now
            ],
        )?;
        tx.commit()?;
        Ok(ClaimOutcome::Claimed)
    }

    /// Release THIS process's claim on `id` (best-effort). Only deletes a claim
    /// this exact process holds (`host`+`boot_id`+`pid`), so it can never free
    /// another live session's conversation. Called on clean exit / conversation
    /// switch; a crash simply leaves a stale claim the next [`claim`](Self::claim)
    /// reclaims. A missing / foreign id is a silent no-op.
    pub fn release(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM live_owners
              WHERE conversation_id = ?1 AND host = ?2 AND boot_id = ?3 AND pid = ?4",
            rusqlite::params![id, self.host, self.boot_id, self.pid],
        )?;
        Ok(())
    }

    /// Refresh THIS process's heartbeat on `id` — the freshness signal a
    /// cross-host / post-reboot liveness check reads. Cheap; meant to piggyback
    /// the per-turn save. No-op if this process does not hold the claim.
    pub fn heartbeat(&self, id: &str) -> anyhow::Result<()> {
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE live_owners SET heartbeat_tick = ?2
              WHERE conversation_id = ?1 AND host = ?3 AND boot_id = ?4 AND pid = ?5",
            rusqlite::params![id, now, self.host, self.boot_id, self.pid],
        )?;
        Ok(())
    }

    /// The raw `live_owners` row for `id`, WITHOUT a liveness judgement — `None`
    /// when unclaimed. `/resume` pairs this with [`is_owner_live`](Self::is_owner_live)
    /// to render each conversation's ● live / ○ open marker.
    pub fn live_owner(&self, id: &str) -> anyhow::Result<Option<StoredOwner>> {
        let conn = self.lock_conn();
        live_owner_row(&conn, id)
    }

    /// Whether `owner` is a running process right now, per the store's (injected)
    /// liveness oracle — the SAME judgement [`claim`](Self::claim) uses, exposed
    /// so `/resume` renders a consistent marker.
    #[must_use]
    pub fn is_owner_live(&self, owner: &StoredOwner) -> bool {
        (self.liveness)(owner, (self.claim_clock)())
    }
}

/// Liveness oracle: is `owner` still a running process, as of `now`? Injectable
/// (like the claim clock) so the unit tier is fully mocked — the production
/// [`system_liveness`] touches the OS (pid probe + boot id); a test double
/// decides from the row alone. A plain `fn`, so it carries no captured state.
pub type LivenessFn = fn(owner: &StoredOwner, now: i64) -> bool;

/// A held conversation whose owner's last heartbeat is older than this is
/// treated as stale (reclaimable) — but ONLY on the fallback path where the pid
/// probe is not authoritative (a foreign host, or the same host after a reboot).
/// One hour: comfortably longer than the gap between a live session's per-turn
/// heartbeats, short enough that a genuinely dead cross-host session frees its
/// conversation the same day.
const LIVENESS_STALE_AFTER_NANOS: i64 = 3_600 * 1_000_000_000;

/// The production [`LivenessFn`]. Same machine and boot: the pid probe is
/// authoritative. Otherwise (a foreign host, or this host after a reboot — where
/// the stored pid is meaningless) fall back to heartbeat freshness.
pub(super) fn system_liveness(owner: &StoredOwner, now: i64) -> bool {
    let (host, boot_id) = current_host_boot();
    if owner.host == host && owner.boot_id == boot_id {
        // #1721: pid EXISTENCE is not pid IDENTITY. `pid_max` is commonly ~4M
        // and wraps within hours on a busy machine, so an unrelated process can
        // inherit a dead owner's pid — and the claim would then be judged live
        // forever, wedging the conversation as permanently HeldBy.
        return pid_is_alive(owner.pid)
            && pid_identity_matches(pid_start_unix_nanos(owner.pid), owner.heartbeat_tick);
    }
    now.saturating_sub(owner.heartbeat_tick) < LIVENESS_STALE_AFTER_NANOS
}

/// Does the process now holding `owner.pid` look like the owner that claimed it?
///
/// The owner heartbeats for as long as it runs, so its start time is necessarily
/// EARLIER than its own last heartbeat. A process that started AFTER that
/// heartbeat therefore cannot be the owner — it inherited the pid after a wrap.
///
/// Deliberately NOT a heartbeat-staleness test: a live session can legitimately
/// go a long time between heartbeats (a single long turn), and reclaiming it
/// would reintroduce the #1030 turn-interleaving bug. This compares identity,
/// not freshness, so it never reclaims a running owner however slow it is.
///
/// `None` (start time unreadable — non-Linux, permissions, or a pid that exited
/// mid-probe) fails CLOSED as "still the owner": reclamation requires positive
/// proof of reuse, never the absence of evidence.
pub(super) fn pid_identity_matches(started_at: Option<i64>, heartbeat_tick: i64) -> bool {
    started_at.is_none_or(|started| started <= heartbeat_tick)
}

/// Unix-epoch nanos at which the process holding `pid` started, for comparison
/// against a `live_owners.heartbeat_tick` (also unix nanos, see
/// [`now_claim_nanos`](super::now_claim_nanos)). `/proc/<pid>/stat` field 22 is the start time in clock
/// ticks since boot, which `/proc/stat`'s `btime` rebases onto the wall clock.
///
/// Second-granularity truncation biases the result EARLIER, which is the
/// fail-closed direction: an under-estimate can only make an impostor look like
/// the owner, never make the owner look like an impostor.
#[cfg(target_os = "linux")]
pub(super) fn pid_start_unix_nanos(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` (field 2) is parenthesised and may itself contain spaces and
    // parens, so fields are counted from AFTER its closing paren: the first
    // token there is field 3, making `starttime` (field 22) index 19.
    let after_comm = stat.rsplit_once(')')?.1;
    let start_ticks: i64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;

    // SAFETY: `sysconf` only reads a system constant.
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }

    let btime_secs: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;

    btime_secs
        .checked_add(start_ticks / ticks_per_sec)?
        .checked_mul(1_000_000_000)
}

/// Non-Linux fallback: no portable start-time probe, so identity is unknown and
/// [`pid_identity_matches`] fails closed to today's pid-existence behavior.
#[cfg(not(target_os = "linux"))]
fn pid_start_unix_nanos(_pid: i64) -> Option<i64> {
    None
}

/// Is `pid` a currently-running process? `kill(pid, 0)` delivers no signal but
/// performs the existence + permission check: `0` = alive; `EPERM` = alive but
/// owned by another user (still alive); `ESRCH` = gone.
#[cfg(unix)]
pub(crate) fn pid_is_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 only probes a pid; it never delivers a signal.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(any(windows, test))]
pub(super) fn wait_probe_reports_live_or_unknown(result: u32, wait_object_0: u32) -> bool {
    // Reclamation must fail closed. Only a signalled process handle proves the
    // process exited; timeout means live, while WAIT_FAILED/unknown means the
    // probe could not establish death.
    result != wait_object_0
}

#[cfg(any(windows, test))]
pub(super) fn open_process_failure_reports_live_or_unknown(
    raw_error: Option<i32>,
    error_invalid_parameter: i32,
) -> bool {
    // `ERROR_INVALID_PARAMETER` is Windows' absent-pid result. Every other
    // failure is inconclusive and must block stale-lock reclamation.
    raw_error != Some(error_invalid_parameter)
}

/// Windows analogue of the `kill(pid, 0)` probe above. `OpenProcess` obtains a
/// query handle, but a retained handle can still refer to an exited process, so
/// only a signalled zero-time wait proves exit. Timeout means live; an unknown
/// wait result fails closed as potentially live rather than permitting reclaim.
/// Only `ERROR_INVALID_PARAMETER` proves the pid absent when opening fails;
/// access denial and every unknown/transient failure remain potentially live.
#[cfg(windows)]
pub(crate) fn pid_is_alive(pid: i64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: `OpenProcess` only queries a handle; it takes no action on the
    // target process.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return open_process_failure_reports_live_or_unknown(
            std::io::Error::last_os_error().raw_os_error(),
            ERROR_INVALID_PARAMETER as i32,
        );
    }
    // SAFETY: `handle` is a valid process handle and the zero timeout makes
    // this a non-blocking state probe.
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    let running = wait_probe_reports_live_or_unknown(wait_result, WAIT_OBJECT_0);
    // SAFETY: `handle` was returned by `OpenProcess` and is not used again.
    unsafe { CloseHandle(handle) };
    running
}

/// This machine's `(hostname, kernel boot id)`. Both come from `/proc` (Linux —
/// the dev + CI + deploy target) and degrade to `("localhost", "")` off-Linux,
/// which simply makes the pid probe the sole liveness signal on the local host.
pub(super) fn current_host_boot() -> (String, String) {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    (host, boot_id)
}

/// Read a raw `live_owners` row (no liveness judgement). Shared by `claim`
/// (inside its `BEGIN IMMEDIATE` txn) and `live_owner`.
pub(super) fn live_owner_row(
    conn: &Connection,
    conversation_id: &str,
) -> anyhow::Result<Option<StoredOwner>> {
    conn.query_row(
        "SELECT host, boot_id, pid, writer_fingerprint, heartbeat_tick
           FROM live_owners WHERE conversation_id = ?1",
        rusqlite::params![conversation_id],
        |row| {
            Ok(StoredOwner {
                host: row.get(0)?,
                boot_id: row.get(1)?,
                pid: row.get(2)?,
                writer_fingerprint: row.get(3)?,
                heartbeat_tick: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
