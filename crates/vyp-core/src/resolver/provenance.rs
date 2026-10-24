use std::collections::HashMap;
use vyp_api::{ResolutionProvenance, SelectionReason, VypVersion};

/// Annotates a resolution result with provenance information.
pub fn annotate_provenance(
    solution: &HashMap<String, VypVersion>,
    raw_provenance: &ResolutionProvenance,
) -> ResolutionProvenance {
    let mut provenance = raw_provenance.clone();

    for (pkg_name, version) in solution {
        if !provenance.records.contains_key(pkg_name) {
            provenance.record_selection(
                pkg_name,
                &version.to_string(),
                SelectionReason::Normal,
                Vec::new(),
            );
        }
    }

    provenance
}
