//! Deterministic [`HandoffPacket`] assembly for CLI, desktop, and vault export.

use phonton_types::{
    GlobalState, HandoffPacket, OutcomeLedger, TaskId, HANDOFF_PACKET_SCHEMA_VERSION,
};

/// Build a handoff packet from orchestrator global state when present.
pub fn from_global_state(task_id: TaskId, state: &GlobalState) -> Option<HandoffPacket> {
    state.handoff_packet.clone().map(|mut packet| {
        packet.schema_version = HANDOFF_PACKET_SCHEMA_VERSION.to_string();
        packet.task_id = task_id;
        packet
    })
}

/// Serialize a packet for desktop/vault consumers with schema metadata.
pub fn export_json(packet: &HandoffPacket) -> serde_json::Result<String> {
    serde_json::to_string_pretty(packet)
}

/// Merge ledger fields when both state and stored ledger exist.
pub fn enrich_ledger(mut ledger: OutcomeLedger, state: &GlobalState) -> OutcomeLedger {
    if ledger.handoff.is_none() {
        ledger.handoff = from_global_state(ledger.task_id, state);
    }
    if let Some(handoff) = ledger.handoff.as_mut() {
        handoff.schema_version = HANDOFF_PACKET_SCHEMA_VERSION.to_string();
    }
    ledger
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{DiffStats, InfluenceSummary, TaskStatus, TokenUsage, VerifyReport};

    #[test]
    fn export_includes_schema_version() {
        let packet = HandoffPacket {
            schema_version: HANDOFF_PACKET_SCHEMA_VERSION.to_string(),
            task_id: TaskId::new(),
            goal: "test".into(),
            headline: "ok".into(),
            changed_files: Vec::new(),
            generated_artifacts: Vec::new(),
            diff_stats: DiffStats::default(),
            verification: VerifyReport::default(),
            run_commands: Vec::new(),
            known_gaps: Vec::new(),
            review_actions: Vec::new(),
            rollback_points: Vec::new(),
            token_usage: TokenUsage::default(),
            influence: InfluenceSummary::default(),
            screenshot_path: None,
            rendering_summary: None,
        };
        let json = export_json(&packet).unwrap();
        assert!(json.contains("\"schema_version\": \"1\""));
    }
}
