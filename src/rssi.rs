//! Median RSSI for one device over one two-second slot.
//! Counts occupy one bin per whole dBm from −100 to −1, the range ingest accepts.

/// Lowest stored strength. Bin 0 is this value.
const RSSI_MIN: i8 = -100;
/// One bin per dBm through −1 inclusive.
const BINS: usize = 100;

/// Frame counts for one `(device, slot)`. The median is taken when the slot closes.
pub struct RssiHist {
    counts: [u16; BINS],
    total: u16,
}

impl Default for RssiHist {
    fn default() -> Self {
        Self {
            counts: [0; BINS],
            total: 0,
        }
    }
}

impl RssiHist {
    /// Count one frame. Strengths outside −100..=−1 are ignored, because ingest drops them.
    /// A bin that reaches `u16::MAX` stops growing; a two-second slot does not get there.
    pub fn record(&mut self, rssi: i8) -> bool {
        let Some(bin) = bin(rssi) else {
            return false;
        };
        if self.counts[bin] == u16::MAX || self.total == u16::MAX {
            return false;
        }
        self.counts[bin] += 1;
        self.total += 1;
        true
    }

    /// Middle frame by strength. An even count averages the two central frames and rounds
    /// to the nearest dBm. Both strengths are negative, so a halfway value rounds weaker.
    pub fn median(&self) -> Option<i8> {
        if self.total == 0 {
            return None;
        }
        let lower_rank = u32::from(self.total - 1) / 2;
        let upper_rank = u32::from(self.total) / 2;
        let mut seen = 0u32;
        let mut lower = None;
        for (index, &count) in self.counts.iter().enumerate() {
            let count = u32::from(count);
            if count == 0 {
                continue;
            }
            let end = seen + count;
            if lower.is_none() && end > lower_rank {
                lower = Some(strength(index));
            }
            if end > upper_rank {
                let upper = strength(index);
                let lower = lower?;
                return Some(if lower == upper {
                    lower
                } else {
                    round_weaker(lower, upper)
                });
            }
            seen = end;
        }
        None
    }
}

/// Bin index for a stored strength, or `None` when ingest would reject it.
fn bin(rssi: i8) -> Option<usize> {
    let index = usize::try_from(i16::from(rssi) - i16::from(RSSI_MIN)).ok()?;
    (index < BINS).then_some(index)
}

fn strength(index: usize) -> i8 {
    RSSI_MIN + index as i8
}

/// Nearest whole dBm to the mean of the two central frames. A tie goes to `low`.
fn round_weaker(low: i8, high: i8) -> i8 {
    let sum = i16::from(low) + i16::from(high);
    let rounded = if sum % 2 == 0 { sum / 2 } else { sum / 2 - 1 };
    rounded as i8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn median_of(samples: &[i8]) -> Option<i8> {
        let mut hist = RssiHist::default();
        for &rssi in samples {
            hist.record(rssi);
        }
        hist.median()
    }

    #[test]
    fn empty_and_single_frame() {
        assert_eq!(median_of(&[]), None);
        assert_eq!(median_of(&[-52]), Some(-52));
    }

    #[test]
    fn odd_count_picks_the_middle_frame() {
        assert_eq!(median_of(&[-70, -60, -50]), Some(-60));
        // The peak and the fade sit at the ends, so neither becomes the median.
        assert_eq!(
            median_of(&[-90, -60, -60, -60, -60, -60, -60, -60, -60, -40]),
            Some(-60)
        );
    }

    #[test]
    fn even_count_averages_the_two_central_frames() {
        assert_eq!(median_of(&[-70, -70, -50, -50]), Some(-60));
        assert_eq!(median_of(&[-60, -58]), Some(-59));
        // −60.5 is halfway, and the weaker strength wins the tie.
        assert_eq!(median_of(&[-61, -60]), Some(-61));
    }

    #[test]
    fn ignores_strengths_ingest_would_reject() {
        assert_eq!(median_of(&[0, -101, -60, -60, -59]), Some(-60));
        assert_eq!(median_of(&[0, -128]), None);
    }
}
