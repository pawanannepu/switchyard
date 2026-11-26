//! Pure launch rules (spec 0003, AC-2 and AC-4): which program may run, where
//! launch files may go, and the environment an agent gets.

use std::collections::BTreeMap;

use sy_adapter_api::LaunchPlan;

/// `TERM` every agent gets.
pub const TERM: &str = "xterm-256color";
/// `COLORTERM` every agent gets.
pub const COLORTERM: &str = "truecolor";
/// The engine socket, for hooks that read env instead of argv.
pub const SIGNAL_SOCKET_VAR: &str = "SWITCHYARD_SIGNAL_SOCKET";
/// The run's signal token, for hooks that read env instead of argv.
pub const SIGNAL_TOKEN_VAR: &str = "SWITCHYARD_SIGNAL_TOKEN";

/// How to find a program the adapter is allowed to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedProgram {
    /// A bare name: resolve it on the login `PATH`.
    Bare(String),
    /// An absolute path: run exactly that, never resolved.
    Absolute(String),
}

/// Check `program` against the adapter's `programs()` list. A bare name
/// matches the same bare name; an absolute path matches only itself,
/// literally. Anything else (a relative path with a `/`, or a program not
/// listed) is refused.
pub fn allowed_program(programs: &[String], program: &str) -> Option<AllowedProgram> {
    if !programs.iter().any(|p| p == program) || program.is_empty() {
        return None;
    }
    if program.starts_with('/') {
        Some(AllowedProgram::Absolute(program.to_owned()))
    } else if program.contains('/') {
        None
    } else {
        Some(AllowedProgram::Bare(program.to_owned()))
    }
}

/// A launch file's path is relative, non empty, has no `..` (or empty)
/// component, and holds no NUL.
pub fn valid_rel_path(rel_path: &str) -> bool {
    !rel_path.is_empty()
        && !rel_path.starts_with('/')
        && !rel_path.contains('\0')
        && rel_path
            .split('/')
            .all(|part| !part.is_empty() && part != "..")
}

/// The agent's environment: the login environment, minus the plan's
/// `env_remove`, plus its `env_set`, plus the engine's own `TERM`,
/// `COLORTERM` and `SWITCHYARD_SIGNAL_*` (which win). Nothing else is added.
pub fn agent_env(
    login: &[(String, String)],
    plan: &LaunchPlan,
    socket: &str,
    token: &str,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = login.iter().cloned().collect();
    for name in &plan.env_remove {
        env.remove(name);
    }
    env.extend(plan.env_set.iter().cloned());
    for (k, v) in [
        ("TERM", TERM),
        ("COLORTERM", COLORTERM),
        (SIGNAL_SOCKET_VAR, socket),
        (SIGNAL_TOKEN_VAR, token),
    ] {
        env.insert(k.to_owned(), v.to_owned());
    }
    env.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs() -> Vec<String> {
        vec!["claude".into(), "/opt/fake/sy-fake-agent".into()]
    }

    #[test]
    fn bare_names_should_resolve_and_absolute_paths_should_match_literally() {
        assert_eq!(
            [
                allowed_program(&programs(), "claude"),
                allowed_program(&programs(), "/opt/fake/sy-fake-agent"),
                allowed_program(&programs(), "/usr/bin/claude"),
                allowed_program(&programs(), "/opt/fake/../fake/sy-fake-agent"),
                allowed_program(&programs(), "sh"),
            ],
            [
                Some(AllowedProgram::Bare("claude".into())),
                Some(AllowedProgram::Absolute("/opt/fake/sy-fake-agent".into())),
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn a_listed_relative_path_should_still_be_refused() {
        assert_eq!(allowed_program(&["bin/agent".into()], "bin/agent"), None);
    }

    #[test]
    fn launch_file_paths_should_stay_inside_the_launch_dir() {
        let ok = ["script.json", "hooks/settings.json"].map(valid_rel_path);
        let bad = ["", "/etc/passwd", "../x", "a/../../x", "a//b", "a\0b"].map(valid_rel_path);
        assert_eq!((ok, bad), ([true; 2], [false; 6]));
    }

    #[test]
    fn agent_env_should_be_the_login_env_plus_only_the_listed_variables() {
        let login = vec![
            ("PATH".into(), "/usr/bin".into()),
            ("HOME".into(), "/Users/me".into()),
            ("TERM".into(), "dumb".into()),
            ("DROP_ME".into(), "x".into()),
        ];
        let plan = LaunchPlan {
            env_set: vec![("AGENT_MODE".into(), "1".into())],
            env_remove: vec!["DROP_ME".into()],
            ..LaunchPlan::default()
        };
        let env = agent_env(&login, &plan, "/tmp/s.sock", "abc");
        let want: Vec<(String, String)> = [
            ("AGENT_MODE", "1"),
            ("COLORTERM", "truecolor"),
            ("HOME", "/Users/me"),
            ("PATH", "/usr/bin"),
            ("SWITCHYARD_SIGNAL_SOCKET", "/tmp/s.sock"),
            ("SWITCHYARD_SIGNAL_TOKEN", "abc"),
            ("TERM", "xterm-256color"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .into();
        assert_eq!(env, want);
    }
}
