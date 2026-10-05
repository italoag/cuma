//! Wasmer: agents compiled to WASI or WASIX, run with `wasmer run`.
//!
//! A guest sees no file and no network the host did not grant. The workspace
//! and the agent's state are granted with `--volume`, the network with
//! `--net` — narrowed to DNS rules when an allowlist is configured — and the
//! environment with `--forward-host-env`, after every variable the agent must
//! not see has been removed by name with `env -u`.
//!
//! The agent's command is a Wasm package or `.wasm` file, then `--`, then its
//! own arguments: `wasmer run` would otherwise read them as its own.

use crate::process::{Input, Output, run};
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, WorkspaceAccess,
    forwarded_env, guest_readable_dirs, writable_mounts,
};
use async_trait::async_trait;
use cuma_config::sandbox::WasmerSandbox;
use cuma_core::error::Result;
use std::time::Duration;

/// The smallest module there is to run: one type, one function, exported as
/// `_start`, that returns at once.
const PROBE_MODULE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version 1
    0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
    0x03, 0x02, 0x01, 0x00, // one function of that type
    0x07, 0x0a, 0x01, 0x06, b'_', b's', b't', b'a', b'r', b't', 0x00, 0x00, // export _start
    0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // its body: end
];

/// `kind = "wasmer"`.
pub struct WasmerProvider {
    name: String,
    settings: WasmerSandbox,
}

impl WasmerProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: WasmerSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }

    fn prefix(&self, request: &LaunchRequest) -> Vec<String> {
        let kept = forwarded_env(request);
        let mut removed: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| !kept.contains(name))
            .collect();
        removed.sort();

        let mut prefix = vec!["env".to_owned()];
        for name in removed {
            prefix.extend(["-u".to_owned(), name]);
        }
        let workspace = crate::canonical(&request.workspace);
        prefix.extend([self.settings.program.clone(), "run".to_owned()]);
        // A guest has no home of its own: state is seen where it lives.
        let home = dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_default();
        for (host, guest) in writable_mounts(request, &home) {
            prefix.extend(["--volume".to_owned(), format!("{host}:{guest}")]);
        }
        // `--volume` takes directories only, and has no read-only form.
        for path in guest_readable_dirs(request) {
            prefix.extend(["--volume".to_owned(), format!("{path}:{path}")]);
        }
        prefix.extend(["--cwd".to_owned(), workspace]);
        if request.allowed_hosts.is_empty() {
            prefix.push("--net".to_owned());
        } else {
            let rules: Vec<String> = request
                .allowed_hosts
                .iter()
                .map(|host| format!("dns:allow={host}:*"))
                .collect();
            prefix.push(format!("--net={}", rules.join(",")));
        }
        prefix.push("--forward-host-env".to_owned());
        prefix.extend(self.settings.extra_args.iter().cloned());
        prefix
    }
}

#[async_trait]
impl SandboxProvider for WasmerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "wasmer"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::Wasm,
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: true,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    fn prefix_hint(&self, request: &LaunchRequest) -> Vec<String> {
        self.prefix(request)
    }

    /// The runtime itself; an agent's package is checked when it negotiates.
    async fn probe(&self) -> Result<()> {
        // Something must run inside it: a module whose `_start` returns,
        // with a directory mapped as a launch maps the workspace.
        let dir = tempfile::Builder::new()
            .prefix("cuma-wasmer-")
            .tempdir()
            .map_err(|e| crate::failure(&self.name, e))?;
        let module = dir.path().join("probe.wasm");
        std::fs::write(&module, PROBE_MODULE).map_err(|e| crate::failure(&self.name, e))?;
        let mapped = crate::canonical(dir.path());
        let args = [
            "run".to_owned(),
            "--volume".to_owned(),
            format!("{mapped}:{mapped}"),
            "--cwd".to_owned(),
            mapped,
            module.display().to_string(),
        ];
        run(
            &self.name,
            &self.settings.program,
            &args,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        Ok(SandboxLaunch::new(self.prefix(request)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;

    fn provider() -> WasmerProvider {
        WasmerProvider::new("wasm", WasmerSandbox::default())
    }

    #[test]
    fn the_workspace_is_a_volume_and_the_environment_is_filtered_by_name() {
        let ws = tempfile::tempdir().unwrap();
        let path = crate::canonical(ws.path());
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.keep_env = vec!["PATH".into()];
        let prefix = provider().prefix(&request);

        assert_eq!(prefix[0], "env");
        let run = prefix.iter().position(|w| w == "wasmer").unwrap();
        assert_eq!(prefix[run + 1], "run");
        assert!(
            prefix
                .windows(2)
                .any(|w| w == ["--volume", &format!("{path}:{path}")])
        );
        assert!(prefix.windows(2).any(|w| w == ["--cwd", &path]));
        assert_eq!(prefix.last().unwrap(), "--forward-host-env");
        // PATH was asked for: it is not removed. HOME was not: it is.
        let removed: Vec<&String> = prefix[..run].iter().skip(1).step_by(2).collect();
        assert!(removed.iter().all(|flag| *flag == "-u"));
        let names: Vec<&String> = prefix[..run].iter().skip(2).step_by(2).collect();
        assert!(!names.iter().any(|n| *n == "PATH"));
        assert!(names.iter().any(|n| *n == "HOME"));
        let home = std::env::var("HOME").unwrap();
        assert!(
            !prefix.iter().any(|w| w.contains('=') && w.contains(&home)),
            "no values"
        );
    }

    #[test]
    fn an_allowlist_becomes_dns_rules() {
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.allowed_hosts = vec!["api.anthropic.com".into(), "github.com".into()];
        let prefix = provider().prefix(&request);
        assert!(
            prefix
                .contains(&"--net=dns:allow=api.anthropic.com:*,dns:allow=github.com:*".to_owned())
        );
        assert!(provider().capabilities().network_allowlist);

        let open = provider().prefix(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute));
        assert!(open.contains(&"--net".to_owned()));
    }

    #[tokio::test]
    async fn the_probe_runs_a_module_rather_than_asking_the_version() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        // A stand-in for wasmer: fails unless handed a real module to run.
        let fake = dir.path().join("wasmer");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\nfor last; do :; done\n\
                 [ \"$(head -c 4 \"$last\" | od -An -c | tr -d ' ')\" = '\\0asm' ]\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = WasmerProvider::new(
            "w",
            WasmerSandbox {
                program: fake.display().to_string(),
                ..WasmerSandbox::default()
            },
        );

        p.probe().await.unwrap();

        let call = std::fs::read_to_string(log).unwrap();
        assert!(call.starts_with("run --volume "), "{call}");
        assert!(call.trim_end().ends_with("probe.wasm"), "{call}");
    }
}
