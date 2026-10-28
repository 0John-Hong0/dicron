//! Patient-space directions: the three patient axes, the DICOM letter for
//! each end of them, and the axis a direction runs closest to. The 2D and 3D
//! orientation markers all take their letters from here.

/// A patient axis, positive towards the patient's left, posterior or head, as
/// DICOM's patient coordinates are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatientAxis {
    LeftRight,
    AnteriorPosterior,
    SuperiorInferior,
}

/// The patient axes in DICOM's coordinate order, x, y and z.
pub(super) const PATIENT_AXES: [PatientAxis; 3] = [
    PatientAxis::LeftRight,
    PatientAxis::AnteriorPosterior,
    PatientAxis::SuperiorInferior,
];

/// The DICOM patient direction letter for one end of an axis.
pub(crate) fn patient_direction_label(axis: PatientAxis, toward_positive: bool) -> &'static str {
    match (axis, toward_positive) {
        (PatientAxis::LeftRight, true) => "L",
        (PatientAxis::LeftRight, false) => "R",
        (PatientAxis::AnteriorPosterior, true) => "P",
        (PatientAxis::AnteriorPosterior, false) => "A",
        (PatientAxis::SuperiorInferior, true) => "H",
        (PatientAxis::SuperiorInferior, false) => "F",
    }
}

/// The letter of the patient direction a vector in patient coordinates points
/// closest to, or `None` for a degenerate vector.
pub(crate) fn dominant_patient_direction(direction: [f64; 3]) -> Option<&'static str> {
    let (axis, sign) = dominant_axis(direction)?;

    Some(patient_direction_label(PATIENT_AXES[axis], sign > 0.0))
}

/// The patient axis a direction vector runs closest to, as an index into the
/// DICOM patient coordinate order `[x, y, z]` together with the sign of that
/// component (`+1.0` towards left / posterior / head).
///
/// Returns `None` for a degenerate direction: a non-finite dominant component
/// or a vector that is effectively zero.
fn dominant_axis(direction: [f64; 3]) -> Option<(usize, f64)> {
    let (axis, value) = direction
        .into_iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))?;

    if !value.is_finite() || value.abs() < f64::EPSILON {
        return None;
    }

    Some((axis, if value.is_sign_positive() { 1.0 } else { -1.0 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_largest_component_decides_the_axis_and_sign() {
        assert_eq!(dominant_axis([1.0, 0.0, 0.0]), Some((0, 1.0)));
        assert_eq!(dominant_axis([0.0, -0.9, 0.1]), Some((1, -1.0)));
        assert_eq!(dominant_axis([0.2, 0.1, -0.95]), Some((2, -1.0)));
    }

    #[test]
    fn degenerate_directions_have_no_dominant_axis() {
        assert_eq!(dominant_axis([0.0, 0.0, 0.0]), None);
        assert_eq!(dominant_axis([f64::NAN, 1.0, 0.0]), None);
        assert_eq!(dominant_axis([0.0, f64::INFINITY, 0.0]), None);
    }

    /// Each of the six cardinal directions gets its own letter, the one the
    /// MPR edges use for the same end of the same axis.
    #[test]
    fn the_cardinal_directions_get_their_own_letters() {
        let letters: Vec<_> = (0..3)
            .flat_map(|axis| {
                [1.0, -1.0].map(|sign| {
                    let mut direction = [0.0; 3];
                    direction[axis] = sign;
                    let letter = dominant_patient_direction(direction).unwrap();
                    assert_eq!(
                        letter,
                        patient_direction_label(PATIENT_AXES[axis], sign > 0.0)
                    );
                    letter
                })
            })
            .collect();

        assert_eq!(letters, ["L", "R", "P", "A", "H", "F"]);
    }
}
