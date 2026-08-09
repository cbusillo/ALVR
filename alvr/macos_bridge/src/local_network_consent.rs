use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::json;

const CONSENT_MODE: &str = "--local-network-consent";
const RESULT_PATH: &str = "--result-path";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalNetworkConsentArgs {
    pub result_path: PathBuf,
}

impl LocalNetworkConsentArgs {
    pub fn from_process_args() -> Result<Option<Self>> {
        Self::parse(std::env::args_os().skip(1))
    }

    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<Self>> {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        if arguments.is_empty() {
            return Ok(None);
        }
        if arguments[0] != CONSENT_MODE {
            bail!("unsupported argument: {}", arguments[0].to_string_lossy());
        }
        if arguments.len() != 3 || arguments[1] != RESULT_PATH {
            bail!("usage: alvr_macos_bridge {CONSENT_MODE} {RESULT_PATH} <absolute-path>");
        }
        let result_path = PathBuf::from(&arguments[2]);
        if !result_path.is_absolute() {
            bail!("local network consent result path must be absolute");
        }
        Ok(Some(Self { result_path }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalNetworkConsentOutcome {
    Ready,
    PolicyDenied,
    Waiting,
    Failed,
    Cancelled,
    SetupFailed,
}

impl LocalNetworkConsentOutcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Ready => 0,
            Self::PolicyDenied => 2,
            Self::Waiting => 3,
            Self::Failed => 4,
            Self::Cancelled => 5,
            Self::SetupFailed => 6,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::PolicyDenied => "policy-denied",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::SetupFailed => "setup-failed",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Ready => "Bonjour discovery is currently available for this executable identity.",
            Self::PolicyDenied => {
                "Local Network access is denied. Enable ALVR macOS Bridge in System Settings."
            }
            Self::Waiting => {
                "Bonjour discovery is waiting. Respond to the macOS prompt or check network state."
            }
            Self::Failed => "Bonjour discovery failed before readiness could be established.",
            Self::Cancelled => {
                "The consent window closed before network readiness was established."
            }
            Self::SetupFailed => {
                "The foreground Local Network consent browser could not be created."
            }
        }
    }
}

unsafe extern "C" {
    fn alvr_run_local_network_consent() -> i32;
}

pub fn run_local_network_consent(
    arguments: &LocalNetworkConsentArgs,
) -> Result<LocalNetworkConsentOutcome> {
    let raw_outcome = unsafe { alvr_run_local_network_consent() };
    let outcome = match raw_outcome {
        0 => LocalNetworkConsentOutcome::Ready,
        1 => LocalNetworkConsentOutcome::PolicyDenied,
        2 => LocalNetworkConsentOutcome::Waiting,
        3 => LocalNetworkConsentOutcome::Failed,
        4 => LocalNetworkConsentOutcome::Cancelled,
        _ => LocalNetworkConsentOutcome::SetupFailed,
    };
    write_result(&arguments.result_path, outcome)?;
    Ok(outcome)
}

fn write_result(path: &Path, outcome: LocalNetworkConsentOutcome) -> Result<()> {
    let parent = path
        .parent()
        .context("local network consent result path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .context("local network consent result path has no file name")?
            .to_string_lossy(),
        std::process::id()
    ));
    let payload = json!({
        "schemaVersion": 1,
        "mode": "local-network-consent",
        "outcome": outcome.name(),
        "networkAvailable": match outcome {
            LocalNetworkConsentOutcome::Ready => Some(true),
            LocalNetworkConsentOutcome::PolicyDenied => Some(false),
            _ => None,
        },
        "message": outcome.message(),
    });
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    output
        .write_all(format!("{}\n", serde_json::to_string_pretty(&payload)?).as_bytes())
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    output
        .sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_consent_mode() {
        let parsed = LocalNetworkConsentArgs::parse([
            OsString::from(CONSENT_MODE),
            OsString::from(RESULT_PATH),
            OsString::from("/tmp/alvr-consent.json"),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            Some(LocalNetworkConsentArgs {
                result_path: PathBuf::from("/tmp/alvr-consent.json")
            })
        );
    }

    #[test]
    fn ordinary_launch_has_no_consent_mode() {
        assert_eq!(LocalNetworkConsentArgs::parse([]).unwrap(), None);
    }

    #[test]
    fn rejects_relative_result_path() {
        let result = LocalNetworkConsentArgs::parse([
            OsString::from(CONSENT_MODE),
            OsString::from(RESULT_PATH),
            OsString::from("relative.json"),
        ]);
        assert!(result.is_err());
    }
}
