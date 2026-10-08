#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Anchor {
    Full,
    Empty { measured_ah: Option<f64> },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gauge {
    capacity_ah: Option<f64>,
    held_ah: Option<f64>,
    out_since_full_ah: Option<f64>,
}

impl Gauge {
    pub fn capacity_ah(&self) -> Option<f64> {
        self.capacity_ah
    }

    pub fn set_capacity(&mut self, ah: f64) {
        if ah <= 0.0 || !ah.is_finite() {
            return;
        }
        self.capacity_ah = Some(ah);
        if let Some(out) = self.out_since_full_ah {
            self.held_ah = Some((ah - out).clamp(0.0, ah));
        } else if let Some(held) = self.held_ah {
            self.held_ah = Some(held.min(ah));
        }
    }

    pub fn forget(&mut self) {
        *self = Self::default();
    }

    pub fn count(&mut self, ah: f64) {
        if let Some(out) = self.out_since_full_ah.as_mut() {
            *out = (*out - ah).max(0.0);
        }
        if let (Some(held), Some(cap)) = (self.held_ah.as_mut(), self.capacity_ah) {
            *held = (*held + ah).clamp(0.0, cap);
        }
    }

    pub fn anchor(&mut self, at: Anchor) {
        match at {
            Anchor::Full => {
                self.out_since_full_ah = Some(0.0);
                self.held_ah = self.capacity_ah;
            }
            Anchor::Empty { measured_ah } => {
                if let Some(ah) = measured_ah
                    .into_iter()
                    .chain(self.out_since_full_ah)
                    .find(|ah| *ah > f64::EPSILON)
                {
                    self.set_capacity(ah);
                }
                self.out_since_full_ah = None;
                self.held_ah = self.capacity_ah.map(|_| 0.0);
            }
        }
    }

    pub fn soc(&self) -> Option<f64> {
        let cap = self.capacity_ah?;
        Some(100.0 * self.held_ah? / cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_known_until_a_full_and_an_empty() {
        let mut g = Gauge::default();
        g.count(5.0);
        assert_eq!(g.soc(), None);
        g.anchor(Anchor::Full);
        assert_eq!(g.soc(), None);
        g.count(-48.0);
        g.anchor(Anchor::Empty { measured_ah: None });
        assert_eq!(g.capacity_ah(), Some(48.0));
        assert_eq!(g.soc(), Some(0.0));
        g.count(12.0);
        assert_eq!(g.soc(), Some(25.0));
    }

    #[test]
    fn the_runs_own_count_beats_the_gauges() {
        let mut g = Gauge::default();
        g.anchor(Anchor::Full);
        g.count(-47.0);
        g.anchor(Anchor::Empty {
            measured_ah: Some(46.2),
        });
        assert_eq!(g.capacity_ah(), Some(46.2));
    }

    #[test]
    fn an_empty_without_a_full_first_learns_nothing() {
        let mut g = Gauge::default();
        g.count(-10.0);
        g.anchor(Anchor::Empty { measured_ah: None });
        assert_eq!(g.capacity_ah(), None);
        assert_eq!(g.soc(), None);
    }

    #[test]
    fn a_full_and_an_empty_with_no_current_between_them_learn_nothing() {
        let mut g = Gauge::default();
        g.anchor(Anchor::Full);
        g.anchor(Anchor::Empty {
            measured_ah: Some(0.0),
        });
        assert_eq!(g.capacity_ah(), None);
    }

    #[test]
    fn a_remembered_capacity_waits_for_an_anchor() {
        let mut g = Gauge::default();
        g.set_capacity(50.0);
        assert_eq!(g.soc(), None);
        g.anchor(Anchor::Full);
        assert_eq!(g.soc(), Some(100.0));
        g.count(-12.5);
        assert_eq!(g.soc(), Some(75.0));
        g.count(100.0);
        assert_eq!(g.soc(), Some(100.0));
    }

    #[test]
    fn a_capacity_learned_after_a_full_places_the_pack() {
        let mut g = Gauge::default();
        g.anchor(Anchor::Full);
        g.count(-10.0);
        g.set_capacity(40.0);
        assert_eq!(g.soc(), Some(75.0));
    }

    #[test]
    fn forgetting_drops_the_count_too() {
        let mut g = Gauge::default();
        g.set_capacity(50.0);
        g.anchor(Anchor::Full);
        g.forget();
        assert_eq!(g.capacity_ah(), None);
        assert_eq!(g.soc(), None);
    }
}
