//! Confining coding agents.
//!
//! A coding agent is not an arbitrary command. It needs its model API, its own
//! login, a clean stdio channel for ACP's JSON-RPC, and somewhere to write its
//! state — and it runs whatever shell commands it decides to. Confining one is
//! therefore not "wrap it and cut the network": that breaks it outright.
//!
//! The profile here follows ai-jail's, which was built for exactly this:
//!
//! | | |
//! |---|---|
//! | System (`/usr`, `/etc`, `/opt`, …) | read-only |
//! | `$HOME` | private and empty, except… |
//! | agent and toolchain state (`~/.claude`, `~/.codex`, `~/.cache`, `~/.npm`, …) | writable |
//! | other dotfiles (`~/.gitconfig`, `~/.nvm`, …) | read-only |
//! | credentials (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.docker`, …) | absent |
//! | `/tmp`, `/run` | private |
//! | the workspace, and its repository's git directory | writable |
//! | environment | a baseline and what `security.agent_env` names; the rest is removed |
//!
//! ai-jail is used when present. Otherwise the same profile is rendered for
//! bubblewrap, macOS `sandbox-exec` or firejail — whichever is installed and
//! actually works here, which is checked by running something inside it: a
//! runtime that exists but is blocked (an AppArmor-restricted bubblewrap, say)
//! must not be reported as protecting anything.
//!
//! Only ai-jail can filter the network by host. Under the others the network is
//! open, and a configured `security.network_allowlist` is reported as not
//! enforced rather than silently ignored.

use cuma_config::SecurityConfig;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Home directories never visible to an agent: credentials and personal data.
///
/// From ai-jail's deny list.
const HIDDEN_IN_HOME: &[&str] = &[
    ".gnupg",
    ".aws",
    ".ssh",
    ".mozilla",
    ".thunderbird",
    ".basilisk-dev",
    ".sparrow",
    ".docker",
    ".kube",
    ".azure",
    ".password-store",
    ".netrc",
    ".pgpass",
    ".git-credentials",
];

/// Home entries an agent may write: agents' own state and the caches of the
/// toolchains they drive. From ai-jail's list, plus `.claude.json`, where
/// Claude Code keeps its login.
const WRITABLE_IN_HOME: &[&str] = &[
    ".gemini",
    ".claude",
    ".claude.json",
    ".jcode",
    ".crush",
    ".codex",
    ".aider",
    ".kiro",
    ".grok",
    ".agents",
    ".config",
    ".cargo",
    ".cache",
    ".bundle",
    ".gem",
    ".rustup",
    ".npm",
    ".bun",
    ".deno",
    ".yarn",
    ".pnpm",
    ".m2",
    ".gradle",
    ".dotnet",
    ".nuget",
    ".pub-cache",
    ".mix",
    ".hex",
];

/// Environment variables an agent keeps: what a process needs to run, reach
/// the network through a proxy, find its toolchains, and log in the way
/// coding agents usually do.
const KEPT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "GOMODCACHE",
    "JAVA_HOME",
    "NVM_DIR",
    "VOLTA_HOME",
    "PNPM_HOME",
    "BUN_INSTALL",
    "DENO_DIR",
    "PYENV_ROOT",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

/// Prefixes of kept variables: locale, XDG directories, and the agents' own
/// configuration and credentials (`ANTHROPIC_API_KEY`, `CODEX_HOME`, …).
const KEPT_ENV_PREFIXES: &[&str] = &[
    "LC_",
    "XDG_",
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "CODEX_",
    "GEMINI_",
    "GOOGLE_GENAI_",
];

/// A runtime that can confine an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRuntime {
    /// ai-jail: built for this, and the only one that filters by host.
    AiJail,
    /// bubblewrap (Linux).
    Bubblewrap,
    /// `sandbox-exec` (macOS).
    SandboxExec,
    /// firejail (Linux).
    Firejail,
}

impl AgentRuntime {
    /// In order of preference.
    const ALL: [Self; 4] = [
        Self::AiJail,
        Self::Bubblewrap,
        Self::SandboxExec,
        Self::Firejail,
    ];

    /// The binary.
    pub fn program(self) -> &'static str {
        match self {
            Self::AiJail => "ai-jail",
            Self::Bubblewrap => "bwrap",
            Self::SandboxExec => "sandbox-exec",
            Self::Firejail => "firejail",
        }
    }

    /// A name for people.
    pub fn name(self) -> &'static str {
        match self {
            Self::AiJail => "ai-jail",
            Self::Bubblewrap => "bubblewrap",
            Self::SandboxExec => "macOS sandbox-exec",
            Self::Firejail => "firejail",
        }
    }

    /// Whether the runtime is installed *and* works here.
    ///
    /// Checked once per process by running `true` inside it.
    fn works(self) -> bool {
        static CHECKED: OnceLock<Vec<AgentRuntime>> = OnceLock::new();
        CHECKED
            .get_or_init(|| {
                Self::ALL
                    .into_iter()
                    .filter(|runtime| runtime.probe())
                    .collect()
            })
            .contains(&self)
    }

    fn probe(self) -> bool {
        if which::which(self.program()).is_err() {
            return false;
        }
        let truth = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap_or("true");
        let args: Vec<&str> = match self {
            // ai-jail builds its own profile; asking for its version is enough
            // to know the binary is ai-jail and runs.
            Self::AiJail => vec!["--version"],
            Self::Bubblewrap => vec![
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--unshare-pid",
                "--die-with-parent",
                "--",
                truth,
            ],
            Self::SandboxExec => vec!["-p", "(version 1)(allow default)", truth],
            Self::Firejail => vec!["--quiet", "--noprofile", "--", truth],
        };
        std::process::Command::new(self.program())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// How agents are confined on this machine.
#[derive(Debug, Clone)]
pub struct AgentSandbox {
    runtime: Option<AgentRuntime>,
    enabled: bool,
    allowed_hosts: Vec<String>,
    forwarded_env: Vec<String>,
    writable: Vec<PathBuf>,
    required: bool,
}

/// What confinement an agent gets, for `cuma doctor` and warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentConfinementLevel {
    /// Filesystem and, where configured, network confined.
    Confined {
        /// The runtime doing it.
        runtime: &'static str,
    },
    /// Filesystem confined; a configured network allowlist is not enforced.
    NetworkUnfiltered {
        /// The runtime doing it.
        runtime: &'static str,
    },
    /// Not confined, because sandboxing is off.
    Disabled,
    /// Not confined, because nothing here can.
    Unavailable,
}

impl AgentConfinementLevel {
    /// Whether this is less than was asked for.
    pub fn is_shortfall(&self) -> bool {
        matches!(self, Self::NetworkUnfiltered { .. } | Self::Unavailable)
    }
}

impl AgentSandbox {
    /// Detect how agents can be confined under `config`.
    pub fn detect(config: &SecurityConfig) -> Self {
        let runtime = if config.sandbox {
            AgentRuntime::ALL.into_iter().find(|r| r.works())
        } else {
            None
        };
        Self::with_runtime(config, runtime)
    }

    /// Confinement by a given runtime, whether or not it is installed — for
    /// tests, and for callers that have already chosen.
    pub fn with_runtime(config: &SecurityConfig, runtime: Option<AgentRuntime>) -> Self {
        Self {
            runtime: runtime.filter(|_| config.sandbox),
            enabled: config.sandbox,
            allowed_hosts: config.network_allowlist.clone(),
            forwarded_env: config.agent_env.clone(),
            writable: config
                .agent_writable_paths
                .iter()
                .map(|p| cuma_config::expand_home(p))
                .collect(),
            required: config.require_agent_sandbox,
        }
    }

    /// The runtime in use, if any.
    pub fn runtime(&self) -> Option<AgentRuntime> {
        self.runtime
    }

    /// What confinement agents get.
    pub fn level(&self) -> AgentConfinementLevel {
        match self.runtime {
            None if !self.enabled => AgentConfinementLevel::Disabled,
            None => AgentConfinementLevel::Unavailable,
            Some(runtime) if runtime != AgentRuntime::AiJail && !self.allowed_hosts.is_empty() => {
                AgentConfinementLevel::NetworkUnfiltered {
                    runtime: runtime.name(),
                }
            }
            Some(runtime) => AgentConfinementLevel::Confined {
                runtime: runtime.name(),
            },
        }
    }

    /// Whether agents must not run, because confinement was required and
    /// falls short.
    pub fn refuses_agents(&self) -> bool {
        self.required && self.level().is_shortfall()
    }

    /// One line on whether agents run confined.
    pub fn describe(&self) -> String {
        match self.level() {
            AgentConfinementLevel::Confined { runtime } => {
                format!("agents: confined by {runtime}")
            }
            AgentConfinementLevel::NetworkUnfiltered { runtime } => format!(
                "agents: filesystem confined by {runtime}, but security.network_allowlist is \
                 NOT enforced — only ai-jail filters by host; the network is open"
            ),
            AgentConfinementLevel::Disabled => "agents: unconfined (sandbox disabled)".to_owned(),
            AgentConfinementLevel::Unavailable => {
                "agents: UNCONFINED — none of ai-jail, bubblewrap, sandbox-exec or firejail \
                 works here"
                    .to_owned()
            }
        }
    }

    /// The prefix that launches an agent working in `workspace`, or `None`
    /// when agents run unconfined.
    ///
    /// `keep_env` names variables the agent needs beyond the baseline — its
    /// command's own assignments, secrets of MCP servers it will launch.
    /// `readable` names paths its own command refers to, which must stay
    /// visible even where the profile would otherwise hide them.
    pub fn launch_prefix(
        &self,
        workspace: &Path,
        keep_env: &[String],
        readable: &[PathBuf],
    ) -> Option<Vec<String>> {
        let runtime = self.runtime?;
        let environment = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        let mut profile = Profile::for_workspace(self, workspace, keep_env, environment);
        profile.command_paths = readable.to_vec();
        Some(match runtime {
            AgentRuntime::AiJail => profile.ai_jail(&self.allowed_hosts),
            AgentRuntime::Bubblewrap => profile.bubblewrap(),
            AgentRuntime::SandboxExec => profile.sandbox_exec(),
            AgentRuntime::Firejail => profile.firejail(),
        })
    }
}

/// Everything that shapes one agent's confinement, whatever the runtime.
#[derive(Debug, Clone)]
struct Profile {
    workspace: PathBuf,
    /// A worktree's repository git directory, which git needs to write.
    git_common_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    /// Existing home entries, writable.
    home_writable: Vec<PathBuf>,
    /// Existing home entries, read-only.
    home_readable: Vec<PathBuf>,
    /// Existing home entries that must stay hidden.
    home_hidden: Vec<PathBuf>,
    /// Configured extra writable paths that exist.
    extra_writable: Vec<PathBuf>,
    /// CUMA's own executable, when the profile would otherwise hide it —
    /// agents launch `cuma mcp proxy` for shared MCP servers.
    cuma_executable: Option<PathBuf>,
    /// Paths the agent's command names, as written. Read-only.
    command_paths: Vec<PathBuf>,
    network: bool,
    /// Variables to remove, by name. Values never appear on a command line.
    strip_env: Vec<String>,
    /// Variables to forward explicitly (ai-jail, which starts from nothing).
    forward_env: Vec<String>,
}

impl Profile {
    /// `environment` is the names of the variables the agent would inherit.
    fn for_workspace(
        sandbox: &AgentSandbox,
        workspace: &Path,
        keep_env: &[String],
        environment: Vec<String>,
    ) -> Self {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let workspace = canonical(workspace);
        let home = dirs::home_dir().map(|h| canonical(&h));

        let mut home_writable = Vec::new();
        let mut home_readable = Vec::new();
        let mut home_hidden = Vec::new();
        if let Some(home) = &home
            && let Ok(entries) = std::fs::read_dir(home)
        {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with('.'))
                .collect();
            names.sort();
            for name in names {
                let path = home.join(&name);
                if HIDDEN_IN_HOME.contains(&name.as_str()) {
                    home_hidden.push(path);
                } else if WRITABLE_IN_HOME.contains(&name.as_str()) {
                    home_writable.push(path);
                } else if !is_special_file(&path) {
                    home_readable.push(path);
                }
            }
        }

        let extra_writable = sandbox
            .writable
            .iter()
            .filter(|p| p.exists())
            .map(|p| canonical(p))
            .collect();

        let cuma_executable = std::env::current_exe()
            .ok()
            .map(|p| canonical(&p))
            .filter(|exe| {
                // Hidden only when it lives under $HOME outside a dotdir.
                home.as_ref().is_some_and(|home| {
                    exe.strip_prefix(home).is_ok_and(|rest| {
                        !rest
                            .components()
                            .next()
                            .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with('.'))
                    })
                }) && !exe.starts_with(&workspace)
            });

        let mut keep: Vec<String> = sandbox.forwarded_env.clone();
        keep.extend(keep_env.iter().cloned());
        let strip_env = environment
            .into_iter()
            .filter(|name| !is_kept(name, &keep))
            .collect();

        Self {
            git_common_dir: git_common_dir(&workspace),
            workspace,
            home,
            home_writable,
            home_readable,
            home_hidden,
            extra_writable,
            cuma_executable,
            command_paths: Vec::new(),
            // An agent cut off from its model API cannot work, so the network
            // stays; ai-jail narrows it to the allowlist.
            network: true,
            strip_env,
            forward_env: keep,
        }
    }

    /// Paths an agent writes, besides the temporary directory.
    fn writable(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.workspace)
            .chain(self.git_common_dir.iter())
            .chain(self.home_writable.iter())
            .chain(self.extra_writable.iter())
    }

    fn ai_jail(&self, allowed_hosts: &[String]) -> Vec<String> {
        let mut prefix = vec![
            "ai-jail".to_owned(),
            "--exec".to_owned(),
            "--agent-state".to_owned(),
        ];
        if allowed_hosts.is_empty() {
            prefix.push("--network".to_owned());
        } else {
            for host in allowed_hosts {
                prefix.extend(["--allow-host".to_owned(), host.clone()]);
            }
        }
        for name in &self.forward_env {
            prefix.extend(["--env".to_owned(), name.clone()]);
        }
        // ai-jail makes its own working directory writable; the agent is
        // started from CUMA's, so the workspace is mapped explicitly, as is a
        // worktree's repository and anything configured.
        let here = std::env::current_dir().ok();
        for path in std::iter::once(&self.workspace)
            .chain(self.git_common_dir.iter())
            .chain(self.extra_writable.iter())
        {
            if here.as_deref() != Some(path.as_path()) {
                prefix.extend(["--rw-map".to_owned(), path.display().to_string()]);
            }
        }
        for path in &self.command_paths {
            prefix.extend(["--map".to_owned(), path.display().to_string()]);
        }
        prefix.push("--".to_owned());
        prefix
    }

    fn bubblewrap(&self) -> Vec<String> {
        let mut args: Vec<String> = ["bwrap", "--die-with-parent", "--new-session"]
            .map(str::to_owned)
            .to_vec();
        for flag in ["--unshare-pid", "--unshare-uts", "--unshare-ipc"] {
            args.push(flag.to_owned());
        }
        if !self.network {
            args.push("--unshare-net".to_owned());
        }
        let mut push = |parts: &[&str]| args.extend(parts.iter().map(|s| (*s).to_owned()));

        // The system, read-only. Merged-/usr distributions have /bin, /lib…
        // as symlinks into /usr; others have real directories.
        push(&["--ro-bind", "/usr", "/usr"]);
        for (dir, target) in [
            ("/bin", "usr/bin"),
            ("/sbin", "usr/sbin"),
            ("/lib", "usr/lib"),
            ("/lib32", "usr/lib32"),
            ("/lib64", "usr/lib64"),
        ] {
            let path = Path::new(dir);
            if path.is_symlink() {
                push(&["--symlink", target, dir]);
            } else if path.is_dir() {
                push(&["--ro-bind", dir, dir]);
            }
        }
        for dir in ["/etc", "/opt", "/nix", "/sys"] {
            push(&["--ro-bind-try", dir, dir]);
        }
        push(&[
            "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", "/run",
        ]);

        // /etc/resolv.conf often points into /run, which is now private.
        if let Ok(resolv) = std::fs::canonicalize("/etc/resolv.conf")
            && (resolv.starts_with("/run") || resolv.starts_with("/tmp"))
        {
            let resolv = resolv.display().to_string();
            push(&["--ro-bind", &resolv, &resolv]);
        }

        // A private home, with only what an agent needs put back.
        if let Some(home) = &self.home
            && home != Path::new("/")
        {
            let home = home.display().to_string();
            push(&["--tmpfs", &home]);
        }
        for path in &self.home_readable {
            let path = path.display().to_string();
            push(&["--ro-bind-try", &path, &path]);
        }
        for path in self
            .home_writable
            .iter()
            .chain(self.extra_writable.iter())
            .chain(self.git_common_dir.iter())
        {
            let path = path.display().to_string();
            push(&["--bind-try", &path, &path]);
        }
        if let Some(exe) = &self.cuma_executable {
            let exe = exe.display().to_string();
            push(&["--ro-bind-try", &exe, &exe]);
        }
        for path in self.command_paths.iter().filter(|p| !self.bwrap_shows(p)) {
            // Bound where the command names it, from wherever it really is,
            // so a symlink in a hidden directory still resolves.
            let source = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let (source, target) = (source.display().to_string(), path.display().to_string());
            push(&["--ro-bind-try", &source, &target]);
        }

        // The workspace last, so nothing above can shadow it…
        let workspace = self.workspace.display().to_string();
        push(&["--bind", &workspace, &workspace]);
        // …then hide credentials again if the workspace contains $HOME.
        for hidden in &self.home_hidden {
            if hidden.starts_with(&self.workspace) {
                let path = hidden.display().to_string();
                if hidden.is_dir() {
                    push(&["--tmpfs", &path]);
                } else {
                    push(&["--ro-bind", "/dev/null", &path]);
                }
            }
        }
        push(&["--chdir", &workspace]);

        for name in &self.strip_env {
            push(&["--unsetenv", name]);
        }
        push(&["--"]);
        args
    }

    /// Whether bubblewrap already shows `path`, through a mount made above.
    /// Binding it again would fail inside a read-only mount.
    fn bwrap_shows(&self, path: &Path) -> bool {
        const SYSTEM: [&str; 10] = [
            "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/nix", "/sys",
        ];
        SYSTEM.iter().any(|root| path.starts_with(root))
            || self
                .home_readable
                .iter()
                .chain(self.writable())
                .any(|root| path.starts_with(root))
    }

    fn firejail(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "firejail",
            "--quiet",
            "--noprofile",
            "--nonewprivs",
            "--caps.drop=all",
            "--read-only=/",
        ]
        .map(str::to_owned)
        .to_vec();
        if !self.network {
            args.push("--net=none".to_owned());
        }
        // Temporary directories stay shared and writable: worktrees live
        // there, and firejail cannot map one into a private /tmp.
        for tmp in ["/tmp", "/var/tmp", "/dev/shm"] {
            if Path::new(tmp).exists() {
                args.push(format!("--read-write={tmp}"));
            }
        }
        for path in self.writable() {
            if path.exists() {
                args.push(format!("--read-write={}", path.display()));
            }
        }
        for hidden in &self.home_hidden {
            args.push(format!("--blacklist={}", hidden.display()));
        }
        for name in &self.strip_env {
            args.push(format!("--rmenv={name}"));
        }
        args.push("--".to_owned());
        args
    }

    fn sandbox_exec(&self) -> Vec<String> {
        vec![
            "sandbox-exec".to_owned(),
            "-p".to_owned(),
            self.seatbelt_profile(),
            "/usr/bin/env".to_owned(),
        ]
        .into_iter()
        .chain(
            self.strip_env
                .iter()
                .flat_map(|name| ["-u".to_owned(), name.clone()]),
        )
        .collect()
    }

    /// The Seatbelt profile. In SBPL the last matching rule wins, so the
    /// order is: allow by default, deny writes, allow the writable places,
    /// then deny the hidden ones outright.
    fn seatbelt_profile(&self) -> String {
        let quote = |p: &Path| {
            format!(
                "\"{}\"",
                p.display()
                    .to_string()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            )
        };
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");

        profile.push_str("(allow file-write*\n");
        for path in self.writable() {
            let kind = if path.is_file() { "literal" } else { "subpath" };
            profile.push_str(&format!("  ({kind} {})\n", quote(path)));
        }
        for tmp in ["/private/tmp", "/private/var/folders", "/dev/fd"] {
            profile.push_str(&format!("  (subpath \"{tmp}\")\n"));
        }
        for device in [
            "/dev/null",
            "/dev/zero",
            "/dev/tty",
            "/dev/ptmx",
            "/dev/dtracehelper",
        ] {
            profile.push_str(&format!("  (literal \"{device}\")\n"));
        }
        profile.push_str("  (regex #\"^/dev/ttys[0-9]+$\"))\n");

        if !self.home_hidden.is_empty() {
            profile.push_str("(deny file-read* file-write*\n");
            for hidden in &self.home_hidden {
                profile.push_str(&format!("  (subpath {})\n", quote(hidden)));
            }
            profile.push_str(")\n");
        }
        if !self.network {
            profile.push_str("(deny network*)\n");
        }
        profile
    }
}

/// Sockets, pipes and devices in `$HOME` are not bound into a sandbox.
fn is_special_file(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        let kind = m.file_type();
        kind.is_socket() || kind.is_fifo() || kind.is_block_device() || kind.is_char_device()
    })
}

fn is_kept(name: &str, extra: &[String]) -> bool {
    KEPT_ENV.contains(&name)
        || KEPT_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
        || is_network_setup(name)
        || extra.iter().any(|e| e == name)
}

/// Proxy and certificate-authority settings, whatever tool they are for
/// (`GIT_SSL_CAINFO`, `PIP_CERT`, `npm_config_https_proxy`, …).
///
/// They hold addresses and file paths, and an agent behind a proxy cannot
/// reach its model API without them.
fn is_network_setup(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("PROXY")
        || [
            "CA_BUNDLE",
            "CAINFO",
            "CACERT",
            "CA_CERTS",
            "CA_STORE",
            "SSL_CERT",
            "SSL_ROOTS",
        ]
        .iter()
        .any(|marker| upper.contains(marker))
        || upper.ends_with("_CERT")
        || ["NODE_OPTIONS", "JAVA_TOOL_OPTIONS", "LD_LIBRARY_PATH"].contains(&name)
}

/// The repository git directory of a worktree, which lives outside it.
///
/// A worktree's `.git` is a file, `gitdir: <repo>/.git/worktrees/<name>`;
/// git inside the worktree writes to that repository directory, so it must
/// be writable too.
fn git_common_dir(workspace: &Path) -> Option<PathBuf> {
    let marker = workspace.join(".git");
    if !marker.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&marker).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        workspace.join(gitdir)
    };
    // <repo>/.git/worktrees/<name> → <repo>/.git
    let common = match gitdir.parent() {
        Some(parent) if parent.file_name().is_some_and(|n| n == "worktrees") => {
            parent.parent()?.to_path_buf()
        }
        _ => gitdir,
    };
    std::fs::canonicalize(&common).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sandbox(runtime: AgentRuntime, config: SecurityConfig) -> AgentSandbox {
        AgentSandbox::with_runtime(&config, Some(runtime))
    }

    fn prefix(runtime: AgentRuntime, workspace: &Path) -> Vec<String> {
        sandbox(runtime, SecurityConfig::default())
            .launch_prefix(workspace, &[], &[])
            .unwrap()
    }

    fn pairs(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    #[test]
    fn bubblewrap_makes_the_workspace_writable_and_the_system_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(dir.path()).unwrap();
        let args = prefix(AgentRuntime::Bubblewrap, &workspace);

        assert_eq!(args[0], "bwrap");
        assert_eq!(args.last().map(String::as_str), Some("--"));
        assert!(args.windows(3).any(|w| w[0] == "--bind"
            && w[1] == workspace.display().to_string()
            && w[2] == workspace.display().to_string()));
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/usr", "/usr"]));
        assert!(args.contains(&"--die-with-parent".to_owned()));
        assert!(
            !args.contains(&"--unshare-net".to_owned()),
            "agents need their model API"
        );
        assert_eq!(
            pairs(&args, "--chdir"),
            vec![workspace.display().to_string()]
        );
    }

    #[test]
    fn credentials_in_home_are_never_bound_and_agent_state_is_writable() {
        let Some(home) = dirs::home_dir() else { return };
        let args = prefix(AgentRuntime::Bubblewrap, Path::new("/"));
        let bound: Vec<String> = pairs(&args, "--ro-bind-try")
            .into_iter()
            .chain(pairs(&args, "--bind-try"))
            .collect();

        for hidden in HIDDEN_IN_HOME {
            let path = home.join(hidden).display().to_string();
            assert!(!bound.contains(&path), "{hidden} must stay hidden");
        }
        for writable in pairs(&args, "--bind-try") {
            let name = Path::new(&writable)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if Path::new(&writable).starts_with(&home) {
                assert!(
                    WRITABLE_IN_HOME.contains(&name.as_str()),
                    "{writable} writable"
                );
            }
        }
    }

    #[test]
    fn only_the_baseline_and_named_variables_are_kept() {
        let config = SecurityConfig {
            agent_env: vec!["FORWARDED".into()],
            ..SecurityConfig::default()
        };
        let environment = [
            "PATH",
            "HOME",
            "LC_ALL",
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "CUMA_ARCHITECT_TOKEN",
            "GIT_SSL_CAINFO",
            "npm_config_https_proxy",
            "PIP_CERT",
            "FORWARDED",
            "AGENT_OWN",
        ]
        .map(str::to_owned)
        .to_vec();
        let profile = Profile::for_workspace(
            &sandbox(AgentRuntime::Bubblewrap, config),
            Path::new("/tmp"),
            &["AGENT_OWN".into()],
            environment,
        );
        assert_eq!(
            profile.strip_env,
            vec!["GITHUB_TOKEN", "CUMA_ARCHITECT_TOKEN"]
        );

        // Names only: a value is never on a command line, where any user on
        // the machine could read it.
        for args in [
            profile.bubblewrap(),
            profile.firejail(),
            profile.sandbox_exec(),
        ] {
            let joined = args.join(" ");
            assert!(joined.contains("GITHUB_TOKEN"));
            assert!(!joined.contains("FORWARDED"));
        }
    }

    #[test]
    fn paths_the_agents_command_names_stay_readable() {
        let script = tempfile::NamedTempFile::new().unwrap();
        let named = script.path().to_path_buf();
        let config = SecurityConfig::default();
        for (runtime, flag) in [
            (AgentRuntime::Bubblewrap, "--ro-bind-try"),
            (AgentRuntime::AiJail, "--map"),
        ] {
            let args = sandbox(runtime, config.clone())
                .launch_prefix(
                    Path::new("/cuma-test-elsewhere"),
                    &[],
                    std::slice::from_ref(&named),
                )
                .unwrap();
            let shown = args
                .windows(2)
                .any(|w| w[0] == flag && args.iter().any(|a| a == &named.display().to_string()));
            assert!(shown, "{runtime:?} hid the agent's own script: {args:?}");
        }
    }

    #[test]
    fn a_path_already_visible_is_not_bound_twice() {
        let args = sandbox(AgentRuntime::Bubblewrap, SecurityConfig::default())
            .launch_prefix(Path::new("/tmp"), &[], &[PathBuf::from("/usr/bin/env")])
            .unwrap();
        assert!(
            !args.iter().any(|a| a == "/usr/bin/env"),
            "binding inside the read-only /usr fails: {args:?}"
        );
    }

    #[test]
    fn a_worktree_gets_its_repository_git_directory_writable() {
        let repo = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let git = repo.path().join(".git/worktrees/task-1");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", git.display()),
        )
        .unwrap();

        let common = std::fs::canonicalize(repo.path().join(".git")).unwrap();
        assert_eq!(git_common_dir(worktree.path()), Some(common.clone()));

        let args = prefix(AgentRuntime::Bubblewrap, worktree.path());
        assert!(pairs(&args, "--bind-try").contains(&common.display().to_string()));
        let jail = prefix(AgentRuntime::AiJail, worktree.path());
        assert!(pairs(&jail, "--rw-map").contains(&common.display().to_string()));
    }

    #[test]
    fn firejail_writes_only_where_bubblewrap_would() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(dir.path()).unwrap();
        let args = prefix(AgentRuntime::Firejail, &workspace);
        assert!(args.contains(&"--read-only=/".to_owned()));
        assert!(args.contains(&format!("--read-write={}", workspace.display())));
        assert!(
            !args.iter().any(|a| a.starts_with("--net=")),
            "network stays for the model API"
        );
    }

    #[test]
    fn the_seatbelt_profile_denies_writes_except_where_agents_work_and_hides_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile {
            workspace: dir.path().to_path_buf(),
            git_common_dir: None,
            home: Some(PathBuf::from("/Users/me")),
            home_writable: vec![PathBuf::from("/Users/me/.claude")],
            home_readable: Vec::new(),
            home_hidden: vec![PathBuf::from("/Users/me/.ssh")],
            extra_writable: Vec::new(),
            cuma_executable: None,
            command_paths: Vec::new(),
            network: true,
            strip_env: vec!["SECRET".into()],
            forward_env: Vec::new(),
        };
        let text = profile.seatbelt_profile();
        let deny_writes = text.find("(deny file-write*)").unwrap();
        let allow = text.find("(allow file-write*").unwrap();
        let hide = text.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_writes < allow && allow < hide,
            "last rule wins:\n{text}"
        );
        assert!(text.contains(&format!("(subpath \"{}\")", dir.path().display())));
        assert!(text.contains("(subpath \"/Users/me/.claude\")"));
        assert!(text.contains("(subpath \"/Users/me/.ssh\")"));
        assert!(!text.contains("deny network"));

        let args = profile.sandbox_exec();
        assert_eq!(&args[..2], ["sandbox-exec", "-p"]);
        assert_eq!(&args[3..], ["/usr/bin/env", "-u", "SECRET"]);
    }

    #[test]
    fn an_allowlist_only_ai_jail_can_enforce_is_reported_not_ignored() {
        let config = SecurityConfig {
            network_allowlist: vec!["api.anthropic.com".into()],
            require_agent_sandbox: true,
            ..SecurityConfig::default()
        };
        let bwrap = sandbox(AgentRuntime::Bubblewrap, config.clone());
        assert!(matches!(
            bwrap.level(),
            AgentConfinementLevel::NetworkUnfiltered { .. }
        ));
        assert!(bwrap.describe().contains("NOT enforced"));
        assert!(bwrap.refuses_agents(), "required confinement falls short");

        let jail = sandbox(AgentRuntime::AiJail, config);
        assert!(matches!(
            jail.level(),
            AgentConfinementLevel::Confined { .. }
        ));
        assert!(!jail.refuses_agents());
    }

    #[test]
    fn nothing_available_is_unconfined_and_refused_only_when_required() {
        let open = AgentSandbox::with_runtime(&SecurityConfig::default(), None);
        assert_eq!(open.level(), AgentConfinementLevel::Unavailable);
        assert!(open.describe().contains("UNCONFINED"));
        assert!(!open.refuses_agents());
        assert!(open.launch_prefix(Path::new("/w"), &[], &[]).is_none());

        let strict = AgentSandbox::with_runtime(
            &SecurityConfig {
                require_agent_sandbox: true,
                ..SecurityConfig::default()
            },
            None,
        );
        assert!(strict.refuses_agents());

        let off = AgentSandbox::with_runtime(
            &SecurityConfig {
                sandbox: false,
                require_agent_sandbox: true,
                ..SecurityConfig::default()
            },
            Some(AgentRuntime::Bubblewrap),
        );
        assert_eq!(off.level(), AgentConfinementLevel::Disabled);
        assert!(
            !off.refuses_agents(),
            "turning the sandbox off is an explicit choice"
        );
    }
}
