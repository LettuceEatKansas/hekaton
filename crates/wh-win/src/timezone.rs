//! The local time zone (Python `datetime.fromtimestamp`, used by `fast.py:do_files`).

use wh_core::backend::{BackendError, BackendResult};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTimeEx,
};

/// 100 ns ticks between 1601-01-01 (FILETIME epoch) and 1970-01-01 (Unix epoch), in seconds.
const EPOCH_GAP_SECS: i64 = 11_644_473_600;

/// A Unix time as a FILETIME tick count, `None` before 1601 or past the representable range.
pub(crate) fn unix_to_ticks(unix_secs: i64) -> Option<u64> {
    let secs = unix_secs.checked_add(EPOCH_GAP_SECS)?;
    u64::try_from(secs).ok()?.checked_mul(10_000_000)
}

fn to_filetime(ticks: u64) -> FILETIME {
    FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}

fn ticks_of(ft: &FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// The offset from UTC, in seconds east, that the local zone has at `unix_secs`.
pub(crate) fn local_utc_offset(unix_secs: i64) -> BackendResult<i32> {
    let unsupported = || {
        BackendError::Os(format!(
            "time {unix_secs} is outside the range Windows can convert"
        ))
    };
    let ticks = unix_to_ticks(unix_secs).ok_or_else(unsupported)?;
    let ft = to_filetime(ticks);
    let mut utc = SYSTEMTIME::default();
    // SAFETY: both pointers are live locals.
    unsafe { FileTimeToSystemTime(&ft, &mut utc) }.map_err(|_| unsupported())?;
    let mut local = SYSTEMTIME::default();
    // SAFETY: a null zone means the zone currently configured; both times are live locals.
    unsafe { SystemTimeToTzSpecificLocalTimeEx(None, &utc, &mut local) }
        .map_err(|_| unsupported())?;
    let mut local_ft = FILETIME::default();
    // SAFETY: both pointers are live locals.
    unsafe { SystemTimeToFileTime(&local, &mut local_ft) }.map_err(|_| unsupported())?;
    let diff = i64::try_from(ticks_of(&local_ft)).unwrap_or(0) - i64::try_from(ticks).unwrap_or(0);
    i32::try_from(diff / 10_000_000).map_err(|_| unsupported())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_is_the_filetime_gap() {
        assert_eq!(unix_to_ticks(0), Some(116_444_736_000_000_000));
        assert_eq!(unix_to_ticks(1), Some(116_444_736_010_000_000));
        assert_eq!(
            unix_to_ticks(1_791_417_600),
            Some((1_791_417_600 + 11_644_473_600) * 10_000_000)
        );
    }

    #[test]
    fn times_before_1601_have_no_filetime() {
        assert_eq!(unix_to_ticks(-11_644_473_601), None);
        assert_eq!(unix_to_ticks(-11_644_473_600), Some(0));
        assert_eq!(unix_to_ticks(i64::MIN), None);
        assert_eq!(unix_to_ticks(i64::MAX), None);
    }

    #[test]
    fn filetimes_split_into_two_words() {
        let ft = to_filetime(0x0123_4567_89AB_CDEF);
        assert_eq!(
            (ft.dwHighDateTime, ft.dwLowDateTime),
            (0x0123_4567, 0x89AB_CDEF)
        );
        assert_eq!(ticks_of(&ft), 0x0123_4567_89AB_CDEF);
    }
}
