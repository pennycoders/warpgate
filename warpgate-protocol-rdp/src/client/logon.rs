//! The target's own sign-in state, as reported by the Save Session Info PDU
//! ([MS-RDPBCGR] 2.2.10.1). Until the target sends it, the session is sitting at the
//! target's sign-in screen — which is where the user types credentials that Warpgate never
//! sees, and which the recording must not keep.

use ironrdp_pdu::rdp::session_info::LogonInfo;
use ironrdp_session::ActiveStageOutput;
use uuid::Uuid;
use warpgate_common::TargetSessionId;
use warpgate_common::audit::AuditEvent;
use warpgate_core::LogonState;

pub(crate) struct LogonWatcher {
    pub target_session_id: TargetSessionId,
    pub target_id: Uuid,
    pub target_name: String,
    pub user_id: Uuid,
    pub username: String,
    pub sign_in: LogonState,
}

impl LogonWatcher {
    /// Reports every completed logon among the outputs of one processed PDU batch.
    pub fn observe(&self, outputs: &[ActiveStageOutput]) {
        for info in completed_logons(outputs) {
            self.notify_logon(info);
        }
    }

    /// Called for a Save Session Info PDU that reports a completed logon, with the logon
    /// information it carries (Logon Info Version 1 or 2; a plain notification has none).
    fn notify_logon(&self, info: Option<&LogonInfo>) {
        self.sign_in.mark_logged_on();

        AuditEvent::TargetLogon {
            session_id: self.target_session_id.0,
            target_id: self.target_id,
            target_name: self.target_name.clone(),
            user_id: self.user_id,
            username: self.username.clone(),
            target_account: info.and_then(account),
            target_session_id: info.map(|info| info.session_id),
        }
        .emit();
    }
}

/// One entry per Save Session Info PDU that reports a completed logon, in wire order, with
/// the logon information it carries. The session emits `LogonInfo` ahead of the
/// `SaveSessionInfo` of the same PDU, and only for Logon Info Version 1 or 2; a plain
/// notification is reported without it. A `LogonInfo` is never carried over to another
/// PDU's notification.
fn completed_logons(outputs: &[ActiveStageOutput]) -> Vec<Option<&LogonInfo>> {
    let mut logons = Vec::new();
    let mut pending = None;
    for output in outputs {
        match output {
            ActiveStageOutput::LogonInfo(info) => pending = Some(info),
            ActiveStageOutput::SaveSessionInfo { logon_complete } => {
                let info = pending.take();
                if *logon_complete {
                    logons.push(info);
                }
            }
            _ => {}
        }
    }
    logons
}

fn account(info: &LogonInfo) -> Option<String> {
    if info.user_name.is_empty() {
        return None;
    }
    Some(if info.domain_name.is_empty() {
        info.user_name.clone()
    } else {
        format!("{}\\{}", info.domain_name, info.user_name)
    })
}

#[cfg(test)]
mod tests {
    use ironrdp_pdu::rdp::session_info::LogonInfo;
    use ironrdp_session::ActiveStageOutput;

    use super::{account, completed_logons};

    fn info(session_id: u32, domain_name: &str, user_name: &str) -> LogonInfo {
        LogonInfo {
            session_id,
            user_name: user_name.to_owned(),
            domain_name: domain_name.to_owned(),
        }
    }

    #[test]
    fn logon_info_is_paired_with_its_own_notification() {
        let outputs = [
            ActiveStageOutput::SessionId(3),
            ActiveStageOutput::LogonInfo(info(3, "CORP", "alice")),
            ActiveStageOutput::SaveSessionInfo {
                logon_complete: true,
            },
            // Plain notification: completed, but no account.
            ActiveStageOutput::SaveSessionInfo {
                logon_complete: true,
            },
        ];
        let logons = completed_logons(&outputs);
        assert_eq!(logons, [Some(&info(3, "CORP", "alice")), None]);
    }

    #[test]
    fn incomplete_notifications_are_not_logons() {
        let outputs = [
            // Extended logon info: not a completed logon.
            ActiveStageOutput::SaveSessionInfo {
                logon_complete: false,
            },
            ActiveStageOutput::PointerDefault,
        ];
        assert!(completed_logons(&outputs).is_empty());
    }

    #[test]
    fn logon_info_does_not_leak_into_a_later_notification() {
        let outputs = [
            ActiveStageOutput::LogonInfo(info(1, "", "bob")),
            ActiveStageOutput::SaveSessionInfo {
                logon_complete: false,
            },
            ActiveStageOutput::SaveSessionInfo {
                logon_complete: true,
            },
        ];
        assert_eq!(completed_logons(&outputs), [None]);
    }

    #[test]
    fn account_includes_the_domain_when_present() {
        assert_eq!(
            account(&info(1, "CORP", "alice")).as_deref(),
            Some("CORP\\alice")
        );
        assert_eq!(account(&info(1, "", "alice")).as_deref(), Some("alice"));
        assert_eq!(account(&info(1, "CORP", "")), None);
    }
}
