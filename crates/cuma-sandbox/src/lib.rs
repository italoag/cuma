//! Agent sandbox providers.
//!
//! A provider turns "launch this agent in this workspace" into a
//! [`SandboxLaunch`]: the prefix the agent's command runs under, plus, for a
//! sandbox with a lifecycle of its own, a session that is finished once the
//! agent exits. ACP is JSON-RPC over the agent's stdio, so a prefix is all the
//! protocol needs.
//!
//! | Module | Sandbox | Workspace |
//! |---|---|---|
//! | [`native`] | ai-jail, bubblewrap, `sandbox-exec`, firejail | mounted |
//! | [`docker`] | Docker, Podman, nerdctl and compatible engines | mounted |
//! | [`microsandbox`] | microsandbox microVMs | mounted |
//! | [`arcbox`] | ArcBox sandbox microVMs | copied |
//! | [`kubernetes`] | kubernetes-sigs/agent-sandbox | copied |
//! | [`e2b`] | CubeSandbox, E2B | copied |
//! | [`opensandbox`] | OpenSandbox | mounted or copied |
//! | [`wasmer`] | Wasmer | mounted |
//! | [`command`] | a configured prefix | mounted |
//! | [`plugin`] | an external program | either |
//!
//! A *copied* workspace is uploaded before the agent starts and merged back
//! afterwards by [`sync`]. Providers that only speak HTTP are reached through
//! CUMA itself ([`bridge`]). See `docs/SANDBOXES.md`.

pub mod arcbox;
pub mod bridge;
pub mod command;
pub mod docker;
pub mod e2b;
pub mod kubernetes;
pub mod microsandbox;
pub mod native;
pub mod opensandbox;
pub mod plugin;
mod process;
pub mod registry;
pub mod sync;
pub mod wasmer;

pub use registry::{Registry, SandboxEntry};

use async_trait::async_trait;
use cuma_core::error::Result;
pub use cuma_core::ports::{LaunchPurpose, SandboxLaunch, SandboxSession};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// What separates the agent from the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A process sandbox on the host's kernel.
    Process,
    /// A container: namespaces, or a userspace kernel such as gVisor.
    Container,
    /// A virtual machine with its own kernel.
    #[serde(rename = "microvm")]
    MicroVm,
    /// A WebAssembly runtime.
    Wasm,
    /// Somewhere else entirely.
    Remote,
    /// Not known until the provider says so.
    Unknown,
}

impl Isolation {
    /// From the names configuration uses.
    pub fn from_name(name: &str) -> Self {
        match name {
            "process" => Self::Process,
            "container" => Self::Container,
            "microvm" => Self::MicroVm,
            "wasm" => Self::Wasm,
            "remote" => Self::Remote,
            _ => Self::Unknown,
        }
    }
}

/// How the agent sees the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    /// Mounted at the same path: the agent's edits land directly.
    Mounted,
    /// Uploaded before the agent starts, merged back after it exits.
    Copied,
}

/// What a provider can and cannot do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// What separates the agent from the machine.
    pub isolation: Isolation,
    /// How the agent sees the workspace.
    pub workspace: WorkspaceAccess,
    /// Whether `security.network_allowlist` is enforced.
    pub network_allowlist: bool,
    /// Whether a credential can be used without its value entering the
    /// sandbox.
    pub secrets_outside: bool,
}

/// Everything about one launch a provider needs to know.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// Where the agent works.
    pub workspace: PathBuf,
    /// Negotiation, or a task.
    pub purpose: LaunchPurpose,
    /// Variables the agent keeps, by name. Never values.
    pub keep_env: Vec<String>,
    /// Paths the agent's own command names; they stay readable.
    pub readable: Vec<PathBuf>,
    /// Directories the agent keeps its login and sessions in.
    pub state: Vec<PathBuf>,
    /// Hosts the agent may reach; empty means unrestricted.
    pub allowed_hosts: Vec<String>,
}

impl LaunchRequest {
    /// A request with nothing but a workspace, for probes and tests.
    pub fn bare(workspace: impl Into<PathBuf>, purpose: LaunchPurpose) -> Self {
        Self {
            workspace: workspace.into(),
            purpose,
            keep_env: Vec::new(),
            readable: Vec::new(),
            state: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }

    /// Whether the agent's work must come back: a task in a copied sandbox.
    pub fn collects(&self) -> bool {
        self.purpose == LaunchPurpose::Execute
    }
}

/// A way to run an agent confined.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// The `[sandboxes.<name>]` it was configured as.
    fn name(&self) -> &str;

    /// The configured `kind`.
    fn kind(&self) -> &'static str;

    /// What it can and cannot do.
    fn capabilities(&self) -> Capabilities;

    /// The program a launch needs on this machine.
    fn program(&self) -> String;

    /// What a launch's prefix looks like, without preparing one — for
    /// display and for checking the program is installed. Only a wrapper's
    /// is exact; a sandbox with a lifecycle names its program.
    fn prefix_hint(&self, request: &LaunchRequest) -> Vec<String> {
        let _ = request;
        vec![self.program()]
    }

    /// Run something inside it, now. Ok only once that succeeded.
    async fn probe(&self) -> Result<()>;

    /// Prepare one launch.
    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch>;

    /// One line on what agents under it get, for `cuma doctor`.
    fn describe(&self) -> String {
        let c = self.capabilities();
        let isolation = serde_json::to_value(c.isolation)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let workspace = match c.workspace {
            WorkspaceAccess::Mounted => "mounted",
            WorkspaceAccess::Copied => "copied",
        };
        format!(
            "agents under {}: {} ({isolation}), workspace {workspace}",
            self.name(),
            self.kind()
        )
    }

    /// What it falls short of under this configuration, if anything — a
    /// network allowlist it cannot enforce, a runtime that does not work.
    fn shortfall(&self, allowed_hosts: &[String]) -> Option<String> {
        (!allowed_hosts.is_empty() && !self.capabilities().network_allowlist).then(|| {
            format!(
                "agents under {}: security.network_allowlist is NOT enforced — {} cannot filter \
                 by host; the network is open",
                self.name(),
                self.kind()
            )
        })
    }
}

/// A fresh name for one launch's sandbox: `cuma-` and twelve hex digits —
/// valid as a container name, a DNS label and a microVM name.
pub(crate) fn session_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("cuma-{}", &id[..12])
}

/// The variables a sandbox that starts from nothing should receive, by name:
/// what was asked for, plus the agent credentials and proxy settings present
/// in this environment. Only names that are set are returned.
pub(crate) fn forwarded_env(request: &LaunchRequest) -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            request.keep_env.contains(name) || cuma_workspace::confine::is_portable_agent_env(name)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Where a host directory is seen inside a guest whose home is `guest_home`:
/// under the guest's home when it lives under the host's, at the same path
/// otherwise.
pub(crate) fn guest_path(host: &Path, guest_home: &str) -> String {
    match dirs::home_dir()
        .and_then(|home| host.strip_prefix(home).ok().map(Path::to_path_buf))
        .filter(|rest| !rest.as_os_str().is_empty())
    {
        Some(rest) => format!("{}/{}", guest_home.trim_end_matches('/'), rest.display()),
        None => host.display().to_string(),
    }
}

/// The directories to mount writable into a mounted-workspace guest: the
/// workspace, a worktree's repository directory, and the agent's state.
/// Each pair is (host, guest).
pub(crate) fn writable_mounts(request: &LaunchRequest, guest_home: &str) -> Vec<(String, String)> {
    let workspace = canonical(&request.workspace);
    let mut mounts = vec![(workspace.clone(), workspace.clone())];
    if let Some(common) = cuma_workspace::confine::git_common_dir(Path::new(&workspace)) {
        let common = common.display().to_string();
        mounts.push((common.clone(), common));
    }
    for state in request.state.iter().filter(|p| p.exists()) {
        mounts.push((canonical(state), guest_path(state, guest_home)));
    }
    mounts
}

/// System directories a guest has its own of. A host file there — a macOS
/// `/bin/sh`, say — would replace the guest's and could not even run.
const GUEST_SYSTEM: [&str; 13] = [
    "/bin", "/sbin", "/usr", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/proc", "/sys",
    "/dev", "/System", "/Library",
];

/// The paths an agent's command names that a guest should see: those that
/// exist, outside the guest's own system directories. Canonical.
pub(crate) fn guest_readable(request: &LaunchRequest) -> Vec<String> {
    request
        .readable
        .iter()
        .filter(|p| p.exists())
        .map(|p| canonical(p))
        .filter(|p| {
            !GUEST_SYSTEM
                .iter()
                .any(|system| p == system || p.starts_with(&format!("{system}/")))
        })
        .collect()
}

/// [`guest_readable`] for sandboxes that mount only directories: a named file
/// brings its directory — never the home directory or one above it, which
/// would expose far more than the file. What the workspace mount already
/// covers is left out.
pub(crate) fn guest_readable_dirs(request: &LaunchRequest) -> Vec<String> {
    let workspace = canonical(&request.workspace);
    let home = dirs::home_dir().map(|h| canonical(&h));
    let within = |path: &str, dir: &str| path == dir || path.starts_with(&format!("{dir}/"));
    let mut found: Vec<String> = guest_readable(request)
        .into_iter()
        .filter_map(|path| {
            let path = Path::new(&path);
            if path.is_dir() {
                Some(path.display().to_string())
            } else {
                path.parent().map(|p| p.display().to_string())
            }
        })
        .filter(|dir| dir != "/" && !home.as_deref().is_some_and(|home| within(home, dir)))
        .filter(|dir| !within(dir, &workspace))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// A path as the host resolves it, as a string.
pub(crate) fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// A sandbox operation failed.
pub(crate) fn failure(sandbox: &str, message: impl std::fmt::Display) -> cuma_core::MetaAgentError {
    cuma_core::MetaAgentError::protocol_msg("sandbox", format!("sandbox {sandbox}: {message}"))
}

/// `sh -c` wrapper run before the agent inside sandboxes that are entered by
/// `exec`: load the environment file if there is one, enter the workspace,
/// then become the agent. `$1` is the workspace; the agent follows.
pub(crate) const ENTER: &str = "if [ -f /tmp/cuma-env ]; then set -a; . /tmp/cuma-env; set +a; fi; \
cd \"$1\" 2>/dev/null; shift; exec \"$@\"";

/// The contents of an environment file for `names`, in `sh` syntax, with the
/// values taken from this process's environment. Written into a sandbox with
/// mode 0600 and sourced by [`ENTER`]; never placed on a command line.
pub(crate) fn env_file(names: &[String]) -> String {
    env_file_with(names, |name| std::env::var(name).ok())
}

fn env_file_with(names: &[String], value: impl Fn(&str) -> Option<String>) -> String {
    names
        .iter()
        .filter_map(|name| {
            let value = value(name)?;
            Some(format!("{name}='{}'\n", value.replace('\'', "'\\''")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn session_ids_are_valid_dns_labels_and_unique() {
        let a = session_id();
        let b = session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 17);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn a_hosts_system_binary_is_never_mounted_into_a_guest() {
        // A macOS /bin/sh mounted over a Linux guest's cannot even execute.
        let script = tempfile::NamedTempFile::new().unwrap();
        let mut request = LaunchRequest::bare("/w", LaunchPurpose::Execute);
        request.readable = vec![
            PathBuf::from("/bin/sh"),
            PathBuf::from("/usr/bin/env"),
            script.path().to_path_buf(),
        ];
        assert_eq!(guest_readable(&request), [canonical(script.path())]);
    }

    #[test]
    fn a_named_file_brings_its_directory_but_never_the_home_directory() {
        let ws = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let script = tools.path().join("agent.sh");
        std::fs::write(&script, "").unwrap();
        let inside = ws.path().join("local.sh");
        std::fs::write(&inside, "").unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.readable = vec![script, inside, dirs::home_dir().unwrap().join(".zshrc")];

        // The tools directory comes; the workspace already does; the home
        // directory never does, whatever file in it was named.
        assert_eq!(guest_readable_dirs(&request), [canonical(tools.path())]);
    }

    #[test]
    fn a_home_directory_is_seen_under_the_guest_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            guest_path(&home.join(".config/devin"), "/root"),
            "/root/.config/devin"
        );
        assert_eq!(guest_path(Path::new("/opt/state"), "/root"), "/opt/state");
    }

    #[test]
    fn an_environment_file_quotes_values_for_sh() {
        let value = "it's \"quoted\" $HOME";
        let file = env_file_with(&["TOKEN".to_owned(), "UNSET".to_owned()], |name| {
            (name == "TOKEN").then(|| value.to_owned())
        });
        assert_eq!(
            file, "TOKEN='it'\\''s \"quoted\" $HOME'\n",
            "unset names are left out"
        );
        // Values must survive `sh` sourcing unchanged.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{file}printf %s \"$TOKEN\""))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), value);
    }
}
