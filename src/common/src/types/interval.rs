// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::ops::{Add, Neg, Sub};

use byteorder::{BigEndian, NetworkEndian, ReadBytesExt, WriteBytesExt};
use bytes::BytesMut;
use num_traits::{CheckedAdd, CheckedNeg, CheckedSub, Zero};
use postgres_types::{to_sql_checked, FromSql};
use risingwave_pb::data::PbInterval;

use super::ops::IsNegative;
use super::to_binary::ToBinary;
use super::*;
use crate::error::{ErrorCode, Result, RwError};

/// Every interval can be represented by a `Interval`.
/// Note that the difference between Interval and Instant.
/// For example, `5 yrs 1 month 25 days 23:22:57` is a interval (Can be interpreted by Interval Unit
/// with months = 61, days = 25, usecs = (57 + 23 * 3600 + 22 * 60) * 1000000),
/// `1970-01-01 04:05:06` is a Instant or Timestamp
/// One month may contain 28/31 days. One day may contain 23/25 hours.
/// This internals is learned from PG:
/// <https://www.postgresql.org/docs/9.1/datatype-datetime.html#:~:text=field%20is%20negative.-,Internally,-interval%20values%20are>
#[derive(Debug, Clone, Copy, Default)]
pub struct Interval {
    months: i32,
    days: i32,
    usecs: i64,
}

pub const USECS_PER_SEC: i64 = 1_000_000;
pub const USECS_PER_DAY: i64 = 86400 * USECS_PER_SEC;
pub const USECS_PER_MONTH: i64 = 30 * USECS_PER_DAY;

impl Interval {
    /// Smallest interval value.
    pub const MIN: Self = Self {
        months: i32::MIN,
        days: i32::MIN,
        usecs: i64::MIN,
    };

    pub fn from_month_day_usec(months: i32, days: i32, usecs: i64) -> Self {
        Interval {
            months,
            days,
            usecs,
        }
    }

    pub fn get_days(&self) -> i32 {
        self.days
    }

    pub fn get_months(&self) -> i32 {
        self.months
    }

    pub fn get_usecs(&self) -> i64 {
        self.usecs
    }

    pub fn get_usecs_of_day(&self) -> u64 {
        self.usecs.rem_euclid(USECS_PER_DAY) as u64
    }

    pub fn to_protobuf<T: Write>(self, output: &mut T) -> ArrayResult<usize> {
        output.write_i32::<BigEndian>(self.months)?;
        output.write_i32::<BigEndian>(self.days)?;
        output.write_i64::<BigEndian>(self.usecs)?;
        Ok(16)
    }

    /// Multiple [`Interval`] by an integer with overflow check.
    pub fn checked_mul_int<I>(&self, rhs: I) -> Option<Self>
    where
        I: TryInto<i32>,
    {
        let rhs = rhs.try_into().ok()?;
        let months = self.months.checked_mul(rhs)?;
        let days = self.days.checked_mul(rhs)?;
        let usecs = self.usecs.checked_mul(rhs as i64)?;

        Some(Interval {
            months,
            days,
            usecs,
        })
    }

    /// Internal utility used by [`Self::mul_float`] and [`Self::div_float`] to adjust fractional
    /// units. Not intended as general constructor.
    fn from_floats(months: f64, days: f64, usecs: f64) -> Option<Self> {
        // TSROUND in include/datatype/timestamp.h
        // round eagerly at usecs precision because floats are imprecise
        // should round to even #5576
        let months_round_usecs =
            |months: f64| (months * (USECS_PER_MONTH as f64)).round() / (USECS_PER_MONTH as f64);

        let days_round_usecs =
            |days: f64| (days * (USECS_PER_DAY as f64)).round() / (USECS_PER_DAY as f64);

        let trunc_fract = |num: f64| (num.trunc(), num.fract());

        // Handle months
        let (months, months_fract) = trunc_fract(months_round_usecs(months));
        if months.is_nan() || months < i32::MIN.into() || months > i32::MAX.into() {
            return None;
        }
        let months = months as i32;
        let (leftover_days, leftover_days_fract) =
            trunc_fract(days_round_usecs(months_fract * 30.));

        // Handle days
        let (days, days_fract) = trunc_fract(days_round_usecs(days));
        if days.is_nan() || days < i32::MIN.into() || days > i32::MAX.into() {
            return None;
        }
        // Note that PostgreSQL split the integer part and fractional part individually before
        // adding `leftover_days`. This makes a difference for mixed sign interval.
        // For example in `interval '3 mons -3 days' / 2`
        // * `leftover_days` is `15`
        // * `days` from input is `-1.5`
        // If we add first, we get `13.5` which is `13 days 12:00:00`;
        // If we split first, we get `14` and `-0.5`, which ends up as `14 days -12:00:00`.
        let (days_fract_whole, days_fract) =
            trunc_fract(days_round_usecs(days_fract + leftover_days_fract));
        let days = (days as i32)
            .checked_add(leftover_days as i32)?
            .checked_add(days_fract_whole as i32)?;
        let leftover_usecs = days_fract * (USECS_PER_DAY as f64);

        // Handle usecs
        let result_usecs = usecs + leftover_usecs;
        let usecs = result_usecs.round();
        if usecs.is_nan() || usecs < (i64::MIN as f64) || usecs > (i64::MAX as f64) {
            return None;
        }
        let usecs = usecs as i64;

        Some(Self {
            months,
            days,
            usecs,
        })
    }

    /// Divides [`Interval`] by an integer/float with zero check.
    pub fn div_float<I>(&self, rhs: I) -> Option<Self>
    where
        I: TryInto<F64>,
    {
        let rhs = rhs.try_into().ok()?;
        let rhs = rhs.0;

        if rhs == 0.0 {
            return None;
        }

        Self::from_floats(
            self.months as f64 / rhs,
            self.days as f64 / rhs,
            self.usecs as f64 / rhs,
        )
    }

    /// times [`Interval`] with an integer/float.
    pub fn mul_float<I>(&self, rhs: I) -> Option<Self>
    where
        I: TryInto<F64>,
    {
        let rhs = rhs.try_into().ok()?;
        let rhs = rhs.0;

        Self::from_floats(
            self.months as f64 * rhs,
            self.days as f64 * rhs,
            self.usecs as f64 * rhs,
        )
    }

    /// Performs an exact division, returns [`None`] if for any unit, lhs % rhs != 0.
    pub fn exact_div(&self, rhs: &Self) -> Option<i64> {
        let mut res = None;
        let mut check_unit = |l: i64, r: i64| {
            if l == 0 && r == 0 {
                return Some(());
            }
            if l != 0 && r == 0 {
                return None;
            }
            if l % r != 0 {
                return None;
            }
            let new_res = l / r;
            if let Some(old_res) = res {
                if old_res != new_res {
                    return None;
                }
            } else {
                res = Some(new_res);
            }

            Some(())
        };

        check_unit(self.months as i64, rhs.months as i64)?;
        check_unit(self.days as i64, rhs.days as i64)?;
        check_unit(self.usecs, rhs.usecs)?;

        res
    }

    /// Checks if [`Interval`] is positive.
    pub fn is_positive(&self) -> bool {
        self > &Self::from_month_day_usec(0, 0, 0)
    }

    /// Truncate the interval to the precision of milliseconds.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(
    ///     interval.truncate_millis().to_string(),
    ///     "5 years 1 mon 25 days 23:22:57.123"
    /// );
    /// ```
    pub const fn truncate_millis(self) -> Self {
        Interval {
            months: self.months,
            days: self.days,
            usecs: self.usecs / 1000 * 1000,
        }
    }

    /// Truncate the interval to the precision of seconds.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(
    ///     interval.truncate_second().to_string(),
    ///     "5 years 1 mon 25 days 23:22:57"
    /// );
    /// ```
    pub const fn truncate_second(self) -> Self {
        Interval {
            months: self.months,
            days: self.days,
            usecs: self.usecs / USECS_PER_SEC * USECS_PER_SEC,
        }
    }

    /// Truncate the interval to the precision of minutes.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(
    ///     interval.truncate_minute().to_string(),
    ///     "5 years 1 mon 25 days 23:22:00"
    /// );
    /// ```
    pub const fn truncate_minute(self) -> Self {
        Interval {
            months: self.months,
            days: self.days,
            usecs: self.usecs / USECS_PER_SEC / 60 * USECS_PER_SEC * 60,
        }
    }

    /// Truncate the interval to the precision of hours.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(
    ///     interval.truncate_hour().to_string(),
    ///     "5 years 1 mon 25 days 23:00:00"
    /// );
    /// ```
    pub const fn truncate_hour(self) -> Self {
        Interval {
            months: self.months,
            days: self.days,
            usecs: self.usecs / USECS_PER_SEC / 60 / 60 * USECS_PER_SEC * 60 * 60,
        }
    }

    /// Truncate the interval to the precision of days.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_day().to_string(), "5 years 1 mon 25 days");
    /// ```
    pub const fn truncate_day(self) -> Self {
        Interval {
            months: self.months,
            days: self.days,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of months.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_month().to_string(), "5 years 1 mon");
    /// ```
    pub const fn truncate_month(self) -> Self {
        Interval {
            months: self.months,
            days: 0,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of quarters.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_quarter().to_string(), "5 years");
    /// ```
    pub const fn truncate_quarter(self) -> Self {
        Interval {
            months: self.months / 3 * 3,
            days: 0,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of years.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "5 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_year().to_string(), "5 years");
    /// ```
    pub const fn truncate_year(self) -> Self {
        Interval {
            months: self.months / 12 * 12,
            days: 0,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of decades.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "15 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_decade().to_string(), "10 years");
    /// ```
    pub const fn truncate_decade(self) -> Self {
        Interval {
            months: self.months / 12 / 10 * 12 * 10,
            days: 0,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of centuries.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "115 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_century().to_string(), "100 years");
    /// ```
    pub const fn truncate_century(self) -> Self {
        Interval {
            months: self.months / 12 / 100 * 12 * 100,
            days: 0,
            usecs: 0,
        }
    }

    /// Truncate the interval to the precision of millenniums.
    ///
    /// # Example
    /// ```
    /// # use risingwave_common::types::Interval;
    /// let interval: Interval = "1115 years 1 mon 25 days 23:22:57.123".parse().unwrap();
    /// assert_eq!(interval.truncate_millennium().to_string(), "1000 years");
    /// ```
    pub const fn truncate_millennium(self) -> Self {
        Interval {
            months: self.months / 12 / 1000 * 12 * 1000,
            days: 0,
            usecs: 0,
        }
    }
}

/// A separate mod so that `use types::*` or `use interval::*` does not `use IntervalTestExt` by
/// accident.
pub mod test_utils {
    use super::*;

    /// These constructors may panic when value out of bound. Only use in tests with known input.
    pub trait IntervalTestExt {
        fn from_ymd(year: i32, month: i32, days: i32) -> Self;
        fn from_month(months: i32) -> Self;
        fn from_days(days: i32) -> Self;
        fn from_millis(ms: i64) -> Self;
        fn from_minutes(minutes: i64) -> Self;
    }

    impl IntervalTestExt for Interval {
        #[must_use]
        fn from_ymd(year: i32, month: i32, days: i32) -> Self {
            let months = year * 12 + month;
            let days = days;
            let usecs = 0;
            Interval {
                months,
                days,
                usecs,
            }
        }

        #[must_use]
        fn from_month(months: i32) -> Self {
            Interval {
                months,
                ..Default::default()
            }
        }

        #[must_use]
        fn from_days(days: i32) -> Self {
            Self {
                days,
                ..Default::default()
            }
        }

        #[must_use]
        fn from_millis(ms: i64) -> Self {
            Self {
                usecs: ms * 1000,
                ..Default::default()
            }
        }

        #[must_use]
        fn from_minutes(minutes: i64) -> Self {
            Self {
                usecs: USECS_PER_SEC * 60 * minutes,
                ..Default::default()
            }
        }
    }
}

/// Wrapper so that `Debug for IntervalDisplay` would use the concise format of `Display for
/// Interval`.
#[derive(Clone, Copy)]
pub struct IntervalDisplay<'a> {
    pub core: &'a Interval,
}

impl std::fmt::Display for IntervalDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self as &dyn std::fmt::Debug).fmt(f)
    }
}

impl std::fmt::Debug for IntervalDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.core)
    }
}

/// <https://github.com/postgres/postgres/blob/REL_15_2/src/backend/utils/adt/timestamp.c#L2384>
///
/// Do NOT make this `pub` as the assumption of 1 month = 30 days and 1 day = 24 hours does not
/// always hold in other places.
///
/// Given this equality definition in PostgreSQL, different interval values can be considered equal,
/// forming equivalence classes. For example:
/// * '-45 days' == '-1 months -15 days' == '1 months -75 days'
/// * '-2147483646 months -210 days' == '-2147483648 months -150 days' == '-2075900865 months
///   -2147483640 days'
///
/// To hash and memcompare them, we need to pick a representative for each equivalence class, and
/// then map all values from the same equivalence class to the same representative. There are 3
/// choices (may be more):
/// (a) an `i128` of total `usecs`, with `months` and `days` transformed into `usecs`;
/// (b) the justified interval, as defined by PostgreSQL `justify_interval`;
/// (c) the alternate representative interval that maximizes `abs` of smaller units;
///
/// For simplicity we will assume there are only `months` and `days` and ignore `usecs` below.
///
/// The justified interval is more human friendly. It requires all units to have the same sign, and
/// that `0 <= abs(usecs) < USECS_PER_DAY && 0 <= abs(days) < 30`. However, it may overflow. In the
/// 2 examples above, '-1 months -15 days' is the justified interval of the first equivalence class,
/// but there is no justified interval in the second one. It would be '-2147483653 months' but this
/// overflows `i32`. A lot of bits are wasted in a justified interval because `days` is using
/// `i32` for `-29..=29` only.
///
/// The alternate representative interval aims to avoid this overflow. It still requires all units
/// to have the same sign, but maximizes `abs` of smaller unit rather than limit it to `29`. The
/// alternate representative of the 2 examples above are '-45 days' and '-2075900865 months
/// -2147483640 days'. The alternate representative interval always exists.
///
/// For serialize, we could use any of 3, with a workaround of using (i33, i6, i38) rather than
/// (i32, i32, i64) to avoid overflow of the justified interval. We chose the `usecs: i128` option.
///
/// For deserialize, we attempt justified interval first and fallback to alternate. This could give
/// human friendly results in common cases and still guarantee no overflow, as long as the bytes
/// were serialized properly.
///
/// Note the alternate representative interval does not exist in PostgreSQL as they do not
/// deserialize from `IntervalCmpValue`.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
struct IntervalCmpValue(i128);

impl From<Interval> for IntervalCmpValue {
    fn from(value: Interval) -> Self {
        let days = (value.days as i64) + 30i64 * (value.months as i64);
        let usecs = (value.usecs as i128) + (USECS_PER_DAY as i128) * (days as i128);
        Self(usecs)
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        IntervalCmpValue::from(*self).cmp(&(*other).into())
    }
}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Interval {}

impl Hash for Interval {
    fn hash<H: Hasher>(&self, state: &mut H) {
        IntervalCmpValue::from(*self).hash(state);
    }
}

/// Loss of information during the process due to `IntervalCmpValue`. Only intended for
/// memcomparable encoding.
impl Serialize for Interval {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let cmp_value = IntervalCmpValue::from(*self);
        // split i128 as (i64, u64), which is equivalent
        (
            (cmp_value.0 >> 64) as i64,
            cmp_value.0 as u64, // truncate to get the lower part
        )
            .serialize(serializer)
    }
}

impl IntervalCmpValue {
    /// Recover the justified interval from this equivalence class, if it exists.
    fn as_justified(&self) -> Option<Interval> {
        let usecs = (self.0 % (USECS_PER_DAY as i128)) as i64;
        let remaining_days = self.0 / (USECS_PER_DAY as i128);
        let days = (remaining_days % 30) as i32;
        let months = (remaining_days / 30).try_into().ok()?;
        Some(Interval::from_month_day_usec(months, days, usecs))
    }

    /// Recover the alternate representative interval from this equivalence class.
    /// It always exists unless the encoding is invalid. See [`IntervalCmpValue`] for details.
    fn as_alternate(&self) -> Option<Interval> {
        match self.0.cmp(&0) {
            Ordering::Equal => Some(Interval::from_month_day_usec(0, 0, 0)),
            Ordering::Greater => {
                let remaining_usecs = self.0;
                let mut usecs = (remaining_usecs % (USECS_PER_DAY as i128)) as i64;
                let mut remaining_days = remaining_usecs / (USECS_PER_DAY as i128);
                // `usecs` is now smaller than `USECS_PER_DAY` but has 64 bits.
                // How much more days (multiples of `USECS_PER_DAY`) can it hold before overflowing
                // i64::MAX?
                // It should also not exceed `remaining_days` to bring it from positive to negative.
                // When `remaining_days` is larger than `i64::MAX`, just limit by `i64::MAX` (no-op)
                let extra_days = ((i64::MAX - usecs) / USECS_PER_DAY)
                    .min(remaining_days.try_into().unwrap_or(i64::MAX));
                // The lhs of `min` ensures `extra_days * USECS_PER_DAY <= i64::MAX - usecs`
                usecs += extra_days * USECS_PER_DAY;
                // The rhs of `min` ensures `extra_days <= remaining_days`
                remaining_days -= extra_days as i128;

                // Similar above
                let mut days = (remaining_days % 30) as i32;
                let mut remaining_months = remaining_days / 30;
                let extra_months =
                    ((i32::MAX - days) / 30).min(remaining_months.try_into().unwrap_or(i32::MAX));
                days += extra_months * 30;
                remaining_months -= extra_months as i128;

                let months = remaining_months.try_into().ok()?;
                Some(Interval::from_month_day_usec(months, days, usecs))
            }
            Ordering::Less => {
                let remaining_usecs = self.0;
                let mut usecs = (remaining_usecs % (USECS_PER_DAY as i128)) as i64;
                let mut remaining_days = remaining_usecs / (USECS_PER_DAY as i128);
                // The negative case. Borrow negative `extra_days` to make `usecs` as close to
                // `i64::MIN` as possible.
                let extra_days = ((i64::MIN - usecs) / USECS_PER_DAY)
                    .max(remaining_days.try_into().unwrap_or(i64::MIN));
                usecs += extra_days * USECS_PER_DAY;
                remaining_days -= extra_days as i128;

                let mut days = (remaining_days % 30) as i32;
                let mut remaining_months = remaining_days / 30;
                let extra_months =
                    ((i32::MIN - days) / 30).max(remaining_months.try_into().unwrap_or(i32::MIN));
                days += extra_months * 30;
                remaining_months -= extra_months as i128;

                let months = remaining_months.try_into().ok()?;
                Some(Interval::from_month_day_usec(months, days, usecs))
            }
        }
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (hi, lo) = <(i64, u64)>::deserialize(deserializer)?;
        let cmp_value = IntervalCmpValue(((hi as i128) << 64) | (lo as i128));
        let interval = cmp_value
            .as_justified()
            .or_else(|| cmp_value.as_alternate());
        interval.ok_or_else(|| {
            use serde::de::Error as _;
            D::Error::custom("memcomparable deserialize interval overflow")
        })
    }
}

impl crate::hash::HashKeySerDe<'_> for Interval {
    type S = [u8; 16];

    fn serialize(self) -> Self::S {
        let cmp_value = IntervalCmpValue::from(self);
        cmp_value.0.to_ne_bytes()
    }

    fn deserialize<R: std::io::Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 16>(source);
        let cmp_value = IntervalCmpValue(i128::from_ne_bytes(value));
        cmp_value
            .as_justified()
            .or_else(|| cmp_value.as_alternate())
            .expect("HashKey deserialize interval overflow")
    }
}

/// Duplicated logic only used by `HopWindow`. See #8452.
#[expect(clippy::from_over_into)]
impl Into<PbInterval> for Interval {
    fn into(self) -> PbInterval {
        PbInterval {
            months: self.months,
            days: self.days,
            usecs: self.usecs,
        }
    }
}

impl From<&'_ PbInterval> for Interval {
    fn from(p: &'_ PbInterval) -> Self {
        Self {
            months: p.months,
            days: p.days,
            usecs: p.usecs,
        }
    }
}

impl From<Time> for Interval {
    fn from(time: Time) -> Self {
        let mut usecs: i64 = (time.0.num_seconds_from_midnight() as i64) * USECS_PER_SEC;
        usecs += (time.0.nanosecond() / 1000) as i64;
        Self {
            months: 0,
            days: 0,
            usecs,
        }
    }
}

impl Add for Interval {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        let months = self.months + rhs.months;
        let days = self.days + rhs.days;
        let usecs = self.usecs + rhs.usecs;
        Interval {
            months,
            days,
            usecs,
        }
    }
}

impl CheckedNeg for Interval {
    fn checked_neg(&self) -> Option<Self> {
        let months = self.months.checked_neg()?;
        let days = self.days.checked_neg()?;
        let usecs = self.usecs.checked_neg()?;
        Some(Interval {
            months,
            days,
            usecs,
        })
    }
}

impl CheckedAdd for Interval {
    fn checked_add(&self, other: &Self) -> Option<Self> {
        let months = self.months.checked_add(other.months)?;
        let days = self.days.checked_add(other.days)?;
        let usecs = self.usecs.checked_add(other.usecs)?;
        Some(Interval {
            months,
            days,
            usecs,
        })
    }
}

impl Sub for Interval {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        let months = self.months - rhs.months;
        let days = self.days - rhs.days;
        let usecs = self.usecs - rhs.usecs;
        Interval {
            months,
            days,
            usecs,
        }
    }
}

impl CheckedSub for Interval {
    fn checked_sub(&self, other: &Self) -> Option<Self> {
        let months = self.months.checked_sub(other.months)?;
        let days = self.days.checked_sub(other.days)?;
        let usecs = self.usecs.checked_sub(other.usecs)?;
        Some(Interval {
            months,
            days,
            usecs,
        })
    }
}

impl Zero for Interval {
    fn zero() -> Self {
        Self::from_month_day_usec(0, 0, 0)
    }

    fn is_zero(&self) -> bool {
        self.months == 0 && self.days == 0 && self.usecs == 0
    }
}

impl IsNegative for Interval {
    fn is_negative(&self) -> bool {
        self < &Self::from_month_day_usec(0, 0, 0)
    }
}

impl Neg for Interval {
    type Output = Self;

    fn neg(self) -> Self {
        Self {
            months: -self.months,
            days: -self.days,
            usecs: -self.usecs,
        }
    }
}

impl ToText for crate::types::Interval {
    fn write<W: std::fmt::Write>(&self, f: &mut W) -> std::fmt::Result {
        write!(f, "{self}")
    }

    fn write_with_type<W: std::fmt::Write>(&self, ty: &DataType, f: &mut W) -> std::fmt::Result {
        match ty {
            DataType::Interval => self.write(f),
            _ => unreachable!(),
        }
    }
}

impl Interval {
    pub fn as_iso_8601(&self) -> String {
        // ISO pattern - PnYnMnDTnHnMnS
        let years = self.months / 12;
        let months = self.months % 12;
        let days = self.days;
        let secs_fract = (self.usecs % USECS_PER_SEC).abs();
        let total_secs = (self.usecs / USECS_PER_SEC).abs();
        let hours = total_secs / 3600;
        let minutes = (total_secs / 60) % 60;
        let seconds = total_secs % 60;
        let mut buf = [0u8; 7];
        let fract_str = if secs_fract != 0 {
            write!(buf.as_mut_slice(), ".{:06}", secs_fract).unwrap();
            std::str::from_utf8(&buf).unwrap().trim_end_matches('0')
        } else {
            ""
        };
        format!("P{years}Y{months}M{days}DT{hours}H{minutes}M{seconds}{fract_str}S")
    }
}

impl Display for Interval {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let years = self.months / 12;
        let months = self.months % 12;
        let days = self.days;
        let mut space = false;
        let mut following_neg = false;
        let mut write_i32 = |arg: i32, unit: &str| -> std::fmt::Result {
            if arg == 0 {
                return Ok(());
            }
            if space {
                write!(f, " ")?;
            }
            if following_neg && arg > 0 {
                write!(f, "+")?;
            }
            write!(f, "{arg} {unit}")?;
            if arg != 1 {
                write!(f, "s")?;
            }
            space = true;
            following_neg = arg < 0;
            Ok(())
        };
        write_i32(years, "year")?;
        write_i32(months, "mon")?;
        write_i32(days, "day")?;
        if self.usecs != 0 || self.months == 0 && self.days == 0 {
            // `abs` on `self.usecs == i64::MIN` would overflow, so we divide first then abs
            let secs_fract = (self.usecs % USECS_PER_SEC).abs();
            let total_secs = (self.usecs / USECS_PER_SEC).abs();
            let hours = total_secs / 3600;
            let minutes = (total_secs / 60) % 60;
            let seconds = total_secs % 60;

            if space {
                write!(f, " ")?;
            }
            if following_neg && self.usecs > 0 {
                write!(f, "+")?;
            } else if self.usecs < 0 {
                write!(f, "-")?;
            }
            write!(f, "{hours:0>2}:{minutes:0>2}:{seconds:0>2}")?;
            if secs_fract != 0 {
                let mut buf = [0u8; 7];
                write!(buf.as_mut_slice(), ".{:06}", secs_fract).unwrap();
                write!(
                    f,
                    "{}",
                    std::str::from_utf8(&buf).unwrap().trim_end_matches('0')
                )?;
            }
        }
        Ok(())
    }
}

impl ToSql for Interval {
    to_sql_checked!();

    fn to_sql(
        &self,
        _: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn Error + 'static + Send + Sync>> {
        // refer: https://github.com/postgres/postgres/blob/517bf2d91/src/backend/utils/adt/timestamp.c#L1008
        out.put_i64(self.usecs);
        out.put_i32(self.days);
        out.put_i32(self.months);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::INTERVAL)
    }
}

impl<'a> FromSql<'a> for Interval {
    fn from_sql(
        _: &Type,
        mut raw: &'a [u8],
    ) -> std::result::Result<Interval, Box<dyn Error + Sync + Send>> {
        let usecs = raw.read_i64::<NetworkEndian>()?;
        let days = raw.read_i32::<NetworkEndian>()?;
        let months = raw.read_i32::<NetworkEndian>()?;
        Ok(Interval::from_month_day_usec(months, days, usecs))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::INTERVAL)
    }
}

impl ToBinary for Interval {
    fn to_binary_with_type(&self, ty: &DataType) -> Result<Option<Bytes>> {
        match ty {
            DataType::Interval => {
                let mut output = BytesMut::new();
                self.to_sql(&Type::ANY, &mut output).unwrap();
                Ok(Some(output.freeze()))
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DateTimeField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl FromStr for DateTimeField {
    type Err = RwError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "years" | "year" | "yrs" | "yr" | "y" => Ok(Self::Year),
            "days" | "day" | "d" => Ok(Self::Day),
            "hours" | "hour" | "hrs" | "hr" | "h" => Ok(Self::Hour),
            "minutes" | "minute" | "mins" | "min" | "m" => Ok(Self::Minute),
            "months" | "month" | "mons" | "mon" => Ok(Self::Month),
            "seconds" | "second" | "secs" | "sec" | "s" => Ok(Self::Second),
            _ => Err(ErrorCode::InvalidInputSyntax(format!("unknown unit {}", s)).into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TimeStrToken {
    Second(F64),
    Num(i64),
    TimeUnit(DateTimeField),
}

fn parse_interval(s: &str) -> Result<Vec<TimeStrToken>> {
    let s = s.trim();
    let mut tokens = Vec::new();
    let mut num_buf = "".to_string();
    let mut char_buf = "".to_string();
    let mut hour_min_sec = Vec::new();
    for (i, c) in s.chars().enumerate() {
        match c {
            '-' | '+' => {
                num_buf.push(c);
            }
            '.' => {
                num_buf.push(c);
            }
            c if c.is_ascii_digit() => {
                convert_unit(&mut char_buf, &mut tokens)?;
                num_buf.push(c);
            }
            c if c.is_ascii_alphabetic() => {
                convert_digit(&mut num_buf, &mut tokens)?;
                char_buf.push(c);
            }
            chr if chr.is_ascii_whitespace() => {
                convert_unit(&mut char_buf, &mut tokens)?;
                convert_digit(&mut num_buf, &mut tokens)?;
            }
            ':' => {
                // there must be a digit before the ':'
                if num_buf.is_empty() {
                    return Err(ErrorCode::InvalidInputSyntax(format!(
                        "invalid interval format: {}",
                        s
                    ))
                    .into());
                }
                hour_min_sec.push(num_buf.clone());
                num_buf.clear();
            }
            _ => {
                return Err(ErrorCode::InvalidInputSyntax(format!(
                    "Invalid character at offset {} in {}: {:?}. Only support digit or alphabetic now",
                    i,s, c
                ))
                .into());
            }
        };
    }
    if !hour_min_sec.is_empty() {
        if !num_buf.is_empty() {
            hour_min_sec.push(num_buf.clone());
            num_buf.clear();
        }
    } else {
        convert_digit(&mut num_buf, &mut tokens)?;
    }
    convert_unit(&mut char_buf, &mut tokens)?;
    convert_hms(&mut hour_min_sec, &mut tokens).ok_or_else(|| {
        ErrorCode::InvalidInputSyntax(format!("Invalid interval: {:?}", hour_min_sec))
    })?;

    Ok(tokens)
}

fn convert_digit(c: &mut String, t: &mut Vec<TimeStrToken>) -> Result<()> {
    if !c.is_empty() {
        match c.parse::<i64>() {
            Ok(num) => {
                t.push(TimeStrToken::Num(num));
            }
            Err(_) => {
                return Err(
                    ErrorCode::InvalidInputSyntax(format!("Invalid interval: {}", c)).into(),
                );
            }
        }
        c.clear();
    }
    Ok(())
}

fn convert_unit(c: &mut String, t: &mut Vec<TimeStrToken>) -> Result<()> {
    if !c.is_empty() {
        t.push(TimeStrToken::TimeUnit(c.parse()?));
        c.clear();
    }
    Ok(())
}

/// convert `hour_min_sec` format
/// e.g.
/// c = ["1", "2", "3"], c will be convert to:
/// [`TimeStrToken::Num(1)`, `TimeStrToken::TimeUnit(DateTimeField::Hour)`,
///  `TimeStrToken::Num(2)`, `TimeStrToken::TimeUnit(DateTimeField::Minute)`,
///  `TimeStrToken::Second("3")`, `TimeStrToken::TimeUnit(DateTimeField::Second)`]
fn convert_hms(c: &mut Vec<String>, t: &mut Vec<TimeStrToken>) -> Option<()> {
    if c.len() > 3 {
        return None;
    }
    const HOUR: usize = 0;
    const MINUTE: usize = 1;
    const SECOND: usize = 2;
    let mut is_neg = false;
    for (i, s) in c.iter().enumerate() {
        match i {
            HOUR => {
                let v = s.parse().ok()?;
                is_neg = v < 0;
                t.push(TimeStrToken::Num(v));
                t.push(TimeStrToken::TimeUnit(DateTimeField::Hour))
            }
            MINUTE => {
                let mut v: i64 = s.parse().ok()?;
                if !(0..60).contains(&v) {
                    return None;
                }
                if is_neg {
                    v = v.checked_neg()?;
                }
                t.push(TimeStrToken::Num(v));
                t.push(TimeStrToken::TimeUnit(DateTimeField::Minute))
            }
            SECOND => {
                let mut v: F64 = s.parse().ok()?;
                // PostgreSQL allows '60.x' for seconds.
                if !(0f64 <= *v && *v < 61f64) {
                    return None;
                }
                if is_neg {
                    v = v.checked_neg()?;
                }
                t.push(TimeStrToken::Second(v));
                t.push(TimeStrToken::TimeUnit(DateTimeField::Second))
            }
            _ => unreachable!(),
        }
    }
    Some(())
}

impl Interval {
    fn parse_sql_standard(s: &str, leading_field: DateTimeField) -> Result<Self> {
        use DateTimeField::*;
        let tokens = parse_interval(s)?;
        // Todo: support more syntax
        if tokens.len() > 1 {
            return Err(ErrorCode::InvalidInputSyntax(format!(
                "(standard sql format) Can't support syntax of interval {}.",
                &s
            ))
            .into());
        }
        let num = match tokens.get(0) {
            Some(TimeStrToken::Num(num)) => *num,
            _ => {
                return Err(ErrorCode::InvalidInputSyntax(format!(
                    "(standard sql format)Invalid interval {}.",
                    &s
                ))
                .into());
            }
        };

        (|| match leading_field {
            Year => {
                let months = num.checked_mul(12)?.try_into().ok()?;
                Some(Interval::from_month_day_usec(months, 0, 0))
            }
            Month => Some(Interval::from_month_day_usec(num.try_into().ok()?, 0, 0)),
            Day => Some(Interval::from_month_day_usec(0, num.try_into().ok()?, 0)),
            Hour => {
                let usecs = num.checked_mul(3600 * USECS_PER_SEC)?;
                Some(Interval::from_month_day_usec(0, 0, usecs))
            }
            Minute => {
                let usecs = num.checked_mul(60 * USECS_PER_SEC)?;
                Some(Interval::from_month_day_usec(0, 0, usecs))
            }
            Second => {
                let usecs = num.checked_mul(USECS_PER_SEC)?;
                Some(Interval::from_month_day_usec(0, 0, usecs))
            }
        })()
        .ok_or_else(|| ErrorCode::InvalidInputSyntax(format!("Invalid interval {}.", s)).into())
    }

    fn parse_postgres(s: &str) -> Result<Self> {
        use DateTimeField::*;
        let mut tokens = parse_interval(s)?;
        if tokens.len()%2!=0 && let Some(TimeStrToken::Num(_)) = tokens.last() {
            tokens.push(TimeStrToken::TimeUnit(DateTimeField::Second));
        }
        if tokens.len() % 2 != 0 {
            return Err(ErrorCode::InvalidInputSyntax(format!("Invalid interval {}.", &s)).into());
        }
        let mut token_iter = tokens.into_iter();
        let mut result = Interval::from_month_day_usec(0, 0, 0);
        while let Some(num) = token_iter.next() && let Some(interval_unit) = token_iter.next() {
            match (num, interval_unit) {
                (TimeStrToken::Num(num), TimeStrToken::TimeUnit(interval_unit)) => {
                    result = (|| match interval_unit {
                        Year => {
                            let months = num.checked_mul(12)?.try_into().ok()?;
                            Some(Interval::from_month_day_usec(months, 0, 0))
                        }
                        Month => Some(Interval::from_month_day_usec(num.try_into().ok()?, 0, 0)),
                        Day => Some(Interval::from_month_day_usec(0, num.try_into().ok()?, 0)),
                        Hour => {
                            let usecs = num.checked_mul(3600 * USECS_PER_SEC)?;
                            Some(Interval::from_month_day_usec(0, 0, usecs))
                        }
                        Minute => {
                            let usecs = num.checked_mul(60 * USECS_PER_SEC)?;
                            Some(Interval::from_month_day_usec(0, 0, usecs))
                        }
                        Second => {
                            let usecs = num.checked_mul(USECS_PER_SEC)?;
                            Some(Interval::from_month_day_usec(0, 0, usecs))
                        }
                    })()
                    .and_then(|rhs| result.checked_add(&rhs))
                    .ok_or_else(|| ErrorCode::InvalidInputSyntax(format!("Invalid interval {}.", s)))?;
                }
                (TimeStrToken::Second(second), TimeStrToken::TimeUnit(interval_unit)) => {
                    result = match interval_unit {
                        Second => {
                            // If unsatisfied precision is passed as input, we should not return None (Error).
                            let usecs = (second.into_inner() * (USECS_PER_SEC as f64)).round() as i64;
                            Some(Interval::from_month_day_usec(0, 0, usecs))
                        }
                        _ => None,
                    }
                    .and_then(|rhs| result.checked_add(&rhs))
                    .ok_or_else(|| ErrorCode::InvalidInputSyntax(format!("Invalid interval {}.", s)))?;
                }
                _ => {
                    return Err(ErrorCode::InvalidInputSyntax(format!("Invalid interval {}.", &s)).into());
                }
            }
        }
        Ok(result)
    }

    pub fn parse_with_fields(s: &str, leading_field: Option<DateTimeField>) -> Result<Self> {
        if let Some(leading_field) = leading_field {
            Self::parse_sql_standard(s, leading_field)
        } else {
            Self::parse_postgres(s)
        }
    }
}

impl FromStr for Interval {
    type Err = RwError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse_with_fields(s, None)
    }
}

#[cfg(test)]
mod tests {
    use interval::test_utils::IntervalTestExt;

    use super::*;
    use crate::types::ordered_float::OrderedFloat;

    #[test]
    fn test_parse() {
        let interval = "04:00:00".parse::<Interval>().unwrap();
        assert_eq!(interval, Interval::from_millis(4 * 3600 * 1000));

        let interval = "1 year 2 months 3 days 00:00:01"
            .parse::<Interval>()
            .unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_millis(1000)
        );

        let interval = "1 year 2 months 3 days 00:00:00.001"
            .parse::<Interval>()
            .unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_millis(1)
        );

        let interval = "1 year 2 months 3 days 00:59:59.005"
            .parse::<Interval>()
            .unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14)
                + Interval::from_days(3)
                + Interval::from_minutes(59)
                + Interval::from_millis(59000)
                + Interval::from_millis(5)
        );

        let interval = "1 year 2 months 3 days 01".parse::<Interval>().unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_millis(1000)
        );

        let interval = "1 year 2 months 3 days 1:".parse::<Interval>().unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_minutes(60)
        );

        let interval = "1 year 2 months 3 days 1:2".parse::<Interval>().unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_minutes(62)
        );

        let interval = "1 year 2 months 3 days 1:2:".parse::<Interval>().unwrap();
        assert_eq!(
            interval,
            Interval::from_month(14) + Interval::from_days(3) + Interval::from_minutes(62)
        );
    }

    #[test]
    fn test_to_string() {
        assert_eq!(
            Interval::from_month_day_usec(-14, 3, (11 * 3600 + 45 * 60 + 14) * USECS_PER_SEC + 233)
                .to_string(),
            "-1 years -2 mons +3 days 11:45:14.000233"
        );
        assert_eq!(
            Interval::from_month_day_usec(-14, 3, 0).to_string(),
            "-1 years -2 mons +3 days"
        );
        assert_eq!(Interval::default().to_string(), "00:00:00");
        assert_eq!(
            Interval::from_month_day_usec(
                -14,
                3,
                -((11 * 3600 + 45 * 60 + 14) * USECS_PER_SEC + 233)
            )
            .to_string(),
            "-1 years -2 mons +3 days -11:45:14.000233"
        );
    }

    #[test]
    fn test_exact_div() {
        let cases = [
            ((14, 6, 6), (14, 6, 6), Some(1)),
            ((0, 0, 0), (0, 0, 0), None),
            ((0, 0, 0), (1, 0, 0), Some(0)),
            ((1, 1, 1), (0, 0, 0), None),
            ((1, 1, 1), (1, 0, 0), None),
            ((10, 0, 0), (1, 0, 0), Some(10)),
            ((10, 0, 0), (4, 0, 0), None),
            ((0, 24, 0), (4, 0, 0), None),
            ((6, 8, 9), (3, 1, 3), None),
            ((6, 8, 12), (3, 4, 6), Some(2)),
        ];

        for (lhs, rhs, expected) in cases {
            let lhs = Interval::from_month_day_usec(lhs.0, lhs.1, lhs.2 as i64);
            let rhs = Interval::from_month_day_usec(rhs.0, rhs.1, rhs.2 as i64);
            let result = std::panic::catch_unwind(|| {
                let actual = lhs.exact_div(&rhs);
                assert_eq!(actual, expected);
            });
            if result.is_err() {
                println!("Failed on {}.exact_div({})", lhs, rhs);
                break;
            }
        }
    }

    #[test]
    fn test_div_float() {
        let cases_int = [
            ((10, 8, 6), 2, Some((5, 4, 3))),
            ((1, 2, 33), 3, Some((0, 10, 57600000011i64))),
            ((1, 0, 11), 10, Some((0, 3, 1))),
            ((5, 6, 7), 0, None),
        ];

        let cases_float = [
            ((10, 8, 6), 2.0f32, Some((5, 4, 3))),
            ((1, 2, 33), 3.0f32, Some((0, 10, 57600000011i64))),
            ((10, 15, 100), 2.5f32, Some((4, 6, 40))),
            ((5, 6, 7), 0.0f32, None),
        ];

        for (lhs, rhs, expected) in cases_int {
            let lhs = Interval::from_month_day_usec(lhs.0, lhs.1, lhs.2 as i64);
            let expected = expected.map(|x| Interval::from_month_day_usec(x.0, x.1, x.2));

            let actual = lhs.div_float(rhs as i16);
            assert_eq!(actual, expected);

            let actual = lhs.div_float(rhs);
            assert_eq!(actual, expected);

            let actual = lhs.div_float(rhs as i64);
            assert_eq!(actual, expected);
        }

        for (lhs, rhs, expected) in cases_float {
            let lhs = Interval::from_month_day_usec(lhs.0, lhs.1, lhs.2 as i64);
            let expected = expected.map(|x| Interval::from_month_day_usec(x.0, x.1, x.2));

            let actual = lhs.div_float(OrderedFloat::<f32>(rhs));
            assert_eq!(actual, expected);

            let actual = lhs.div_float(OrderedFloat::<f64>(rhs as f64));
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_serialize_deserialize() {
        let mut serializer = memcomparable::Serializer::new(vec![]);
        let a = Interval::from_month_day_usec(123, 456, 789);
        a.serialize(&mut serializer).unwrap();
        let buf = serializer.into_inner();
        let mut deserializer = memcomparable::Deserializer::new(&buf[..]);
        assert_eq!(Interval::deserialize(&mut deserializer).unwrap(), a);
    }

    #[test]
    fn test_memcomparable() {
        let cases = [
            ((1, 2, 3), (4, 5, 6), Ordering::Less),
            ((0, 31, 0), (1, 0, 0), Ordering::Greater),
            ((1, 0, 0), (0, 0, USECS_PER_MONTH + 1), Ordering::Less),
            ((0, 1, 0), (0, 0, USECS_PER_DAY + 1), Ordering::Less),
            (
                (2, 3, 4),
                (1, 2, 4 + USECS_PER_DAY + USECS_PER_MONTH),
                Ordering::Equal,
            ),
        ];

        for ((lhs_months, lhs_days, lhs_usecs), (rhs_months, rhs_days, rhs_usecs), order) in cases {
            let lhs = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                Interval::from_month_day_usec(lhs_months, lhs_days, lhs_usecs)
                    .serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            let rhs = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                Interval::from_month_day_usec(rhs_months, rhs_days, rhs_usecs)
                    .serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            assert_eq!(lhs.cmp(&rhs), order)
        }
    }

    #[test]
    fn test_deserialize_justify() {
        let cases = [
            (
                (0, 0, USECS_PER_MONTH * 2 + USECS_PER_DAY * 3 + 4),
                Some((2, 3, 4i64, "2 mons 3 days 00:00:00.000004")),
            ),
            ((i32::MIN, i32::MIN, i64::MIN), None),
            ((i32::MAX, i32::MAX, i64::MAX), None),
            (
                (0, i32::MIN, i64::MIN),
                Some((
                    -75141187,
                    -29,
                    -14454775808,
                    "-6261765 years -7 mons -29 days -04:00:54.775808",
                )),
            ),
            (
                (i32::MIN, -60, i64::MAX),
                Some((
                    -2143925250,
                    -8,
                    -71945224193,
                    "-178660437 years -6 mons -8 days -19:59:05.224193",
                )),
            ),
        ];
        for ((lhs_months, lhs_days, lhs_usecs), rhs) in cases {
            let input = Interval::from_month_day_usec(lhs_months, lhs_days, lhs_usecs);
            let actual_deserialize = IntervalCmpValue::from(input).as_justified();

            match rhs {
                None => {
                    assert_eq!(actual_deserialize, None);
                }
                Some((rhs_months, rhs_days, rhs_usecs, rhs_str)) => {
                    // We should test individual fields rather than using custom `Eq`
                    assert_eq!(actual_deserialize.unwrap().get_months(), rhs_months);
                    assert_eq!(actual_deserialize.unwrap().get_days(), rhs_days);
                    assert_eq!(actual_deserialize.unwrap().get_usecs(), rhs_usecs);
                    assert_eq!(actual_deserialize.unwrap().to_string(), rhs_str);
                }
            }
        }

        // A false positive overflow that is buggy in PostgreSQL 15.2.
        let input = Interval::from_month_day_usec(i32::MIN, -30, 1);
        let actual_deserialize = IntervalCmpValue::from(input).as_justified();
        // It has a justified interval within range, and can be obtained by our deserialization.
        assert_eq!(actual_deserialize.unwrap().get_months(), i32::MIN);
        assert_eq!(actual_deserialize.unwrap().get_days(), -29);
        assert_eq!(actual_deserialize.unwrap().get_usecs(), -USECS_PER_DAY + 1);
    }

    #[test]
    fn test_deserialize_alternate() {
        let cases = [
            (0, 0, USECS_PER_MONTH * 2 + USECS_PER_DAY * 3 + 4),
            (i32::MIN, i32::MIN, i64::MIN),
            (i32::MAX, i32::MAX, i64::MAX),
            (0, i32::MIN, i64::MIN),
            (i32::MIN, -60, i64::MAX),
        ];
        for (months, days, usecs) in cases {
            let input = Interval::from_month_day_usec(months, days, usecs);

            let mut serializer = memcomparable::Serializer::new(vec![]);
            input.serialize(&mut serializer).unwrap();
            let buf = serializer.into_inner();
            let mut deserializer = memcomparable::Deserializer::new(&buf[..]);
            let actual = Interval::deserialize(&mut deserializer).unwrap();

            // The Interval we get back can be a different one, but they should be equal.
            assert_eq!(actual, input);
        }

        // Decoding invalid value
        let mut serializer = memcomparable::Serializer::new(vec![]);
        (i64::MAX, u64::MAX).serialize(&mut serializer).unwrap();
        let buf = serializer.into_inner();
        let mut deserializer = memcomparable::Deserializer::new(&buf[..]);
        assert!(Interval::deserialize(&mut deserializer).is_err());

        let buf = i128::MIN.to_ne_bytes();
        std::panic::catch_unwind(|| {
            <Interval as crate::hash::HashKeySerDe>::deserialize(&mut &buf[..])
        })
        .unwrap_err();
    }
}
