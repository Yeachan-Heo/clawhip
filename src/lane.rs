use std::sync::Arc;

use serde_json::json;
use time::format_description::well_known::Rfc3339;

use crate::Result;
use crate::cli::{LaneUpdateKind, LaneWorkflowArg};
use crate::client::DaemonClient;
use crate::config::AppConfig;
use crate::discord::{
    DiscordClient, DiscordLaneDisposition, DiscordThreadMessageFetch, DiscordThreadMessageList,
    DiscordThreadReachability,
};
use crate::source::tmux::{
    BoundedCategory, LaneDeliveryMutation, LaneDetail, LaneLaunchState, LaneRetirementMutation,
    LaneSnapshot, LaneVerificationMutation, LaneVisibility, LaneWorkflow, LaneWorkflowMutation,
    current_timestamp_rfc3339, session_exists,
};
use crate::tmux_wrapper::{LaneInspectionResult, inspect_lane_r2};

const RETIREMENT_GUIDANCE: &str = "lane runtime remains active; quiesce it before retirement";

pub async fn status(
    config: Arc<AppConfig>,
    session: Option<String>,
    json_output: bool,
) -> Result<()> {
    let client = DaemonClient::from_config(config.as_ref());
    let snapshots = client.list_lanes().await?;
    let mut details = Vec::new();
    for snapshot in snapshots {
        if session
            .as_deref()
            .is_some_and(|requested| requested != snapshot.session)
        {
            continue;
        }
        let detail = client.lane_detail(&snapshot.session).await?;
        let current = status_snapshot(&detail);

        if session.is_none() && current.workflow == LaneWorkflow::Retired {
            continue;
        }
        let runtime_live = session_exists(&current.session).await.ok();
        let inspection = if current.durable_launch_state == LaneLaunchState::IdentityVerified {
            Some(inspect_lane_r2(&current.session, current).await)
        } else {
            None
        };
        details.push((detail, runtime_live, inspection));
    }
    if let Some(requested) = session
        && details.is_empty()
    {
        return Err(format!("lane session not found: {requested}").into());
    }
    render_status(&details, json_output)
}

fn status_snapshot(detail: &LaneDetail) -> &LaneSnapshot {
    &detail.snapshot
}

pub async fn verify_thread(
    config: Arc<AppConfig>,
    session: String,
    json_output: bool,
) -> Result<()> {
    let client = DaemonClient::from_config(config.as_ref());
    let detail = client.lane_detail(&session).await?;
    let Some(thread_id) = detail.thread_id.as_deref() else {
        return verification_result(
            &client,
            &detail,
            "unverified",
            Some("thread-target-missing"),
            LaneVisibility::Unverified,
            json_output,
            true,
        )
        .await;
    };
    let discord = DiscordClient::from_config(config)?;
    match discord.probe_thread_reachability(thread_id).await {
        DiscordThreadReachability::Reachable => {}
        DiscordThreadReachability::Unreachable(category) => {
            return verification_result(
                &client,
                &detail,
                "unreachable",
                Some(discord_category(category)),
                LaneVisibility::Unreachable,
                json_output,
                true,
            )
            .await;
        }
        DiscordThreadReachability::Unknown(category) => {
            return verification_result(
                &client,
                &detail,
                "unverified",
                Some(discord_category(category)),
                LaneVisibility::Unverified,
                json_output,
                true,
            )
            .await;
        }
    }

    if let Some(kickoff_id) = detail.kickoff_message_id.as_deref() {
        match discord.fetch_thread_message(thread_id, kickoff_id).await {
            DiscordThreadMessageFetch::Found(_) => {
                verification_result(
                    &client,
                    &detail,
                    "kickoff-visible",
                    None,
                    LaneVisibility::Visible,
                    json_output,
                    false,
                )
                .await
            }
            DiscordThreadMessageFetch::NotFound => {
                verification_result(
                    &client,
                    &detail,
                    "kickoff-missing",
                    Some("kickoff-missing"),
                    LaneVisibility::Unreachable,
                    json_output,
                    true,
                )
                .await
            }
            DiscordThreadMessageFetch::Unavailable(category) => {
                verification_result(
                    &client,
                    &detail,
                    "unverified",
                    Some(discord_category(category)),
                    LaneVisibility::Unverified,
                    json_output,
                    true,
                )
                .await
            }
        }
    } else {
        match discord.list_thread_messages(thread_id).await {
            DiscordThreadMessageList::One(_) => {
                verification_result(
                    &client,
                    &detail,
                    "message-observed-unverified",
                    Some("kickoff-receipt-missing"),
                    LaneVisibility::Unverified,
                    json_output,
                    true,
                )
                .await
            }
            DiscordThreadMessageList::Empty => {
                verification_result(
                    &client,
                    &detail,
                    "unverified",
                    Some("no-message-observed"),
                    LaneVisibility::Unverified,
                    json_output,
                    true,
                )
                .await
            }
            DiscordThreadMessageList::Unavailable(category) => {
                verification_result(
                    &client,
                    &detail,
                    "unverified",
                    Some(discord_category(category)),
                    LaneVisibility::Unverified,
                    json_output,
                    true,
                )
                .await
            }
        }
    }
}

pub async fn update(
    config: Arc<AppConfig>,
    session: String,
    message: String,
    kind: LaneUpdateKind,
    workflow: Option<LaneWorkflowArg>,
    json_output: bool,
) -> Result<()> {
    if message.chars().count() > 2_000 {
        return Err("payload-too-large".into());
    }
    let client = DaemonClient::from_config(config.as_ref());
    let mut detail = client.lane_detail(&session).await?;
    let thread_id = detail
        .thread_id
        .clone()
        .ok_or("stored-thread-target-missing")?;

    let delivery_workflow = delivery_workflow(workflow);

    if let Some(workflow) = workflow {
        if workflow == LaneWorkflowArg::Retired {
            let retirement = LaneRetirementMutation {
                session: session.clone(),
                generation_id: detail.snapshot.generation_id.clone(),
                expected_revision: detail.snapshot.revision,
            };
            let snapshot = client
                .retire_lane(&retirement)
                .await
                .map_err(retirement_error)?;

            detail = client.lane_detail(&session).await?;
            debug_assert_eq!(snapshot.revision, detail.snapshot.revision);
        } else {
            let workflow_mutation = LaneWorkflowMutation {
                session: session.clone(),
                generation_id: detail.snapshot.generation_id.clone(),
                expected_revision: detail.snapshot.revision,
                workflow: workflow.into(),
                quiesced: false,
            };
            let snapshot = client.update_lane_workflow(&workflow_mutation).await?;
            detail = client.lane_detail(&session).await?;
            debug_assert_eq!(snapshot.revision, detail.snapshot.revision);
        }
    }

    let discord = DiscordClient::from_config(config)?;
    match discord.send_thread_with_receipt(&thread_id, &message).await {
        Ok(receipt) => {
            let delivered_at = receipt.timestamp.format(&Rfc3339)?;
            let input = LaneDeliveryMutation {
                session: session.clone(),
                expected_revision: detail.snapshot.revision,
                workflow: delivery_workflow,

                message_id: Some(receipt.message_id.clone()),
                kind: Some(kind.as_str().into()),
                delivered_at: Some(delivered_at.clone()),
                visibility: LaneVisibility::Visible,
                error_category: None,
                disposition: Some("accepted".into()),
                generation_id: detail.snapshot.generation_id.clone(),
                initial_kickoff: lane_update_initial_kickoff(),
                kickoff_operation_id: lane_update_kickoff_operation_id(),
            };
            let snapshot = client.record_lane_delivery(&input).await?;

            render_update(
                &snapshot,
                Some(&receipt.message_id),
                Some(&delivered_at),
                None,
                json_output,
            );
            Ok(())
        }
        Err(error) => {
            let category = BoundedCategory::new(discord_category(error.category));
            let visibility = delivery_failure_visibility(error.disposition);
            let disposition = delivery_disposition_name(error.disposition);
            let input = LaneDeliveryMutation {
                session: session.clone(),
                expected_revision: detail.snapshot.revision,
                workflow: delivery_workflow,

                message_id: None,
                kind: Some(kind.as_str().into()),
                delivered_at: None,
                visibility,
                error_category: Some(category.clone()),
                disposition: Some(disposition.into()),
                generation_id: detail.snapshot.generation_id.clone(),
                initial_kickoff: lane_update_initial_kickoff(),
                kickoff_operation_id: lane_update_kickoff_operation_id(),
            };
            let snapshot = client.record_lane_delivery(&input).await?;

            render_update(&snapshot, None, None, Some(category.as_str()), json_output);
            Err("lane update delivery failed".into())
        }
    }
}

async fn verification_result(
    client: &DaemonClient,
    detail: &LaneDetail,
    outcome: &str,
    reason: Option<&str>,
    visibility: LaneVisibility,
    json_output: bool,
    failed: bool,
) -> Result<()> {
    let input = LaneVerificationMutation {
        session: detail.snapshot.session.clone(),
        expected_revision: detail.snapshot.revision,
        checked_at: current_timestamp_rfc3339(),
        outcome: outcome.into(),
        reason: reason.map(crate::source::tmux::BoundedReason::new),
        visibility,
        generation_id: detail.snapshot.generation_id.clone(),
    };
    let snapshot = client.record_lane_verification(&input).await?;
    let value = json!({
        "session": snapshot.session,
        "outcome": outcome,
        "reason": reason,
        "visibility": visibility_name(visibility),
        "workflow": workflow_name(snapshot.workflow),
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("SESSION\tOUTCOME\tVISIBILITY\tREASON");
        println!(
            "{}\t{}\t{}\t{}",
            snapshot.session,
            outcome,
            visibility_name(visibility),
            reason.unwrap_or("—")
        );
    }
    if failed {
        Err("thread verification is unverified".into())
    } else {
        Ok(())
    }
}

fn render_status(
    details: &[(LaneDetail, Option<bool>, Option<LaneInspectionResult>)],
    json_output: bool,
) -> Result<()> {
    if json_output {
        let lanes: Vec<_> = details
            .iter()
            .map(|(detail, runtime_live, inspection)| {
                status_json(detail, *runtime_live, inspection.as_ref())
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&lanes)?);
        return Ok(());
    }
    println!("SESSION\tWORKFLOW\tRUNTIME\tSTATUS\tVISIBILITY\tWORKER\tCHECKED-AT\tREASON");
    for (detail, runtime_live, inspection) in details {
        let snapshot = &detail.snapshot;
        let verification = detail.verification.as_ref();
        let derived_status = inspection
            .as_ref()
            .and_then(|value| value.derived_status.as_deref())
            .or_else(|| launched_runtime_status(snapshot, *runtime_live))
            .or(snapshot.derived_status.as_deref())
            .unwrap_or("—");
        let worker_started = inspection
            .as_ref()
            .map(|value| value.worker_started)
            .unwrap_or(snapshot.worker_started);
        let observation_reason = inspection
            .as_ref()
            .and_then(|value| value.observation_reason.as_deref())
            .or_else(|| {
                snapshot
                    .observation_reason
                    .as_ref()
                    .map(|reason| reason.as_str())
            })
            .or_else(|| {
                verification.and_then(|item| item.reason.as_ref().map(|reason| reason.as_str()))
            })
            .unwrap_or("—");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            snapshot.session,
            workflow_name(snapshot.workflow),
            runtime_name(*runtime_live),
            derived_status,
            snapshot.visibility.map(visibility_name).unwrap_or("—"),
            worker_name(worker_started),
            verification
                .map(|item| item.checked_at.as_str())
                .unwrap_or("—"),
            observation_reason,
        );
    }
    Ok(())
}

fn status_json(
    detail: &LaneDetail,
    runtime_live: Option<bool>,
    inspection: Option<&LaneInspectionResult>,
) -> serde_json::Value {
    let snapshot = &detail.snapshot;
    json!({
        "session": snapshot.session,
        "worker_effect_kind": worker_effect_name(snapshot.worker_effect_kind),
        "durable_launch_state": launch_state_name(snapshot.durable_launch_state),
        "workflow": workflow_name(snapshot.workflow),
        "runtime": runtime_name(runtime_live),

        "derived_status": inspection.as_ref().and_then(|value| value.derived_status.as_deref()).or_else(|| launched_runtime_status(snapshot, runtime_live)).or(snapshot.derived_status.as_deref()),
        "observation_reason": inspection.as_ref().and_then(|value| value.observation_reason.as_deref()).or_else(|| snapshot.observation_reason.as_ref().map(|reason| reason.as_str())),
        "worker_started": inspection.as_ref().map(|value| value.worker_started).unwrap_or(snapshot.worker_started),
        "evidence_commit": inspection.as_ref().map(|value| value.evidence_commit).unwrap_or(snapshot.evidence_commit),
        "repair_needed": inspection.as_ref().map(|value| value.repair_needed).unwrap_or(snapshot.repair_needed),
        "healthy": inspection.as_ref().map(|value| value.healthy).unwrap_or_else(|| launched_runtime_healthy(snapshot, runtime_live)),
        "exit_category": inspection.as_ref().and_then(|value| value.exit_category.as_deref()).or_else(|| snapshot.exit_category.as_ref().map(|category| category.as_str())),

        "visibility": snapshot.visibility.map(visibility_name),
        "checked_at": detail.verification.as_ref().map(|item| item.checked_at.as_str()),
        "verification_outcome": detail.verification.as_ref().map(|item| item.outcome.as_str()),
        "verification_reason": detail.verification.as_ref().and_then(|item| item.reason.as_ref().map(|reason| reason.as_str())),
    })
}

fn render_update(
    snapshot: &LaneSnapshot,
    message_id: Option<&str>,
    delivered_at: Option<&str>,
    error: Option<&str>,
    json_output: bool,
) {
    let value = json!({
        "session": snapshot.session,
        "workflow": workflow_name(snapshot.workflow),
        "visibility": snapshot.visibility.map(visibility_name),
        "message_id": message_id,
        "delivered_at": delivered_at,
        "error_category": error,
    });
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("lane update JSON is serializable")
        );
    } else {
        println!("SESSION\tWORKFLOW\tVISIBILITY\tDELIVERY");

        println!(
            "{}\t{}\t{}\t{}",
            snapshot.session,
            workflow_name(snapshot.workflow),
            snapshot.visibility.map(visibility_name).unwrap_or("—"),
            error.unwrap_or("accepted")
        );
    }
}

fn delivery_workflow(workflow: Option<LaneWorkflowArg>) -> Option<LaneWorkflow> {
    match workflow {
        Some(LaneWorkflowArg::Retired) => Some(LaneWorkflow::Retired),
        _ => None,
    }
}

fn lane_update_initial_kickoff() -> bool {
    false
}

fn lane_update_kickoff_operation_id() -> Option<String> {
    None
}

fn runtime_name(runtime_live: Option<bool>) -> &'static str {
    match runtime_live {
        Some(true) => "live",
        Some(false) => "tmux-missing",
        None => "unknown",
    }
}

fn launched_runtime_status(
    snapshot: &LaneSnapshot,
    runtime_live: Option<bool>,
) -> Option<&'static str> {
    if snapshot.durable_launch_state != LaneLaunchState::Launched || runtime_live != Some(false) {
        return None;
    }
    match snapshot.workflow {
        LaneWorkflow::Active => Some("tmux-missing"),
        LaneWorkflow::NeedsReview
        | LaneWorkflow::NeedsQa
        | LaneWorkflow::PrOpen
        | LaneWorkflow::AwaitingCi
        | LaneWorkflow::AwaitingHuman => Some("workflow-handoff"),
        LaneWorkflow::Retired => None,
    }
}

fn launched_runtime_healthy(snapshot: &LaneSnapshot, runtime_live: Option<bool>) -> bool {
    if snapshot.durable_launch_state != LaneLaunchState::Launched {
        return snapshot.healthy;
    }
    snapshot.workflow == LaneWorkflow::Active
        && runtime_live == Some(true)
        && snapshot.visibility == Some(LaneVisibility::Visible)
}

fn delivery_failure_visibility(disposition: DiscordLaneDisposition) -> LaneVisibility {
    match disposition {
        DiscordLaneDisposition::DefinitiveFailure => LaneVisibility::DeliveryFailed,
        DiscordLaneDisposition::AmbiguousAcceptance => LaneVisibility::Unverified,
    }
}

fn delivery_disposition_name(disposition: DiscordLaneDisposition) -> &'static str {
    match disposition {
        DiscordLaneDisposition::DefinitiveFailure => "definitive-failure",
        DiscordLaneDisposition::AmbiguousAcceptance => "ambiguous-acceptance",
    }
}

fn worker_name(worker_started: Option<bool>) -> &'static str {
    match worker_started {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn workflow_name(workflow: LaneWorkflow) -> &'static str {
    match workflow {
        LaneWorkflow::Active => "active",
        LaneWorkflow::NeedsReview => "needs-review",
        LaneWorkflow::NeedsQa => "needs-qa",
        LaneWorkflow::PrOpen => "pr-open",
        LaneWorkflow::AwaitingCi => "awaiting-ci",
        LaneWorkflow::AwaitingHuman => "awaiting-human",
        LaneWorkflow::Retired => "retired",
    }
}

fn visibility_name(visibility: LaneVisibility) -> &'static str {
    match visibility {
        LaneVisibility::Unverified => "unverified",
        LaneVisibility::Visible => "visible",
        LaneVisibility::Unreachable => "unreachable",
        LaneVisibility::DeliveryFailed => "delivery-failed",
    }
}

fn worker_effect_name(kind: crate::source::tmux::WorkerEffectKind) -> &'static str {
    match kind {
        crate::source::tmux::WorkerEffectKind::CommandSubmission => "command-submission",
        crate::source::tmux::WorkerEffectKind::SessionCreation => "session-creation",
    }
}

fn launch_state_name(state: crate::source::tmux::LaneLaunchState) -> &'static str {
    match state {
        crate::source::tmux::LaneLaunchState::Ready => "ready",
        crate::source::tmux::LaneLaunchState::Claimed => "claimed",
        crate::source::tmux::LaneLaunchState::IdentityVerified => "identity-markers-verified",
        crate::source::tmux::LaneLaunchState::Launched => "launched",
        crate::source::tmux::LaneLaunchState::NoWorkerEffect => "launch-failed-no-worker-effect",

        crate::source::tmux::LaneLaunchState::CommandSubmitAmbiguous => {
            "blocked-command-submit-ambiguous"
        }
        crate::source::tmux::LaneLaunchState::SessionCreationAmbiguous => {
            "blocked-session-creation-ambiguous"
        }
    }
}

fn discord_category(category: crate::discord::DiscordLaneErrorCategory) -> &'static str {
    match category {
        crate::discord::DiscordLaneErrorCategory::ContentTooLong => "payload-too-large",
        crate::discord::DiscordLaneErrorCategory::BadRequest => "bad-request",
        crate::discord::DiscordLaneErrorCategory::Unauthorized => "unauthorized",
        crate::discord::DiscordLaneErrorCategory::Forbidden => "forbidden",
        crate::discord::DiscordLaneErrorCategory::NotFound => "not-found",
        crate::discord::DiscordLaneErrorCategory::RateLimited => "rate-limited",
        crate::discord::DiscordLaneErrorCategory::ArchivedOrNotWritable => {
            "archived-or-not-writable"
        }
        crate::discord::DiscordLaneErrorCategory::Timeout => "timeout",
        crate::discord::DiscordLaneErrorCategory::Transport => "transport",
        crate::discord::DiscordLaneErrorCategory::MalformedSuccess => "malformed-success",
        crate::discord::DiscordLaneErrorCategory::EmptyMessageId => "empty-message-id",
        crate::discord::DiscordLaneErrorCategory::MissingMessageId => "missing-message-id",
    }
}

impl LaneUpdateKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Kickoff => "kickoff",
            Self::Progress => "progress",
            Self::Blocked => "blocked",
            Self::PrOpen => "pr-open",
            Self::Handoff => "handoff",
        }
    }
}

impl From<LaneWorkflowArg> for LaneWorkflow {
    fn from(value: LaneWorkflowArg) -> Self {
        match value {
            LaneWorkflowArg::Active => Self::Active,
            LaneWorkflowArg::NeedsReview => Self::NeedsReview,
            LaneWorkflowArg::NeedsQa => Self::NeedsQa,
            LaneWorkflowArg::PrOpen => Self::PrOpen,
            LaneWorkflowArg::AwaitingCi => Self::AwaitingCi,
            LaneWorkflowArg::AwaitingHuman => Self::AwaitingHuman,
            LaneWorkflowArg::Retired => Self::Retired,
        }
    }
}

fn retirement_error(error: crate::DynError) -> crate::DynError {
    if error.to_string().contains("lane runtime remains active") {
        RETIREMENT_GUIDANCE.into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_launch_state_names_match_canonical_launch_surface() {
        assert_eq!(
            launch_state_name(LaneLaunchState::NoWorkerEffect),
            "launch-failed-no-worker-effect",
        );
        assert_eq!(
            launch_state_name(LaneLaunchState::SessionCreationAmbiguous),
            "blocked-session-creation-ambiguous",
        );
    }
    #[test]
    fn lane_update_kickoff_kind_is_never_an_initial_kickoff() {
        assert!(!lane_update_initial_kickoff());
        assert_eq!(lane_update_kickoff_operation_id(), None);
    }

    #[test]
    fn status_uses_detail_snapshot_identity_over_list_snapshot() {
        let mut detail = LaneDetail {
            snapshot: snapshot_with_status(None),
            thread_id: None,
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            verification: None,
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            last_error: None,
            quiesced: false,
        };
        detail.snapshot.generation_id = "current-generation".into();
        detail.snapshot.revision = 9;
        let current = status_snapshot(&detail);
        assert_eq!(current.generation_id, "current-generation");
        assert_eq!(current.revision, 9);
    }

    #[test]
    fn runtime_rendering_preserves_missing_and_unknown_liveness() {
        assert_eq!(runtime_name(Some(false)), "tmux-missing");
        assert_eq!(runtime_name(None), "unknown");
    }

    #[test]
    fn retirement_runtime_error_includes_manual_quiescence_guidance() {
        let error = retirement_error("lane runtime remains active".into());
        assert_eq!(error.to_string(), RETIREMENT_GUIDANCE);
    }

    #[test]
    fn update_ambiguity_persists_unverified_visibility_and_canonical_disposition() {
        assert_eq!(
            delivery_failure_visibility(DiscordLaneDisposition::DefinitiveFailure),
            LaneVisibility::DeliveryFailed,
        );
        assert_eq!(
            delivery_failure_visibility(DiscordLaneDisposition::AmbiguousAcceptance),
            LaneVisibility::Unverified,
        );
        assert_eq!(
            delivery_disposition_name(DiscordLaneDisposition::DefinitiveFailure),
            "definitive-failure",
        );
        assert_eq!(
            delivery_disposition_name(DiscordLaneDisposition::AmbiguousAcceptance),
            "ambiguous-acceptance",
        );
    }

    #[test]
    fn retired_update_delivery_repeats_only_the_retired_workflow_exception() {
        assert_eq!(
            delivery_workflow(Some(LaneWorkflowArg::Retired)),
            Some(LaneWorkflow::Retired),
        );
        assert_eq!(
            delivery_workflow(Some(LaneWorkflowArg::AwaitingHuman)),
            None,
        );
        assert_eq!(delivery_workflow(None), None);
    }

    #[test]
    fn launched_runtime_overlay_distinguishes_active_missing_from_handoff_and_health() {
        let mut snapshot = snapshot_with_status(None);
        assert_eq!(
            launched_runtime_status(&snapshot, Some(false)),
            Some("tmux-missing")
        );
        assert!(launched_runtime_healthy(&snapshot, Some(true)));
        assert!(!launched_runtime_healthy(&snapshot, Some(false)));

        snapshot.workflow = LaneWorkflow::NeedsReview;
        assert_eq!(
            launched_runtime_status(&snapshot, Some(false)),
            Some("workflow-handoff")
        );
        assert!(!launched_runtime_healthy(&snapshot, Some(true)));

        snapshot.workflow = LaneWorkflow::Active;
        snapshot.visibility = Some(LaneVisibility::Unverified);
        assert!(!launched_runtime_healthy(&snapshot, Some(true)));
        assert_eq!(launched_runtime_status(&snapshot, Some(true)), None);
    }

    #[test]
    fn status_json_does_not_include_private_thread_target() {
        let detail = LaneDetail {
            snapshot: snapshot_with_status(None),
            thread_id: Some("private-thread".into()),
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            verification: None,
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            last_error: None,
            quiesced: false,
        };
        assert!(
            !status_json(&detail, Some(true), None)
                .to_string()
                .contains("private-thread")
        );
    }

    #[test]
    fn r2_overlay_wins_status_rendering_without_private_target() {
        let mut snapshot = snapshot_with_status(Some("launch-commit-ambiguous"));
        snapshot.durable_launch_state = LaneLaunchState::IdentityVerified;
        let detail = LaneDetail {
            snapshot,
            thread_id: Some("private-thread".into()),
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            verification: None,
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            last_error: None,
            quiesced: false,
        };
        let overlay = LaneInspectionResult {
            derived_status: Some("identity-conflict".into()),
            observation_reason: Some("identity-mismatch".into()),
            worker_started: None,
            evidence_commit: false,
            repair_needed: true,
            healthy: false,
            exit_category: Some("identity-mismatch".into()),
            runtime: "tmux-missing",
        };
        let rendered = status_json(&detail, Some(false), Some(&overlay));
        assert_eq!(rendered["runtime"], "tmux-missing");
        assert_eq!(rendered["derived_status"], "identity-conflict");
        assert_eq!(rendered["observation_reason"], "identity-mismatch");
        assert_eq!(rendered["worker_started"], serde_json::Value::Null);
        assert_eq!(rendered["exit_category"], "identity-mismatch");
        assert!(!rendered.to_string().contains("private-thread"));
    }

    fn snapshot_with_status(status: Option<&str>) -> LaneSnapshot {
        LaneSnapshot {
            session: "lane".into(),
            generation_id: "g".into(),
            kickoff_operation_id: "k".into(),
            launch_operation_id: "l".into(),
            executor_id: "e".into(),
            worker_effect_kind: crate::source::tmux::WorkerEffectKind::CommandSubmission,
            durable_launch_state: crate::source::tmux::LaneLaunchState::Launched,
            derived_status: status.map(str::to_owned),
            observation_reason: None,
            worker_started: Some(true),
            evidence_commit: true,
            repair_needed: false,
            healthy: true,
            exit_category: None,
            workflow: LaneWorkflow::Active,
            visibility: Some(LaneVisibility::Visible),
            revision: 1,
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
        }
    }
}
