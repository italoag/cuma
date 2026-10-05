//! ai-jail, bubblewrap, `sandbox-exec` and firejail — the profile in
//! [`cuma_workspace::confine`], as a provider.

use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, WorkspaceAccess,
};
use async_trait::async_trait;
use cuma_config::SecurityConfig;
use cuma_config::sandbox::NativeSandbox;
use cuma_core::error::{MetaAgentError, Result};
use cuma_workspace::{AgentRuntime, AgentSandbox};

/// `kind = "native"`, and the built-in names `auto`, `ai-jail`,
/// `bubblewrap`, `sandbox-exec` and `firejail`.
pub struct NativeProvider {
    name: String,
    sandbox: AgentSandbox,
}

impl NativeProvider {
    /// `runtime` is `auto` or a native runtime's configuration name.
    pub fn new(
        name: impl Into<String>,
        settings: &NativeSandbox,
        security: &SecurityConfig,
    ) -> Self {
        let sandbox = match AgentRuntime::from_config_name(&settings.runtime) {
            Some(runtime) => AgentSandbox::detect_only(security, runtime),
            None => AgentSandbox::detect(security),
        };
        Self {
            name: name.into(),
            sandbox,
        }
    }

    /// A provider around an already-built confinement — for tests.
    pub fn with_sandbox(name: impl Into<String>, sandbox: AgentSandbox) -> Self {
        Self {
            name: name.into(),
            sandbox,
        }
    }

    /// The confinement, for `cuma doctor` and the shortfall warnings.
    pub fn confinement(&self) -> &AgentSandbox {
        &self.sandbox
    }

    fn prefix(&self, request: &LaunchRequest) -> Vec<String> {
        self.sandbox
            .clone()
            .with_state(request.state.iter().cloned())
            .launch_prefix(&request.workspace, &request.keep_env, &request.readable)
            .unwrap_or_default()
    }
}

#[async_trait]
impl SandboxProvider for NativeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "native"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::Process,
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: self.sandbox.filters_network(),
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.sandbox
            .runtime()
            .map(|runtime| runtime.program().to_owned())
            .unwrap_or_default()
    }

    fn prefix_hint(&self, request: &LaunchRequest) -> Vec<String> {
        self.prefix(request)
    }

    async fn probe(&self) -> Result<()> {
        // Detection already ran `true` inside the runtime.
        match self.sandbox.runtime() {
            Some(_) => Ok(()),
            None => Err(MetaAgentError::Configuration(self.sandbox.describe())),
        }
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        Ok(SandboxLaunch::new(self.prefix(request)))
    }

    fn describe(&self) -> String {
        self.sandbox.describe()
    }

    fn shortfall(&self, _allowed_hosts: &[String]) -> Option<String> {
        self.sandbox
            .level()
            .is_shortfall()
            .then(|| self.sandbox.describe())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;

    #[tokio::test]
    async fn an_agents_state_becomes_writable_under_ai_jail() {
        let security = SecurityConfig::default();
        let sandbox = AgentSandbox::with_runtime(&security, Some(AgentRuntime::AiJail));
        let provider = NativeProvider::with_sandbox("jail", sandbox);
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.state = vec![state.path().to_path_buf()];

        let launch = provider.open(&request).await.unwrap();

        let prefix = launch.prefix();
        assert_eq!(prefix[0], "ai-jail");
        let state = std::fs::canonicalize(state.path())
            .unwrap()
            .display()
            .to_string();
        assert!(
            prefix
                .windows(2)
                .any(|w| w[0] == "--rw-map" && w[1] == state),
            "{prefix:?}"
        );
        assert!(!launch.has_session(), "a wrapper has nothing to tear down");
    }

    #[tokio::test]
    async fn without_a_working_runtime_the_agent_runs_unconfined_and_the_probe_says_why() {
        let security = SecurityConfig::default();
        let provider =
            NativeProvider::with_sandbox("none", AgentSandbox::with_runtime(&security, None));
        let ws = tempfile::tempdir().unwrap();
        let launch = provider
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        assert!(launch.prefix().is_empty());
        let err = provider.probe().await.unwrap_err().to_string();
        assert!(err.contains("UNCONFINED"), "{err}");
    }

    #[test]
    fn only_ai_jail_is_reported_as_filtering_the_network() {
        let security = SecurityConfig::default();
        for (runtime, filters) in [
            (AgentRuntime::AiJail, true),
            (AgentRuntime::SandboxExec, false),
            (AgentRuntime::Bubblewrap, false),
        ] {
            let provider = NativeProvider::with_sandbox(
                "n",
                AgentSandbox::with_runtime(&security, Some(runtime)),
            );
            assert_eq!(
                provider.capabilities().network_allowlist,
                filters,
                "{runtime:?}"
            );
        }
    }
}
