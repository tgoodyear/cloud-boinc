//! Running averages, following `lib/average.h` and `update_average` in
//! `lib/util.cpp`.

pub const HAV_AVG_THRESH: f64 = 20.0;
pub const HAV_AVG_WEIGHT: f64 = 0.01;
pub const HAV_AVG_LIMIT: f64 = 10.0;

/// Credit half life: one week.
pub const CREDIT_HALF_LIFE: f64 = 86400.0 * 7.0;
/// Cobblestones per floating point operation.
pub const COBBLESTONE_SCALE: f64 = 200.0 / 86400e9;
/// Stand-in speed for hosts whose benchmark failed.
pub const AVG_CPU_FPOPS: f64 = 4.3e9;

/// `AVERAGE_VAR`: mean and variance, plain for the first `n_threshold`
/// samples and exponentially weighted after that.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AverageVar {
    pub n: f64,
    pub avg: f64,
    pub var: f64,
    pub q: f64,
}

impl AverageVar {
    /// Returns true if the sample was truncated to `sample_limit` times the
    /// current average, or ignored because it was negative.
    pub fn update_var(
        &mut self,
        mut sample: f64,
        n_threshold: f64,
        sample_weight: f64,
        sample_limit: f64,
    ) -> bool {
        let mut truncated = false;
        if sample < 0.0 {
            return true;
        }
        if self.n != 0.0 && self.avg != 0.0 && sample > self.avg * sample_limit {
            sample = self.avg * sample_limit;
            truncated = true;
        }
        self.n += 1.0;
        let delta = sample - self.avg;
        if self.n < n_threshold {
            self.avg += delta / self.n;
            self.q += delta * (sample - self.avg);
            self.var = self.q / self.n;
        } else {
            self.avg += sample_weight * delta;
            let vdelta = delta * delta - self.var;
            self.var += sample_weight * vdelta;
        }
        truncated
    }

    pub fn update_hav(&mut self, sample: f64) -> bool {
        self.update_var(sample, HAV_AVG_THRESH, HAV_AVG_WEIGHT, HAV_AVG_LIMIT)
    }
}

/// Exponentially decaying average of work per day (`update_average`).
pub fn update_average(
    now: f64,
    work_start_time: f64,
    work: f64,
    half_life: f64,
    avg: &mut f64,
    avg_time: &mut f64,
) {
    if *avg_time != 0.0 {
        let diff = (now - *avg_time).max(0.0);
        let diff_days = diff / 86400.0;
        let weight = (-diff * std::f64::consts::LN_2 / half_life).exp();
        *avg *= weight;
        if (1.0 - weight) > 1.0e-6 {
            *avg += (1.0 - weight) * (work / diff_days);
        } else {
            *avg += std::f64::consts::LN_2 * work * 86400.0 / half_life;
        }
    } else if work != 0.0 {
        let dd = (now - work_start_time) / 86400.0;
        // A job reported in the second it was sent would divide by zero.
        *avg = if dd > 0.0 { work / dd } else { 0.0 };
    }
    *avg_time = now;
}

/// An average as of `now`: the stored value halved for each `CREDIT_HALF_LIFE`
/// since it was last updated. The stored one is only brought up to date when
/// the account or computer earns credit, so a list shows this instead.
pub fn decayed(avg: f64, avg_time: f64, now: f64) -> f64 {
    if avg_time <= 0.0 || now <= avg_time {
        return avg;
    }
    avg * (-(now - avg_time) * std::f64::consts::LN_2 / CREDIT_HALF_LIFE).exp()
}

/// The sort key of an average, comparable across rows: `ln(avg) + t * ln 2 /
/// half life`. The decayed value at any time `now` is `exp(key - now * ln 2 /
/// half life)`, so ordering by the key is ordering by the average as it stands
/// at any common time. Rows with no average get a key below every real one.
pub const NO_KEY: f64 = -1e9;
pub fn recent_key(avg: f64, avg_time: f64) -> f64 {
    if avg > 0.0 && avg.is_finite() && avg_time > 0.0 {
        avg.ln() + avg_time * std::f64::consts::LN_2 / CREDIT_HALF_LIFE
    } else {
        NO_KEY
    }
}

/// Average in which each value is weighted by the sum of the others, which
/// reduces the weight of large outliers (`low_average` in `sched/credit.cpp`).
pub fn low_average(v: &[f64]) -> f64 {
    match v.len() {
        0 => 0.0,
        1 => v[0],
        n => {
            let sum: f64 = v.iter().sum();
            if sum == 0.0 {
                return 0.0;
            }
            let total: f64 = v.iter().map(|x| x * (sum - x)).sum();
            total / ((n as f64 - 1.0) * sum)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_var_plain_then_weighted() {
        let mut a = AverageVar::default();
        for s in [2.0, 4.0, 6.0] {
            a.update_hav(s);
        }
        assert_eq!(a.n, 3.0);
        assert!((a.avg - 4.0).abs() < 1e-12);
        // 100 is cut to 10 times the average.
        assert!(a.update_hav(1000.0));
        assert!((a.avg - 13.0).abs() < 1e-12);
    }

    #[test]
    fn low_average_discounts_outliers() {
        assert_eq!(low_average(&[5.0]), 5.0);
        assert!((low_average(&[1.0, 1.0]) - 1.0).abs() < 1e-12);
        assert!(low_average(&[1.0, 1.0, 100.0]) < 34.0);
    }

    #[test]
    fn decay_halves_each_week() {
        assert_eq!(decayed(8.0, 0.0, 1e9), 8.0);
        assert_eq!(decayed(8.0, 100.0, 50.0), 8.0);
        assert!((decayed(8.0, 0.0 + 1.0, 1.0 + CREDIT_HALF_LIFE) - 4.0).abs() < 1e-9);
        assert!((decayed(8.0, 1.0, 1.0 + 2.0 * CREDIT_HALF_LIFE) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn the_key_orders_as_the_decayed_averages_do() {
        let now = 2e9;
        let week = CREDIT_HALF_LIFE;
        // Idle for ten weeks at 100 against active at 10 now.
        let idle = (100.0, now - 10.0 * week);
        let active = (10.0, now);
        assert!(decayed(idle.0, idle.1, now) < decayed(active.0, active.1, now));
        assert!(recent_key(idle.0, idle.1) < recent_key(active.0, active.1));
        // And the other way round three weeks in.
        let idle = (100.0, now - 3.0 * week);
        assert!(decayed(idle.0, idle.1, now) > decayed(active.0, active.1, now));
        assert!(recent_key(idle.0, idle.1) > recent_key(active.0, active.1));
        assert_eq!(recent_key(0.0, now), NO_KEY);
        assert_eq!(recent_key(5.0, 0.0), NO_KEY);
    }

    #[test]
    fn update_average_first_sample() {
        let (mut avg, mut t) = (0.0, 0.0);
        update_average(
            86400.0 * 2.0,
            86400.0,
            10.0,
            CREDIT_HALF_LIFE,
            &mut avg,
            &mut t,
        );
        assert!((avg - 10.0).abs() < 1e-9);
        assert_eq!(t, 86400.0 * 2.0);
    }
}
