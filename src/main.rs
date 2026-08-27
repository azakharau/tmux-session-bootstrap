use std::{
    env,
    ffi::{OsStr, OsString},
    fmt, io,
    process::{Command, ExitCode, ExitStatus, Output, Stdio},
};

const WINDOW_NAMES: [&str; 3] = ["agent", "vim", "terminal"];
const USAGE: &str = "Usage: ts <session-name>";
const HELP_TEXT: &str = concat!(
    "Usage: ts <session-name>\n\n",
    "Options:\n",
    "  -h, --help  Show this help and exit"
);

fn main() -> ExitCode {
    match run(env::args_os()) {
        Ok(Some(message)) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    }
}

fn run<I, S>(args: I) -> Result<Option<String>, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    match parse_command(args)? {
        CliCommand::Help => Ok(Some(HELP_TEXT.to_owned())),
        CliCommand::CreateSession(session_name) => {
            ensure_session_absent(&session_name)?;
            create_session(&session_name)?;
            rollback_session_creation(
                &session_name,
                enter_session(&session_name),
                cleanup_session,
            )?;

            Ok(None)
        }
    }
}

fn parse_command<I, S>(args: I) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program_name = args.next();

    let raw_argument = args.next().ok_or(CliError::MissingSessionName)?;

    if args.next().is_some() {
        return Err(CliError::TooManyArguments);
    }

    let argument = raw_argument.to_string_lossy();
    let trimmed_argument = argument.trim();

    if matches!(trimmed_argument, "-h" | "--help") {
        return Ok(CliCommand::Help);
    }

    let session_name = trimmed_argument.to_owned();

    if session_name.is_empty() {
        return Err(CliError::EmptySessionName);
    }

    Ok(CliCommand::CreateSession(session_name))
}

fn ensure_session_absent(session_name: &str) -> Result<(), CliError> {
    let output = run_tmux_output(
        ["has-session", "-t", session_name],
        "check whether the tmux session already exists",
    )?;

    if output.status.success() {
        return Err(CliError::SessionAlreadyExists(session_name.to_owned()));
    }

    Ok(())
}

fn create_session(session_name: &str) -> Result<(), CliError> {
    let session_target = format!("{session_name}:");

    let first_window_id = run_tmux_identifier(
        [
            "new-session",
            "-dP",
            "-F",
            "#{window_id}",
            "-s",
            session_name,
        ],
        "create the detached tmux session",
    )?;

    rollback_session_creation(
        session_name,
        (|| {
            configure_window(&first_window_id, WINDOW_NAMES[0])?;

            for window_name in WINDOW_NAMES.iter().skip(1) {
                let window_id = run_tmux_identifier(
                    [
                        "new-window",
                        "-dP",
                        "-F",
                        "#{window_id}",
                        "-t",
                        &session_target,
                    ],
                    "create tmux windows",
                )?;

                configure_window(&window_id, window_name)?;
            }

            run_tmux_checked(
                ["select-window", "-t", &first_window_id],
                "select the starting tmux window",
            )
        })(),
        cleanup_session,
    )
}

fn enter_session(session_name: &str) -> Result<(), CliError> {
    let tmux_env = env::var_os("TMUX");
    SessionEntryMode::detect(tmux_env.as_deref()).enter(session_name)
}

fn configure_window(window_target: &str, window_name: &str) -> Result<(), CliError> {
    run_tmux_checked(
        ["rename-window", "-t", window_target, window_name],
        "rename a tmux window",
    )?;

    for option in ["automatic-rename", "allow-rename"] {
        run_tmux_checked(
            ["set-window-option", "-t", window_target, option, "off"],
            "disable tmux window auto-renaming",
        )?;
    }

    Ok(())
}

fn cleanup_session(session_name: &str) -> Result<(), CliError> {
    run_tmux_checked(
        ["kill-session", "-t", session_name],
        "clean up the partially created tmux session",
    )
}

fn rollback_session_creation<T, Cleanup>(
    session_name: &str,
    result: Result<T, CliError>,
    cleanup: Cleanup,
) -> Result<T, CliError>
where
    Cleanup: FnOnce(&str) -> Result<(), CliError>,
{
    match result {
        Ok(value) => Ok(value),
        Err(setup_error) => match cleanup(session_name) {
            Ok(()) => Err(setup_error),
            Err(cleanup_error) => Err(session_creation_rollback_error(setup_error, cleanup_error)),
        },
    }
}

fn session_creation_rollback_error(setup_error: CliError, cleanup_error: CliError) -> CliError {
    CliError::TmuxCommandFailed {
        action: "clean up the partially created tmux session",
        details: format!("session setup failed: {setup_error}; cleanup failed: {cleanup_error}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEntryMode {
    AttachSession,
    SwitchClient,
}

impl SessionEntryMode {
    fn detect(tmux_env: Option<&OsStr>) -> Self {
        Self::from_tmux_context(tmux_env, has_active_tmux_client_context())
    }

    fn from_tmux_context(tmux_env: Option<&OsStr>, has_active_client_context: bool) -> Self {
        if tmux_env.is_some_and(|value| !value.is_empty()) && has_active_client_context {
            Self::SwitchClient
        } else {
            Self::AttachSession
        }
    }

    fn tmux_env_policy(self) -> TmuxEnvPolicy {
        match self {
            Self::AttachSession => TmuxEnvPolicy::ClearTmux,
            Self::SwitchClient => TmuxEnvPolicy::Preserve,
        }
    }

    fn stdio_policy(self) -> TmuxStdioPolicy {
        TmuxStdioPolicy::Inherit
    }

    fn enter(self, session_name: &str) -> Result<(), CliError> {
        run_tmux_checked_with_options(
            match self {
                Self::AttachSession => ["attach-session", "-t", session_name],
                Self::SwitchClient => ["switch-client", "-t", session_name],
            },
            match self {
                Self::AttachSession => "attach to the tmux session",
                Self::SwitchClient => "switch the active tmux client to the session",
            },
            self.tmux_env_policy(),
            self.stdio_policy(),
        )
    }
}

fn has_active_tmux_client_context() -> bool {
    run_tmux_output(
        ["display-message", "-p", "#{client_pid}"],
        "detect an active tmux client context",
    )
    .ok()
    .filter(|output| output.status.success())
    .is_some_and(|output| tmux_client_context_evidence(&output.stdout))
}

fn tmux_client_context_evidence(stdout: &[u8]) -> bool {
    !String::from_utf8_lossy(stdout).trim().is_empty()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Help,
    CreateSession(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxEnvPolicy {
    Preserve,
    ClearTmux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxStdioPolicy {
    Capture,
    Inherit,
}

fn run_tmux_checked<const N: usize>(args: [&str; N], action: &'static str) -> Result<(), CliError> {
    run_tmux_checked_with_options(
        args,
        action,
        TmuxEnvPolicy::Preserve,
        TmuxStdioPolicy::Capture,
    )
}

fn run_tmux_identifier<const N: usize>(
    args: [&str; N],
    action: &'static str,
) -> Result<String, CliError> {
    let output = run_tmux_output_with_env(args, action, TmuxEnvPolicy::Preserve)?;

    if output.status.success() {
        parse_tmux_identifier(&output.stdout, action)
    } else {
        Err(CliError::TmuxCommandFailed {
            action,
            details: format_tmux_output(&output),
        })
    }
}

fn run_tmux_output<const N: usize>(
    args: [&str; N],
    action: &'static str,
) -> Result<Output, CliError> {
    run_tmux_output_with_env(args, action, TmuxEnvPolicy::Preserve)
}

fn run_tmux_checked_with_options<const N: usize>(
    args: [&str; N],
    action: &'static str,
    env_policy: TmuxEnvPolicy,
    stdio_policy: TmuxStdioPolicy,
) -> Result<(), CliError> {
    match stdio_policy {
        TmuxStdioPolicy::Capture => {
            let output = run_tmux_output_with_env(args, action, env_policy)?;

            if output.status.success() {
                Ok(())
            } else {
                Err(CliError::TmuxCommandFailed {
                    action,
                    details: format_tmux_output(&output),
                })
            }
        }
        TmuxStdioPolicy::Inherit => {
            let status = run_tmux_status_with_env(args, action, env_policy)?;

            if status.success() {
                Ok(())
            } else {
                Err(CliError::TmuxCommandFailed {
                    action,
                    details: format_tmux_status(status),
                })
            }
        }
    }
}

fn run_tmux_output_with_env<const N: usize>(
    args: [&str; N],
    action: &'static str,
    env_policy: TmuxEnvPolicy,
) -> Result<Output, CliError> {
    tmux_command(args, env_policy)
        .output()
        .map_err(|error| map_tmux_io_error(error, action))
}

fn run_tmux_status_with_env<const N: usize>(
    args: [&str; N],
    action: &'static str,
    env_policy: TmuxEnvPolicy,
) -> Result<ExitStatus, CliError> {
    let mut command = tmux_command(args, env_policy);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| map_tmux_io_error(error, action))
}

fn tmux_command<const N: usize>(args: [&str; N], env_policy: TmuxEnvPolicy) -> Command {
    let mut command = Command::new("tmux");
    command.args(args);

    if matches!(env_policy, TmuxEnvPolicy::ClearTmux) {
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
    }

    command
}

fn map_tmux_io_error(error: io::Error, action: &'static str) -> CliError {
    if error.kind() == io::ErrorKind::NotFound {
        CliError::TmuxUnavailable
    } else {
        CliError::TmuxCommandFailed {
            action,
            details: error.to_string(),
        }
    }
}

fn format_tmux_output(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    match (stderr.is_empty(), stdout.is_empty()) {
        (false, true) => stderr,
        (true, false) => stdout,
        (false, false) => format!("{stderr}; stdout: {stdout}"),
        (true, true) => "tmux exited with a non-zero status without additional output".to_owned(),
    }
}

fn format_tmux_status(status: ExitStatus) -> String {
    status.to_string()
}

fn parse_tmux_identifier(stdout: &[u8], action: &'static str) -> Result<String, CliError> {
    let identifier = String::from_utf8_lossy(stdout).trim().to_owned();

    if identifier.is_empty() {
        Err(CliError::TmuxCommandFailed {
            action,
            details: "tmux exited successfully without returning the expected identifier"
                .to_owned(),
        })
    } else {
        Ok(identifier)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliError {
    MissingSessionName,
    EmptySessionName,
    TooManyArguments,
    TmuxUnavailable,
    SessionAlreadyExists(String),
    TmuxCommandFailed {
        action: &'static str,
        details: String,
    },
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::MissingSessionName | Self::EmptySessionName | Self::TooManyArguments => {
                ExitCode::from(2)
            }
            Self::TmuxUnavailable => ExitCode::from(127),
            Self::SessionAlreadyExists(_) => ExitCode::from(1),
            Self::TmuxCommandFailed { .. } => ExitCode::from(1),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSessionName => {
                write!(formatter, "error: missing session name\n{USAGE}")
            }
            Self::EmptySessionName => {
                write!(formatter, "error: session name cannot be empty\n{USAGE}")
            }
            Self::TooManyArguments => {
                write!(
                    formatter,
                    "error: expected exactly one session name\n{USAGE}"
                )
            }
            Self::TmuxUnavailable => {
                write!(
                    formatter,
                    "error: tmux is not installed or not available in PATH"
                )
            }
            Self::SessionAlreadyExists(session_name) => {
                write!(
                    formatter,
                    "error: tmux session '{session_name}' already exists"
                )
            }
            Self::TmuxCommandFailed { action, details } => {
                write!(formatter, "error: failed to {action}: {details}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::ffi::OsStr;

    use super::{
        CliCommand, CliError, HELP_TEXT, SessionEntryMode, TmuxEnvPolicy, TmuxStdioPolicy,
        WINDOW_NAMES, parse_command, parse_tmux_identifier, rollback_session_creation, run,
        session_creation_rollback_error, tmux_client_context_evidence,
    };

    #[test]
    fn parse_command_requires_an_argument() {
        assert_eq!(parse_command(["ts"]), Err(CliError::MissingSessionName));
    }

    #[test]
    fn parse_command_rejects_empty_values() {
        assert_eq!(
            parse_command(["ts", "   "]),
            Err(CliError::EmptySessionName)
        );
    }

    #[test]
    fn parse_command_rejects_extra_arguments() {
        assert_eq!(
            parse_command(["ts", "demo", "extra"]),
            Err(CliError::TooManyArguments)
        );
    }

    #[test]
    fn parse_command_trims_surrounding_whitespace() {
        assert_eq!(
            parse_command(["ts", "  demo  "]),
            Ok(CliCommand::CreateSession("demo".to_owned()))
        );
    }

    #[test]
    fn parse_command_supports_long_help_flag() {
        assert_eq!(parse_command(["ts", "--help"]), Ok(CliCommand::Help));
    }

    #[test]
    fn parse_command_supports_short_help_flag() {
        assert_eq!(parse_command(["ts", "-h"]), Ok(CliCommand::Help));
    }

    #[test]
    fn run_returns_help_text_for_long_help_flag() {
        assert_eq!(run(["ts", "--help"]), Ok(Some(HELP_TEXT.to_owned())));
    }

    #[test]
    fn run_returns_help_text_for_short_help_flag() {
        assert_eq!(run(["ts", "-h"]), Ok(Some(HELP_TEXT.to_owned())));
    }

    #[test]
    fn help_text_matches_cli_contract() {
        assert_eq!(
            HELP_TEXT,
            "Usage: ts <session-name>\n\nOptions:\n  -h, --help  Show this help and exit"
        );
    }

    #[test]
    fn window_names_match_cli_contract() {
        assert_eq!(WINDOW_NAMES, ["agent", "vim", "terminal"]);
    }

    #[test]
    fn session_entry_mode_uses_attach_session_outside_tmux() {
        assert_eq!(
            SessionEntryMode::from_tmux_context(None, false),
            SessionEntryMode::AttachSession
        );
    }

    #[test]
    fn session_entry_mode_uses_attach_session_for_empty_tmux_env() {
        assert_eq!(
            SessionEntryMode::from_tmux_context(Some(OsStr::new("")), true),
            SessionEntryMode::AttachSession
        );
    }

    #[test]
    fn session_entry_mode_uses_attach_session_for_stale_tmux_env_without_client_context() {
        assert_eq!(
            SessionEntryMode::from_tmux_context(
                Some(OsStr::new("/tmp/tmux-1000/default,123,0")),
                false,
            ),
            SessionEntryMode::AttachSession
        );
    }

    #[test]
    fn session_entry_mode_uses_switch_client_only_with_live_client_context() {
        assert_eq!(
            SessionEntryMode::from_tmux_context(
                Some(OsStr::new("/tmp/tmux-1000/default,123,0")),
                true,
            ),
            SessionEntryMode::SwitchClient
        );
    }

    #[test]
    fn attach_session_entry_clears_tmux_env_and_inherits_stdio() {
        let entry_mode = SessionEntryMode::AttachSession;

        assert_eq!(entry_mode.tmux_env_policy(), TmuxEnvPolicy::ClearTmux);
        assert_eq!(entry_mode.stdio_policy(), TmuxStdioPolicy::Inherit);
    }

    #[test]
    fn switch_client_entry_preserves_tmux_env_and_inherits_stdio() {
        let entry_mode = SessionEntryMode::SwitchClient;

        assert_eq!(entry_mode.tmux_env_policy(), TmuxEnvPolicy::Preserve);
        assert_eq!(entry_mode.stdio_policy(), TmuxStdioPolicy::Inherit);
    }

    #[test]
    fn tmux_client_context_evidence_rejects_empty_output() {
        assert!(!tmux_client_context_evidence(b"\n"));
    }

    #[test]
    fn tmux_client_context_evidence_accepts_non_empty_output() {
        assert!(tmux_client_context_evidence(b"4242\n"));
    }

    #[test]
    fn parse_tmux_identifier_trims_newlines() {
        assert_eq!(
            parse_tmux_identifier(b"@42\n", "read tmux identifier"),
            Ok("@42".to_owned())
        );
    }

    #[test]
    fn parse_tmux_identifier_rejects_empty_output() {
        assert_eq!(
            parse_tmux_identifier(b"\n", "read tmux identifier"),
            Err(CliError::TmuxCommandFailed {
                action: "read tmux identifier",
                details: "tmux exited successfully without returning the expected identifier"
                    .to_owned(),
            })
        );
    }

    #[test]
    fn rollback_session_creation_skips_cleanup_after_success() {
        let cleanup_called = Cell::new(false);

        let result = rollback_session_creation("demo", Ok(()), |_| {
            cleanup_called.set(true);
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert!(!cleanup_called.get());
    }

    #[test]
    fn rollback_session_creation_returns_original_error_after_successful_cleanup() {
        let cleanup_session_name = RefCell::new(None);
        let setup_error = CliError::TmuxCommandFailed {
            action: "rename a tmux window",
            details: "rename failed".to_owned(),
        };

        let result =
            rollback_session_creation::<(), _>("demo", Err(setup_error.clone()), |session_name| {
                cleanup_session_name.replace(Some(session_name.to_owned()));
                Ok(())
            });

        assert_eq!(result, Err(setup_error));
        assert_eq!(cleanup_session_name.into_inner(), Some("demo".to_owned()));
    }

    #[test]
    fn rollback_session_creation_reports_cleanup_failure_with_original_context() {
        let setup_error = CliError::TmuxCommandFailed {
            action: "create tmux windows",
            details: "new-window failed".to_owned(),
        };
        let cleanup_error = CliError::TmuxCommandFailed {
            action: "clean up the partially created tmux session",
            details: "kill-session failed".to_owned(),
        };

        let result = rollback_session_creation::<(), _>("demo", Err(setup_error.clone()), |_| {
            Err(cleanup_error.clone())
        });

        assert_eq!(
            result,
            Err(session_creation_rollback_error(setup_error, cleanup_error))
        );
    }
}
