//! Integer-exact spatial predicates for replayable rules.

use crate::QPos;

/// Whether a segment intersects a sphere on the millimetre lattice.
///
/// All differences are widened before subtraction. Products saturate at the
/// `i128` boundary, giving a deterministic fail-closed answer even for hostile
/// coordinates outside a game's ordinary world bounds.
#[must_use]
pub fn segment_intersects_sphere(start: QPos, end: QPos, center: QPos, radius_mm: i64) -> bool {
    if radius_mm < 0 {
        return false;
    }
    let sub = |a: i64, b: i64| i128::from(a).saturating_sub(i128::from(b));
    let ab = [
        sub(end.x, start.x),
        sub(end.y, start.y),
        sub(end.z, start.z),
    ];
    let ac = [
        sub(center.x, start.x),
        sub(center.y, start.y),
        sub(center.z, start.z),
    ];
    let bc = [
        sub(center.x, end.x),
        sub(center.y, end.y),
        sub(center.z, end.z),
    ];
    let dot = |a: [i128; 3], b: [i128; 3]| {
        a.into_iter().zip(b).fold(0i128, |sum, (left, right)| {
            sum.saturating_add(left.saturating_mul(right))
        })
    };
    let ab_sq = dot(ab, ab);
    let radius = i128::from(radius_mm);
    let radius_sq = radius.saturating_mul(radius);
    let projection = dot(ac, ab);
    if ab_sq == 0 || projection <= 0 {
        return dot(ac, ac) <= radius_sq;
    }
    if projection >= ab_sq {
        return dot(bc, bc) <= radius_sq;
    }
    dot(ac, ac)
        .saturating_mul(ab_sq)
        .saturating_sub(projection.saturating_mul(projection))
        <= radius_sq.saturating_mul(ab_sq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: i64, y: i64) -> QPos {
        QPos { x, y, z: 0 }
    }

    #[test]
    fn segment_sphere_handles_middle_endpoints_tangency_and_miss() {
        assert!(segment_intersects_sphere(p(0, 0), p(100, 0), p(50, 9), 10));
        assert!(segment_intersects_sphere(p(0, 0), p(100, 0), p(-5, 0), 5));
        assert!(segment_intersects_sphere(p(0, 0), p(100, 0), p(50, 10), 10));
        assert!(!segment_intersects_sphere(
            p(0, 0),
            p(100, 0),
            p(50, 11),
            10
        ));
    }
}
