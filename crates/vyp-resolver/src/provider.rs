use version_ranges::Ranges;

use vyp_api::VypVersion;

pub type VypVS = Ranges<VypVersion>;

/// Convert requirement constraints to version Ranges.
///
/// Follows uv's version range conversion semantics for correctness:
/// - `<V` excludes pre-releases of V (uses V.dev0 as boundary)
/// - `>V` excludes post-releases of V (uses V.post(MAX) as boundary)
/// - `<=V` includes local versions of V
/// - `==X.Y.*` / `!=X.Y.*` use range-based wildcard matching
pub fn requirements_to_range(
    constraints: &[vyp_api::types::requirement::VersionConstraint],
) -> VypVS {
    use vyp_api::ComparisonOp;

    if constraints.is_empty() {
        return Ranges::full();
    }

    let mut range: VypVS = Ranges::full();
    for c in constraints {
        let constraint_range = match c.op {
            ComparisonOp::Gte => Ranges::higher_than(c.version.clone()),
            ComparisonOp::Gt => {
                if c.version.post.is_none() && c.version.dev.is_none() && c.version.local.is_empty()
                {
                    Ranges::strictly_higher_than(c.version.clone().with_post_max())
                } else {
                    Ranges::strictly_higher_than(c.version.clone())
                }
            }
            ComparisonOp::Lte => Ranges::lower_than(c.version.clone().with_local_max()),
            ComparisonOp::Lt => {
                if c.version.pre.is_none() && c.version.dev.is_none() && c.version.local.is_empty()
                {
                    Ranges::strictly_lower_than(c.version.clone().with_dev(0))
                } else {
                    Ranges::strictly_lower_than(c.version.clone())
                }
            }
            ComparisonOp::Eq => Ranges::singleton(c.version.clone()),
            ComparisonOp::NotEq => Ranges::singleton(c.version.clone()).complement(),
            ComparisonOp::EqStar => {
                let lower = c.version.clone().with_dev(0);
                let mut upper_segs = c.version.release.clone();
                if let Some(last) = upper_segs.last_mut() {
                    *last += 1;
                }
                let upper = VypVersion::new(upper_segs).with_dev(0);
                Ranges::from_range_bounds(lower..upper)
            }
            ComparisonOp::NotEqStar => {
                let lower = c.version.clone().with_dev(0);
                let mut upper_segs = c.version.release.clone();
                if let Some(last) = upper_segs.last_mut() {
                    *last += 1;
                }
                let upper = VypVersion::new(upper_segs).with_dev(0);
                Ranges::from_range_bounds(lower..upper).complement()
            }
            ComparisonOp::Compatible => {
                let lower = Ranges::higher_than(c.version.clone());
                let rel = &c.version.release;
                if rel.len() >= 2 {
                    let mut upper_segs = rel[..rel.len() - 1].to_vec();
                    if let Some(last) = upper_segs.last_mut() {
                        *last += 1;
                    }
                    let upper_version = VypVersion::new(upper_segs);
                    let upper = Ranges::strictly_lower_than(upper_version);
                    lower.intersection(&upper)
                } else {
                    lower
                }
            }
            ComparisonOp::ArbitraryEq => Ranges::singleton(c.version.clone()),
        };
        range = range.intersection(&constraint_range);
    }
    range
}
