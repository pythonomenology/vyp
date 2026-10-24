use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Provenance record for a single resolved package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    pub package: String,
    pub version: String,
    pub selected_by: SelectionReason,
    pub requested_by: Vec<String>,
    pub conflict_with: Vec<String>,
    pub resolution_path: Option<String>,
    pub alternatives_rejected: Vec<RejectedAlternative>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum SelectionReason {
    #[default]
    Normal,
    ConflictResolution,
    Override,
    Substitution,
    PluginStrategy(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedAlternative {
    pub version: String,
    pub reason: String,
}

/// Complete provenance data for an entire resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolutionProvenance {
    pub records: HashMap<String, ProvenanceRecord>,
}

impl ResolutionProvenance {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_selection(
        &mut self,
        package: &str,
        version: &str,
        reason: SelectionReason,
        requested_by: Vec<String>,
    ) {
        let record = self
            .records
            .entry(package.to_string())
            .or_insert_with(|| ProvenanceRecord {
                package: package.to_string(),
                version: version.to_string(),
                ..Default::default()
            });
        if !version.is_empty() {
            record.version = version.to_string();
        }
        record.selected_by = reason;
        // Merge rather than replace requested_by
        for r in requested_by {
            if !record.requested_by.contains(&r) {
                record.requested_by.push(r);
            }
        }
    }

    pub fn record_rejection(&mut self, package: &str, version: &str, reason: &str) {
        let record = self.records.entry(package.to_string()).or_default();
        record.alternatives_rejected.push(RejectedAlternative {
            version: version.to_string(),
            reason: reason.to_string(),
        });
    }
}
