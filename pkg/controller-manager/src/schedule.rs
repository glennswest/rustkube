//! Cron schedules, as pure functions.
//!
//! Two things here are easy to get wrong and both are silent when wrong — a
//! job that does not run leaves no trace saying so.
//!
//! **The day-of-month / day-of-week rule.** When *both* fields are restricted,
//! cron runs when **either** matches, not both. `0 0 1 * 1` is "the first of
//! the month, and also every Monday" — not "Mondays that fall on the first",
//! which would be roughly one run every seven months. This is inherited from
//! Vixie cron and is what Kubernetes documents. Implementing it as an AND, as
//! this controller previously did, turns a daily-ish schedule into an almost
//! never.
//!
//! **Missed starts.** A controller that only asks "does the schedule match
//! *right now*" loses every run it was not awake for: a restart, a slow
//! reconcile, a paused node. The schedule has to be evaluated over the window
//! since the last run, which is what [`missed_starts`] does.

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

/// How far back to look for missed starts. A CronJob whose last run was
/// longer ago than this is not caught up minute by minute — see
/// [`missed_starts`] for why that is the safe answer rather than an omission.
const MAX_LOOKBACK_DAYS: i64 = 7;

/// How many missed starts are too many to be a catch-up rather than a flood.
/// Upstream uses the same number and for the same reason: past this, running
/// them is not recovery, it is a stampede.
pub const TOO_MANY_MISSED: usize = 100;

/// Does this schedule fire at this instant (to the minute)?
pub fn matches(schedule: &str, at: &DateTime<Utc>) -> bool {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }

    let minute_set = parse_field(fields[0], 0, 59);
    let hour_set = parse_field(fields[1], 0, 23);
    let dom_set = parse_field(fields[2], 1, 31);
    let month_set = parse_field(fields[3], 1, 12);
    let dow_set = parse_field(fields[4], 0, 7);

    if !minute_set.contains(&at.minute())
        || !hour_set.contains(&at.hour())
        || !month_set.contains(&at.month())
    {
        return false;
    }

    // chrono is 1=Mon..7=Sun; cron is 0=Sun..6=Sat, and accepts 7 for Sunday
    // as well — so a schedule may name Sunday either way and both must match.
    let dow = at.weekday().number_from_monday();
    let cron_dow = if dow == 7 { 0 } else { dow };
    let dow_matches = dow_set.contains(&cron_dow) || (cron_dow == 0 && dow_set.contains(&7));
    let dom_matches = dom_set.contains(&at.day());

    // The rule this module exists for. Restricted means "not `*`": when both
    // days are restricted the schedule is a union, not an intersection.
    let dom_restricted = fields[2] != "*";
    let dow_restricted = fields[4] != "*";
    match (dom_restricted, dow_restricted) {
        (true, true) => dom_matches || dow_matches,
        _ => dom_matches && dow_matches,
    }
}

/// Why a catch-up was refused.
#[derive(Debug, PartialEq)]
pub enum MissedError {
    /// More starts were missed than can sensibly be recovered.
    TooMany(usize),
    /// The last run is further back than the lookback window, so the number of
    /// missed starts cannot be established.
    TooFarBehind,
}

/// Every scheduled instant strictly after `since` and at or before `now`.
///
/// Returned oldest-first. The caller runs **only the most recent** — that is
/// what upstream does, and it is the right call: a CronJob that missed six
/// hourly runs wants one run now, not six at once.
///
/// Refuses rather than guesses when there is too much history: a controller
/// that was down for a week must not wake up and decide it owes the cluster a
/// thousand jobs.
pub fn missed_starts(
    schedule: &str,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<DateTime<Utc>>, MissedError> {
    if now < since {
        return Ok(Vec::new());
    }
    if now - since > Duration::days(MAX_LOOKBACK_DAYS) {
        return Err(MissedError::TooFarBehind);
    }

    let mut out = Vec::new();
    // Start at the minute after `since`, truncated: schedules have minute
    // resolution, so a `since` with seconds on it must not skip its own minute
    // boundary or re-fire the run it represents.
    let mut t = since
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(since)
        + Duration::minutes(1);
    let end = now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now);

    while t <= end {
        if matches(schedule, &t) {
            out.push(t);
            if out.len() > TOO_MANY_MISSED {
                return Err(MissedError::TooMany(out.len()));
            }
        }
        t += Duration::minutes(1);
    }
    Ok(out)
}

/// The start a CronJob should act on now, if any.
///
/// Applies `startingDeadlineSeconds`: a missed start older than the deadline
/// is not run at all. That is the setting's purpose — a job that was supposed
/// to run at 02:00 is often worse than useless at 09:00, and the deadline is
/// how an author says so.
pub fn start_to_run(
    schedule: &str,
    last_schedule: Option<DateTime<Utc>>,
    created: DateTime<Utc>,
    now: DateTime<Utc>,
    starting_deadline_secs: Option<i64>,
) -> Result<Option<DateTime<Utc>>, MissedError> {
    let mut since = last_schedule.unwrap_or(created);
    if let Some(deadline) = starting_deadline_secs {
        let earliest = now - Duration::seconds(deadline);
        if earliest > since {
            since = earliest;
        }
    }
    let starts = missed_starts(schedule, since, now)?;
    Ok(starts.last().copied())
}

/// One cron field to the set of values it matches.
///
/// `*`, `*/n`, `a-b`, `a-b/n`, and comma-separated lists of those.
fn parse_field(field: &str, min: u32, max: u32) -> HashSet<u32> {
    let mut out = HashSet::new();
    for part in field.split(',') {
        let part = part.trim();
        // A step may apply to `*` or to a range: `*/15` and `0-30/10`.
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, s.parse::<u32>().unwrap_or(0)),
            None => (part, 1),
        };
        if step == 0 {
            continue;
        }
        let (lo, hi) = if base == "*" {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            match (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                (Ok(a), Ok(b)) if a <= b => (a, b),
                _ => continue,
            }
        } else {
            match base.trim().parse::<u32>() {
                Ok(v) => (v, v),
                Err(_) => continue,
            }
        };
        let mut v = lo;
        while v <= hi.min(max) {
            if v >= min {
                out.insert(v);
            }
            v += step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn every_minute_and_fixed_times() {
        assert!(matches("* * * * *", &at("2026-08-28T13:37:00Z")));
        assert!(matches("37 13 * * *", &at("2026-08-28T13:37:00Z")));
        assert!(!matches("37 13 * * *", &at("2026-08-28T13:38:00Z")));
        // A schedule with the wrong number of fields never fires, rather than
        // firing constantly.
        assert!(!matches("* * * *", &at("2026-08-28T13:37:00Z")));
        assert!(!matches("", &at("2026-08-28T13:37:00Z")));
    }

    #[test]
    fn steps_ranges_and_lists() {
        assert!(matches("*/15 * * * *", &at("2026-08-28T13:30:00Z")));
        assert!(!matches("*/15 * * * *", &at("2026-08-28T13:31:00Z")));
        assert!(matches("0 9-17 * * *", &at("2026-08-28T17:00:00Z")));
        assert!(!matches("0 9-17 * * *", &at("2026-08-28T18:00:00Z")));
        assert!(matches("0,30 * * * *", &at("2026-08-28T13:30:00Z")));
        // A step over a range.
        assert!(matches("0-30/10 * * * *", &at("2026-08-28T13:20:00Z")));
        assert!(!matches("0-30/10 * * * *", &at("2026-08-28T13:25:00Z")));
    }

    /// The rule this module exists for: both day fields restricted means OR.
    /// 2026-08-28 is a Friday and the 28th.
    #[test]
    fn both_day_fields_restricted_is_a_union_not_an_intersection() {
        // "the 1st, and also every Monday"
        let sched = "0 0 1 * 1";
        // 2026-09-01 is a Tuesday — matches by day-of-month alone.
        assert!(
            matches(sched, &at("2026-09-01T00:00:00Z")),
            "the 1st must fire even though it is not a Monday"
        );
        // 2026-08-31 is a Monday — matches by day-of-week alone.
        assert!(
            matches(sched, &at("2026-08-31T00:00:00Z")),
            "a Monday must fire even though it is not the 1st"
        );
        // A day that is neither does not fire.
        assert!(!matches(sched, &at("2026-09-02T00:00:00Z")));
    }

    /// With only one day field restricted the other is `*` and the semantics
    /// are the ordinary AND — this is the common case and must not regress.
    #[test]
    fn one_day_field_restricted_behaves_normally() {
        assert!(matches("0 0 15 * *", &at("2026-09-15T00:00:00Z")));
        assert!(!matches("0 0 15 * *", &at("2026-09-16T00:00:00Z")));
        // 2026-08-30 is a Sunday; both spellings of Sunday must work.
        assert!(matches("0 0 * * 0", &at("2026-08-30T00:00:00Z")));
        assert!(matches("0 0 * * 7", &at("2026-08-30T00:00:00Z")));
        assert!(!matches("0 0 * * 0", &at("2026-08-31T00:00:00Z")));
    }

    /// A controller that was asleep finds what it missed.
    #[test]
    fn missed_starts_are_found_oldest_first() {
        let since = at("2026-08-28T10:00:00Z");
        let now = at("2026-08-28T13:00:00Z");
        let got = missed_starts("0 * * * *", since, now).unwrap();
        assert_eq!(
            got,
            vec![
                at("2026-08-28T11:00:00Z"),
                at("2026-08-28T12:00:00Z"),
                at("2026-08-28T13:00:00Z")
            ]
        );
    }

    /// The run just taken is not re-run: `since` is exclusive.
    #[test]
    fn the_last_run_is_not_repeated() {
        let since = at("2026-08-28T13:00:00Z");
        let now = at("2026-08-28T13:00:30Z");
        assert!(missed_starts("0 * * * *", since, now).unwrap().is_empty());
    }

    /// Only the most recent missed start is acted on — six missed hourly runs
    /// want one run now, not six at once.
    #[test]
    fn only_the_most_recent_missed_start_is_returned() {
        let last = at("2026-08-28T07:00:00Z");
        let now = at("2026-08-28T13:00:00Z");
        let got = start_to_run("0 * * * *", Some(last), last, now, None).unwrap();
        assert_eq!(got, Some(at("2026-08-28T13:00:00Z")));
    }

    /// startingDeadlineSeconds drops a start that is too stale to be useful.
    #[test]
    fn a_start_older_than_the_deadline_is_not_run() {
        let last = at("2026-08-28T02:00:00Z");
        let now = at("2026-08-28T09:00:30Z");
        // Deadline of 60s: the 09:00 start is within it and is taken.
        assert_eq!(
            start_to_run("0 2 * * *", Some(last), last, now, Some(60)).unwrap(),
            None,
            "the 02:00 start is hours stale and must not run"
        );
        // Without a deadline, the missed 02:00-style schedule is not owed
        // either, because 02:00 already ran; but an hourly one is.
        assert_eq!(
            start_to_run("0 * * * *", Some(last), last, now, Some(3600)).unwrap(),
            Some(at("2026-08-28T09:00:00Z"))
        );
    }

    /// A controller down for a week does not wake up owing a thousand jobs.
    #[test]
    fn too_far_behind_is_refused_rather_than_guessed() {
        let since = at("2026-08-01T00:00:00Z");
        let now = at("2026-08-28T00:00:00Z");
        assert_eq!(
            missed_starts("0 * * * *", since, now),
            Err(MissedError::TooFarBehind)
        );
    }

    /// And within the window, a flood is still refused.
    #[test]
    fn too_many_missed_starts_is_refused() {
        let since = at("2026-08-26T00:00:00Z");
        let now = at("2026-08-28T00:00:00Z");
        // Every minute for two days is far past the cap.
        assert!(matches!(
            missed_starts("* * * * *", since, now),
            Err(MissedError::TooMany(_))
        ));
    }

    /// A brand-new CronJob with no lastScheduleTime uses its creation time,
    /// and does not fire for schedules that predate it.
    #[test]
    fn a_new_cronjob_starts_from_its_creation() {
        let created = Utc.with_ymd_and_hms(2026, 8, 28, 12, 30, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 45, 0).unwrap();
        // Hourly on the hour: nothing between 12:30 and 12:45.
        assert_eq!(start_to_run("0 * * * *", None, created, now, None).unwrap(), None);
        // Every fifteen minutes: 12:45 is due.
        assert_eq!(
            start_to_run("*/15 * * * *", None, created, now, None).unwrap(),
            Some(now)
        );
    }
}
