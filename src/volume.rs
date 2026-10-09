/// Linear gain in `[0.0, 1.0]`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Volume(f32);

impl Volume {
    pub const FULL: Self = Self(1.0);

    pub fn new(value: f32) -> Self {
        Self(if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    pub fn as_gain(self) -> f32 {
        self.0
    }

    pub fn percent(self) -> u8 {
        (self.0 * 100.0).round() as u8
    }

    pub fn adjusted(self, delta: f32) -> Self {
        Self::new(self.0 + delta)
    }
}

impl Default for Volume {
    fn default() -> Self {
        Self::FULL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_and_non_finite_values_are_tamed() {
        assert_eq!(Volume::new(2.0), Volume::FULL);
        assert_eq!(Volume::new(-1.0).as_gain(), 0.0);
        assert_eq!(Volume::new(f32::NAN).as_gain(), 0.0);
    }

    #[test]
    fn adjustment_saturates_rather_than_wrapping() {
        assert_eq!(Volume::FULL.adjusted(0.5), Volume::FULL);
        assert_eq!(Volume::new(0.1).adjusted(-0.5).as_gain(), 0.0);
        assert_eq!(Volume::new(0.5).percent(), 50);
    }
}
