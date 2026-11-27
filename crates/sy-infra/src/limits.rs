//! Process resource limits (spec 0004, AC-13).

/// Raise this process's open file soft limit to the smaller of its hard
/// limit and `want`, never lowering it. Agents inherit it. Returns the soft
/// limit now.
#[cfg(unix)]
pub fn raise_nofile_limit(want: u64) -> std::io::Result<u64> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let now = getrlimit(Resource::Nofile);
    let target = want.min(now.maximum.unwrap_or(u64::MAX));
    match now.current {
        None => return Ok(u64::MAX),
        Some(current) if current >= target => return Ok(current),
        Some(_) => {}
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(target),
            maximum: now.maximum,
        },
    )?;
    Ok(target)
}

/// Raise this process's open file limit (nothing to do off Unix).
#[cfg(not(unix))]
pub fn raise_nofile_limit(want: u64) -> std::io::Result<u64> {
    Ok(want)
}

#[cfg(all(test, unix))]
mod tests {
    use rustix::process::{Resource, getrlimit};

    use super::*;

    #[test]
    fn the_soft_limit_should_rise_to_at_most_the_hard_limit_and_never_fall() {
        let hard = getrlimit(Resource::Nofile).maximum.unwrap_or(u64::MAX);
        let now = raise_nofile_limit(4096).unwrap_or(0);
        assert!(now >= 4096.min(hard), "{now}");
        // Asking for less never lowers it.
        assert_eq!(raise_nofile_limit(16).unwrap_or(0), now);
    }
}
