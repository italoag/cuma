//! `cuma setup`: detect the agents this machine can run and write a
//! configuration, asking only what cannot be defaulted.
//!
//! Detection is `which`-level, never launch-level. Three sources are probed:
//! a table of known agents and their ACP flags, the ACP registry (which knows
//! the executable every published agent answers to — this is what finds a
//! locally installed `devin` or `junie`), and a generic `*-acp` scan of `PATH`
//! for adapters nothing else catalogues. What cannot be detected — which
//! agents to enable, strategy, budget, where the file lives — the wizard
//! asks, with a default for every question so an operator who does not know
//! can press Enter throughout.

use cuma_config::{Config, RoutingStrategy};
use cuma_core::error::{MetaAgentError, Result};
use cuma_protocol_acp::registry::{self, Launch};
use dialoguer::{Confirm, Input, MultiSelect, Select, theme::ColorfulTheme};
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// How an installed agent is launched into an ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeLaunch {
    /// A binary that speaks ACP itself (`gemini --acp`, `opencode acp`), or a
    /// locally installed adapter (`codex-acp`). Written as `command`.
    Direct(&'static str),
    /// The well-known adapter resolved through `npx` — needs `npx` on PATH.
    /// No `command` is written; the resolver fills it in.
    WellKnown,
}

/// A coding agent setup knows how to configure when its binary turns up.
struct Probe {
    /// The `[agents.<id>]` name.
    id: &'static str,
    /// What it is, for the report.
    name: &'static str,
    /// `(binary, launch)` pairs tried in order; the first on `PATH` wins.
    candidates: &'static [(&'static str, ProbeLaunch)],
    /// Directories it keeps its login and sessions in — written to `state`
    /// only when they exist, so the file reflects this machine.
    state: &'static [&'static str],
}

/// Agent ids whose state directories the `~/.<id>` conventions miss — e.g.
/// the `hermes-acp` adapter re-execs from `~/.hermes`.
const EXTRA_STATE: &[(&str, &[&str])] = &[
    ("hermes-acp", &["~/.hermes"]),
    ("hermes", &["~/.hermes"]),
    ("copilot", &["~/.config/github-copilot"]),
    ("github-copilot-cli", &["~/.config/github-copilot"]),
];

/// Directories a detected agent plausibly needs writable inside a confined
/// launch, filtered to those that exist. `~/.config` and `~/.cache` are
/// already writable toolchain-wide; the value here is `.local` state and
/// the agent's own dotdir — a directory it needs that is missing from this
/// list fails the agent on startup, which is what `cuma doctor` reports.
fn inferred_state(id: &str) -> Vec<String> {
    let mut dirs: Vec<String> = [
        format!("~/.{id}"),
        format!("~/.config/{id}"),
        format!("~/.local/share/{id}"),
        format!("~/.local/state/{id}"),
        format!("~/.cache/{id}"),
    ]
    .into_iter()
    .chain(
        EXTRA_STATE
            .iter()
            .find(|(needle, _)| *needle == id)
            .into_iter()
            .flat_map(|(_, dirs)| dirs.iter().map(|d| (*d).to_owned())),
    )
    .filter(|path| cuma_config::expand_home(path).exists())
    .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Agents that speak ACP natively, plus the published adapters for the two
/// well-known names. The registry and the `*-acp` PATH scan cover everything
/// this table does not.
const PROBES: &[Probe] = &[
    Probe {
        id: "claude-code",
        name: "Claude Code",
        candidates: &[
            ("claude-agent-acp", ProbeLaunch::Direct("claude-agent-acp")),
            ("claude", ProbeLaunch::WellKnown),
        ],
        state: &["~/.claude", "~/.claude.json"],
    },
    Probe {
        id: "codex",
        name: "OpenAI Codex",
        candidates: &[
            ("codex-acp", ProbeLaunch::Direct("codex-acp")),
            ("codex", ProbeLaunch::WellKnown),
        ],
        state: &["~/.codex"],
    },
    Probe {
        id: "gemini",
        name: "Gemini CLI",
        candidates: &[("gemini", ProbeLaunch::Direct("gemini --acp"))],
        state: &["~/.gemini"],
    },
    Probe {
        id: "copilot",
        name: "GitHub Copilot CLI",
        candidates: &[("copilot", ProbeLaunch::Direct("copilot --acp --stdio"))],
        state: &["~/.copilot"],
    },
    Probe {
        id: "kimi",
        name: "Kimi Code CLI",
        candidates: &[("kimi", ProbeLaunch::Direct("kimi acp"))],
        state: &["~/.kimi-code"],
    },
    Probe {
        id: "opencode",
        name: "OpenCode",
        candidates: &[("opencode", ProbeLaunch::Direct("opencode acp"))],
        state: &["~/.config/opencode", "~/.local/share/opencode"],
    },
    Probe {
        id: "goose",
        name: "Goose",
        candidates: &[("goose", ProbeLaunch::Direct("goose acp"))],
        state: &["~/.config/goose"],
    },
    Probe {
        id: "auggie",
        name: "Augment (auggie)",
        candidates: &[("auggie", ProbeLaunch::Direct("auggie --acp"))],
        state: &[],
    },
    Probe {
        id: "cline",
        name: "Cline CLI",
        candidates: &[("cline", ProbeLaunch::Direct("cline --acp"))],
        state: &[],
    },
    Probe {
        id: "cursor",
        name: "Cursor Agent CLI",
        candidates: &[("cursor-agent", ProbeLaunch::Direct("cursor-agent acp"))],
        state: &[],
    },
    Probe {
        id: "kilo",
        name: "Kilo Code",
        candidates: &[("kilo", ProbeLaunch::Direct("kilo acp"))],
        state: &[],
    },
    Probe {
        id: "kiro",
        name: "Kiro CLI",
        candidates: &[("kiro-cli", ProbeLaunch::Direct("kiro-cli acp"))],
        state: &["~/.kiro"],
    },
    Probe {
        id: "openhands",
        name: "OpenHands",
        candidates: &[("openhands", ProbeLaunch::Direct("openhands acp"))],
        state: &[],
    },
    Probe {
        id: "openclaw",
        name: "OpenClaw",
        candidates: &[("openclaw", ProbeLaunch::Direct("openclaw acp"))],
        state: &[],
    },
    Probe {
        id: "stakpak",
        name: "Stakpak",
        candidates: &[("stakpak", ProbeLaunch::Direct("stakpak acp"))],
        state: &[],
    },
    Probe {
        id: "mcode",
        name: "MiniMax Code",
        candidates: &[("mcode", ProbeLaunch::Direct("mcode acp"))],
        state: &[],
    },
    Probe {
        id: "vibe",
        name: "Mistral Vibe",
        candidates: &[("vibe-acp", ProbeLaunch::Direct("vibe-acp"))],
        state: &[],
    },
    Probe {
        id: "pi",
        name: "Pi",
        candidates: &[("pi-acp", ProbeLaunch::Direct("pi-acp"))],
        state: &[],
    },
    Probe {
        id: "fast-agent",
        name: "fast-agent",
        candidates: &[("fast-agent-acp", ProbeLaunch::Direct("fast-agent-acp"))],
        state: &[],
    },
];

/// A coding agent found on this machine, by any of the probes.
struct Detected {
    id: String,
    name: String,
    /// Written to `command`; `None` resolves through the well-known adapter.
    command: Option<String>,
    /// State directories that exist, in `~/…` spelling.
    state: Vec<String>,
    /// Where the executable resolved, for the report and the picker.
    location: Option<PathBuf>,
    /// How it was found: `probe`, `registry`, `PATH scan`.
    found: &'static str,
    /// Why it cannot be used, when it cannot.
    unusable: Option<String>,
}

/// A registry agent not installed here, offered for opt-in in the wizard.
struct RegistryOffer {
    id: String,
    name: String,
    /// The `command` written when selected; `None` when nothing on this
    /// platform can run it — the row is still listed, per the registry.
    command: Option<String>,
    /// Right-hand annotation in the picker (`npx · installs on first use`).
    note: String,
    /// Extra line shown in the result when the row is selected.
    heads_up: Option<String>,
}

/// Everything setup learned about the machine, before any choice.
struct Detection {
    agents: Vec<Detected>,
    offers: Vec<RegistryOffer>,
    /// One line about the registry itself ("45 agents catalogued").
    registry_note: String,
    npx: bool,
    uvx: bool,
    ai_memory: bool,
    rtk: bool,
    sandbox: String,
}

/// A resolved executable path already claimed by a detected agent, with the
/// id that claimed it. Two registry entries can name the same binary
/// (`opencode` and `opencode-v2` both ship an `opencode` executable); only
/// the first becomes a detection, the rest stay selectable as offers.
type Claimed = Vec<(PathBuf, String)>;

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn detect(config: &Config, workspace: &Path) -> Detection {
    let npx = which::which("npx").is_ok();
    let mut claimed: Claimed = Vec::new();
    let mut agents = probe_table(npx, &mut claimed);

    // The registry serves double duty: it names the executable every
    // published agent answers to — which is how an installed `devin` or
    // `junie` is found — and it is the catalogue the wizard offers the rest
    // from. A network failure costs the second part, never the run.
    let cache = crate::commands::registry_cache(workspace);
    let mut offers = Vec::new();
    let registry_note = match registry::fetch_registry(registry::REGISTRY_URL, &cache).await {
        Ok(fetched) => {
            let total = fetched.registry.agents.len();
            offers = probe_registry(&fetched.registry, config, &mut agents, &mut claimed);
            match fetched.stale {
                Some(err) => {
                    format!("{total} agents catalogued (cached copy — {err})")
                }
                None => format!("{total} agents catalogued"),
            }
        }
        Err(_) => "unavailable — PATH probes only".to_owned(),
    };

    scan_path(&mut agents, &mut claimed);

    Detection {
        agents,
        offers,
        registry_note,
        npx,
        uvx: which::which("uvx").is_ok(),
        ai_memory: which::which("ai-memory").is_ok(),
        rtk: which::which("rtk").is_ok(),
        sandbox: cuma_workspace::AgentSandbox::detect(&config.security).describe(),
    }
}

/// The known-agent table — the only probe that knows ACP flags for binaries
/// the registry does not document, and the well-known `npx` adapters.
fn probe_table(npx: bool, claimed: &mut Claimed) -> Vec<Detected> {
    let mut agents = Vec::new();
    for probe in PROBES {
        let Some((binary, launch)) = probe
            .candidates
            .iter()
            .find_map(|(binary, launch)| which::which(binary).ok().map(|path| (path, launch)))
        else {
            continue;
        };
        claimed.push((canonical(&binary), probe.id.to_owned()));

        let (command, unusable) = match launch {
            ProbeLaunch::Direct(command) => (Some((*command).to_owned()), None),
            ProbeLaunch::WellKnown => (None, (!npx).then(|| "needs npx on PATH".to_owned())),
        };

        let mut state: Vec<String> = probe
            .state
            .iter()
            .filter(|path| cuma_config::expand_home(path).exists())
            .map(|path| (*path).to_owned())
            .collect();
        state.extend(inferred_state(probe.id));
        state.sort();
        state.dedup();

        agents.push(Detected {
            id: probe.id.to_owned(),
            name: probe.name.to_owned(),
            command,
            state,
            location: Some(binary),
            found: "probe",
            unusable,
        });
    }
    agents
}

/// Registry agents: the ones already installed become detections, the rest
/// become offers — every one of them, launchable here or not.
fn probe_registry(
    registry: &registry::AgentRegistry,
    config: &Config,
    detected: &mut Vec<Detected>,
    claimed: &mut Claimed,
) -> Vec<RegistryOffer> {
    let mut offers = Vec::new();
    for agent in &registry.agents {
        if config.agents.contains_key(&agent.id) || detected.iter().any(|a| a.id == agent.id) {
            continue;
        }

        // Executables this agent answers to when installed: the binary's own
        // name for archive distributions, the package's bin for npx/uvx.
        let mut candidates: Vec<String> = Vec::new();
        let platform = registry::current_platform();
        if let Some(binary) = agent.distribution.binary.get(&platform) {
            candidates.push(binary_basename(&binary.cmd).to_owned());
            if binary_basename(&binary.cmd) != agent.id {
                candidates.push(agent.id.clone());
                candidates.push(format!("{}-cli", agent.id));
            }
        } else if let Some(package) = agent
            .distribution
            .npx
            .as_ref()
            .or(agent.distribution.uvx.as_ref())
        {
            candidates.push(package_bin_name(&package.package).to_owned());
        }
        candidates.sort();
        candidates.dedup();

        if let Some((path, executable)) = candidates
            .iter()
            .find_map(|c| which::which(c).ok().map(|p| (p, c.clone())))
        {
            // Installed. Binary entries ship their ACP args in the registry;
            // a package's args are its program's args.
            let args: &[String] = agent
                .distribution
                .binary
                .get(&platform)
                .map(|b| b.args.as_slice())
                .or_else(|| {
                    agent
                        .distribution
                        .npx
                        .as_ref()
                        .or(agent.distribution.uvx.as_ref())
                        .map(|p| p.args.as_slice())
                })
                .unwrap_or(&[]);
            let mut words = vec![executable];
            words.extend(args.iter().cloned());
            let command = shell_words::join(&words);

            let canon = canonical(&path);
            match claimed.iter().find(|(p, _)| *p == canon) {
                // The same executable is already an agent under another id —
                // detected once, but still selectable under this one.
                Some((_, winner)) => {
                    let winner = winner.clone();
                    offers.push(RegistryOffer {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                        command: Some(command),
                        note: format!("installed · same binary as {winner}"),
                        heads_up: None,
                    });
                }
                None => {
                    claimed.push((canon, agent.id.clone()));
                    detected.push(Detected {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                        command: Some(command),
                        state: inferred_state(&agent.id),
                        location: Some(path),
                        found: "registry",
                        unusable: None,
                    });
                }
            }
            continue;
        }

        offers.push(offer(agent, &platform));
    }
    offers
}

/// The offer for a registry agent that is not installed: the command it
/// would run as, plus the annotation the picker shows.
fn offer(agent: &registry::RegistryAgent, platform: &str) -> RegistryOffer {
    let (command, note, heads_up) = match agent.launch(false) {
        Launch::Command { command, launcher } => {
            if which::which(&launcher).is_ok() {
                (
                    Some(command),
                    format!("{launcher} · installs on first use"),
                    None,
                )
            } else {
                (
                    Some(command),
                    format!("needs {launcher}, not on PATH"),
                    Some(format!(
                        "{}: `{launcher}` is not on PATH — the agent runs once it is",
                        agent.id
                    )),
                )
            }
        }
        Launch::Binary { .. } => {
            // `Launch::Binary` is only produced from a platform entry, so
            // this lookup always succeeds; treat a miss as Unsupported.
            let Some(binary) = agent.distribution.binary.get(platform) else {
                return RegistryOffer {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                    command: None,
                    note: format!("no build for {platform}"),
                    heads_up: None,
                };
            };
            let mut words = vec![binary_basename(&binary.cmd).to_owned()];
            words.extend(binary.args.iter().cloned());
            (
                Some(shell_words::join(&words)),
                "binary · needs manual install".to_owned(),
                Some(format!(
                    "{}: not installed — `cuma agents add {}` prints the download",
                    agent.id, agent.id
                )),
            )
        }
        Launch::Unsupported => (
            None,
            format!("no build for {platform}"),
            Some(format!(
                "{}: skipped — the registry publishes nothing for {platform}",
                agent.id
            )),
        ),
    };
    RegistryOffer {
        id: agent.id.clone(),
        name: agent.name.clone(),
        command,
        note,
        heads_up,
    }
}

/// `PATH` executables named like ACP adapters (`*-acp`, `acp-*`) that no
/// probe or registry entry claimed. The convention is stdio ACP on a bare
/// invocation.
fn scan_path(agents: &mut Vec<Detected>, claimed: &mut Claimed) {
    let mut found_bins = BTreeSet::new();
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if (name.ends_with("-acp") || name.starts_with("acp-")) && !name.contains('.') {
                found_bins.insert(name.to_owned());
            }
        }
    }

    for bin in found_bins {
        if agents.iter().any(|a| {
            a.id == bin
                || a.command
                    .as_deref()
                    .is_some_and(|c| c.split_whitespace().next() == Some(bin.as_str()))
        }) {
            continue;
        }
        let Ok(path) = which::which(&bin) else {
            continue;
        };
        let canon = canonical(&path);
        if claimed.iter().any(|(p, _)| *p == canon) {
            continue;
        }
        claimed.push((canon, bin.clone()));
        let state = inferred_state(&bin);
        agents.push(Detected {
            id: bin.clone(),
            name: "ACP adapter".to_owned(),
            command: Some(bin),
            state,
            location: Some(path),
            found: "PATH scan",
            unusable: None,
        });
    }
}

/// `./bin/devin` → `devin`; `.\bin\devin.exe` → `devin`.
fn binary_basename(cmd: &str) -> &str {
    let tail = cmd.rsplit(['/', '\\']).next().unwrap_or(cmd);
    tail.strip_suffix(".exe").unwrap_or(tail)
}

/// `@scope/name@1.2.3` → `name`; `pkg==0.3.1` → `pkg`.
fn package_bin_name(package: &str) -> &str {
    let tail = package.rsplit('/').next().unwrap_or(package);
    tail.split(['@', '=', '<', '>', '!', '~'])
        .next()
        .unwrap_or(tail)
}

/// What setup will write — the wizard's answers, or the defaults.
struct Plan {
    path: PathBuf,
    agents: Vec<PlannedAgent>,
    strategy: RoutingStrategy,
    sandbox: bool,
    max_cost_usd: Option<f64>,
    memory: bool,
    /// Heads-up lines for the result (picked agents needing manual installs).
    notes: Vec<String>,
}

struct PlannedAgent {
    id: String,
    command: Option<String>,
    state: Vec<String>,
}

/// What applying a plan did.
enum Outcome {
    Nothing,
    DryRun {
        path: PathBuf,
        text: String,
        manual: Vec<String>,
        notes: Vec<String>,
    },
    Written {
        path: PathBuf,
        merged: bool,
        manual: Vec<String>,
        notes: Vec<String>,
    },
}

impl Outcome {
    fn describe(&self) -> String {
        let notes = |out: &mut String, notes: &[String]| {
            if !notes.is_empty() {
                out.push_str("\nHeads up:");
                for note in notes {
                    out.push_str(&format!("\n  {note}"));
                }
            }
        };
        match self {
            Self::Nothing => "Configuration is already complete; nothing to do.".to_owned(),
            Self::DryRun {
                path,
                text,
                manual,
                notes: heads_up,
            } => {
                let mut out = format!(
                    "Dry run — nothing was written. {} would contain:\n\n{text}",
                    path.display()
                );
                if !manual.is_empty() {
                    out.push_str("\nCould not merge automatically; set these by hand:");
                    for item in manual {
                        out.push_str(&format!("\n  {item}"));
                    }
                }
                notes(&mut out, heads_up);
                out
            }
            Self::Written {
                path,
                merged,
                manual,
                notes: heads_up,
            } => {
                let mut out = format!(
                    "{} {}",
                    if *merged { "Updated" } else { "Wrote" },
                    path.display()
                );
                if !manual.is_empty() {
                    out.push_str("\nCould not merge automatically; set these by hand:");
                    for item in manual {
                        out.push_str(&format!("\n  {item}"));
                    }
                }
                notes(&mut out, heads_up);
                out.push_str("\n\nNext: cuma doctor && cuma explain \"add a health endpoint\"");
                out
            }
        }
    }
}

/// `cuma setup`.
pub async fn run(
    config: Config,
    workspace: PathBuf,
    yes: bool,
    project: bool,
    global: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let detection = detect(&config, &workspace).await;
    let interactive = !yes && !json && !dry_run && std::io::stdin().is_terminal();

    if !json {
        report_detection(&detection, &config);
    }

    let plan = if interactive {
        wizard(&config, &workspace, &detection, project, global)?
    } else {
        default_plan(&config, &workspace, &detection, project, global)?
    };

    let outcome = apply(&plan, dry_run)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "detected": {
                    "agents": detection.agents.iter().map(|a| serde_json::json!({
                        "id": a.id,
                        "command": a.command,
                        "path": a.location,
                        "found": a.found,
                        "unusable": a.unusable,
                    })).collect::<Vec<_>>(),
                    "registry": detection.registry_note,
                    "npx": detection.npx,
                    "uvx": detection.uvx,
                    "ai_memory": detection.ai_memory,
                    "rtk": detection.rtk,
                    "sandbox": detection.sandbox,
                },
                "outcome": outcome.describe(),
            }))
            .unwrap_or_default()
        );
    } else {
        println!("{}", outcome.describe());
    }

    Ok(())
}

fn report_detection(detection: &Detection, config: &Config) {
    println!("Detecting this machine:\n");

    if detection.agents.is_empty() {
        println!("  agents:   none found — no known ACP agent binary is on PATH");
    } else {
        for agent in &detection.agents {
            let location = agent
                .location
                .as_ref()
                .map(|p| format!(" · {}", shrink_home(p)))
                .unwrap_or_default();
            let state = if let Some(reason) = &agent.unusable {
                format!(" — unusable: {reason}")
            } else if config.agents.contains_key(&agent.id) {
                " — already configured".to_owned()
            } else {
                String::new()
            };
            println!(
                "  agent:    {} — {}{location}{state} [{}]",
                agent.id, agent.name, agent.found
            );
        }
    }

    println!("  registry: {}", detection.registry_note);
    println!(
        "  launch:   npx {} · uvx {}",
        if detection.npx { "found" } else { "missing" },
        if detection.uvx { "found" } else { "missing" },
    );
    println!("  {}", detection.sandbox);
    println!(
        "  memory:   ai-memory {}",
        if detection.ai_memory {
            "found"
        } else {
            "not found"
        }
    );
    println!(
        "  rtk:      {}",
        if detection.rtk { "found" } else { "not found" }
    );
    println!();
}

/// `/Users/alice/.local/bin/devin` → `~/.local/bin/devin`.
fn shrink_home(path: &Path) -> String {
    match dirs::home_dir() {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(path).display())
        }
        _ => path.display().to_string(),
    }
}

/// Every choice, defaulted: the non-interactive plan, and the baseline the
/// wizard starts from.
fn default_plan(
    config: &Config,
    workspace: &Path,
    detection: &Detection,
    project: bool,
    global: bool,
) -> Result<Plan> {
    let agents = detection
        .agents
        .iter()
        .filter(|a| a.unusable.is_none() && !config.agents.contains_key(&a.id))
        .map(|a| PlannedAgent {
            id: a.id.clone(),
            command: a.command.clone(),
            state: a.state.clone(),
        })
        .collect();

    Ok(Plan {
        path: target_path(workspace, project, global)?,
        agents,
        strategy: config.router.strategy,
        sandbox: config.security.sandbox,
        // What the wizard would answer: an existing cap is kept, an unset one
        // becomes 10.00 — unattended spend is the surprise defaults deny.
        max_cost_usd: Some(config.limits.max_cost_usd.unwrap_or(10.0)),
        memory: false,
        notes: Vec::new(),
    })
}

fn target_path(workspace: &Path, project: bool, global: bool) -> Result<PathBuf> {
    if project {
        return Ok(workspace.join(".cuma").join("config.toml"));
    }
    let _ = global; // the default already; the flag exists for explicitness
    cuma_config::global_config_path().ok_or_else(|| {
        MetaAgentError::Configuration(
            "cannot locate the user config directory; pass --project".to_owned(),
        )
    })
}

/// The wizard. Every question has a default; pressing Enter is a complete
/// valid configuration.
fn wizard(
    config: &Config,
    workspace: &Path,
    detection: &Detection,
    project: bool,
    global: bool,
) -> Result<Plan> {
    let theme = ColorfulTheme::default();
    let prompt = |err: dialoguer::Error| {
        MetaAgentError::Other(format!("the setup wizard was aborted: {err}"))
    };

    // --- where -------------------------------------------------------------
    let path = if project || global {
        target_path(workspace, project, global)?
    } else {
        let global_path = cuma_config::global_config_path();
        let project_path = workspace.join(".cuma").join("config.toml");
        let options = vec![
            format!(
                "{} (applies to every project)",
                global_path.as_ref().map_or_else(
                    || "~/.config/cuma/config.toml".to_owned(),
                    |p| p.display().to_string()
                )
            ),
            format!("{} (this project only)", project_path.display()),
        ];
        match Select::with_theme(&theme)
            .with_prompt("Where should the configuration live?")
            .items(&options)
            .default(0)
            .interact()
            .map_err(prompt)?
        {
            0 => global_path.ok_or_else(|| {
                MetaAgentError::Configuration("cannot locate the user config directory".to_owned())
            })?,
            _ => project_path,
        }
    };
    if path.exists() {
        println!(
            "  note: {} exists — new settings are merged in, nothing is removed",
            path.display()
        );
    }

    // --- agents ------------------------------------------------------------
    let mut agents = Vec::new();
    let found: Vec<&Detected> = detection
        .agents
        .iter()
        .filter(|a| !config.agents.contains_key(&a.id))
        .collect();
    if detection.agents.is_empty() {
        println!("No agents detected. Install one, or browse the ACP registry with:");
        println!("  cuma agents discover --registry && cuma agents add <id>");
    }
    if !found.is_empty() {
        let labels: Vec<String> = found
            .iter()
            .map(|a| {
                let location = a
                    .location
                    .as_ref()
                    .map(|p| format!(" · {}", shrink_home(p)))
                    .unwrap_or_default();
                let suffix = a
                    .unusable
                    .as_ref()
                    .map(|r| format!(" — unusable: {r}"))
                    .unwrap_or_default();
                format!("{} — {}{location}{suffix}", a.id, a.name)
            })
            .collect();
        let defaults: Vec<bool> = found.iter().map(|a| a.unusable.is_none()).collect();
        for index in MultiSelect::with_theme(&theme)
            .with_prompt("Agents found on this machine — select which to enable")
            .items(&labels)
            .defaults(&defaults)
            .interact()
            .map_err(prompt)?
        {
            let a = found[index];
            agents.push(PlannedAgent {
                id: a.id.clone(),
                command: a.command.clone(),
                state: a.state.clone(),
            });
        }
    }

    let mut notes = Vec::new();
    if !detection.offers.is_empty() {
        let labels: Vec<String> = detection
            .offers
            .iter()
            .map(|o| format!("{} — {} · {}", o.id, o.name, o.note))
            .collect();
        for index in MultiSelect::with_theme(&theme)
            .with_prompt("Add agents from the ACP registry")
            .items(&labels)
            .interact()
            .map_err(prompt)?
        {
            let offer = &detection.offers[index];
            if let Some(command) = &offer.command {
                agents.push(PlannedAgent {
                    id: offer.id.clone(),
                    command: Some(command.clone()),
                    state: Vec::new(),
                });
            }
            if let Some(heads_up) = &offer.heads_up {
                notes.push(heads_up.clone());
            }
        }
    }

    // --- routing -----------------------------------------------------------
    let strategies = [
        (
            RoutingStrategy::Balanced,
            "balanced — quality, cost and reliability weighed together",
        ),
        (
            RoutingStrategy::QualityFirst,
            "quality-first — the most capable agent, price aside",
        ),
        (
            RoutingStrategy::CostFirst,
            "cost-first — the cheapest agent that can do the job",
        ),
        (
            RoutingStrategy::LatencyFirst,
            "latency-first — the fastest agent",
        ),
        (
            RoutingStrategy::LocalFirst,
            "local-first — prefer agents running on this machine",
        ),
        (
            RoutingStrategy::PrivacyFirst,
            "privacy-first — prefer agents that keep data local",
        ),
        (
            RoutingStrategy::Manual,
            "manual — route only to a pinned agent",
        ),
    ];
    let options: Vec<String> = strategies
        .iter()
        .map(|(_, label)| (*label).to_owned())
        .collect();
    let strategy = strategies[Select::with_theme(&theme)
        .with_prompt("Routing strategy?")
        .items(&options)
        .default(0)
        .interact()
        .map_err(prompt)?]
    .0;

    // --- safety ------------------------------------------------------------
    let sandbox = Confirm::with_theme(&theme)
        .with_prompt("Run agents inside a sandbox when possible?")
        .default(true)
        .interact()
        .map_err(prompt)?;
    if sandbox && detection.sandbox.contains("UNCONFINED") {
        println!(
            "  note: no sandbox runtime works here — agents will run unconfined.\n  \
             Install ai-jail, or pick a provider under [sandboxes.*]; see docs/SANDBOXES.md"
        );
    }

    let answer = Input::<String>::with_theme(&theme)
        .with_prompt("Max spend per run in USD (\"none\" for no cap)")
        .default("10.00".to_owned())
        .interact_text()
        .map_err(prompt)?;
    let max_cost_usd = match answer.trim() {
        "" => Some(10.0),
        value if value.eq_ignore_ascii_case("none") => None,
        value => match value.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => Some(v),
            _ => {
                eprintln!("  {value:?} is not a positive number — keeping the 10.00 default");
                Some(10.0)
            }
        },
    };

    let memory = detection.ai_memory
        && Confirm::with_theme(&theme)
            .with_prompt("Enable long-term memory (ai-memory found)?")
            .default(false)
            .interact()
            .map_err(prompt)?;

    Ok(Plan {
        path,
        agents,
        strategy,
        sandbox,
        max_cost_usd,
        memory,
        notes,
    })
}

/// Render the plan, merge it into whatever is already there, and write —
/// unless `dry_run`.
fn apply(plan: &Plan, dry_run: bool) -> Result<Outcome> {
    let additions = plan_to_toml(plan);
    let existing = std::fs::read_to_string(&plan.path).unwrap_or_default();

    if dry_run {
        let (text, manual) = if existing.trim().is_empty() {
            (render_new_file(&additions), Vec::new())
        } else {
            match merge_text(&existing, &additions)? {
                Some((merged, manual)) => (
                    format!("(merged into the existing file)\n\n{merged}"),
                    manual,
                ),
                None => (
                    "(already configured — nothing to add)\n".to_owned(),
                    Vec::new(),
                ),
            }
        };
        return Ok(Outcome::DryRun {
            path: plan.path.clone(),
            text,
            manual,
            notes: plan.notes.clone(),
        });
    }

    if existing.trim().is_empty() {
        write_file(&plan.path, &render_new_file(&additions))?;
        return Ok(Outcome::Written {
            path: plan.path.clone(),
            merged: false,
            manual: Vec::new(),
            notes: plan.notes.clone(),
        });
    }

    let Some((merged, manual)) = merge_text(&existing, &additions)? else {
        return Ok(Outcome::Nothing);
    };

    // Never write a file CUMA itself cannot read back.
    Config::from_toml(&merged).map_err(|err| {
        MetaAgentError::Configuration(format!(
            "the merged configuration is invalid ({err}); nothing was written — \
             add the missing keys to {} by hand",
            plan.path.display()
        ))
    })?;

    let backup = plan.path.with_extension("toml.bak");
    let _ = std::fs::copy(&plan.path, &backup);
    write_file(&plan.path, &merged)?;

    Ok(Outcome::Written {
        path: plan.path.clone(),
        merged: true,
        manual,
        notes: plan.notes.clone(),
    })
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            MetaAgentError::Configuration(format!("cannot create {}: {err}", parent.display()))
        })?;
    }
    std::fs::write(path, text).map_err(|err| {
        MetaAgentError::Configuration(format!("cannot write {}: {err}", path.display()))
    })
}

/// The plan as a nested TOML document: the canonical form used both for a
/// fresh file and for computing what an existing file still lacks.
fn plan_to_toml(plan: &Plan) -> toml::Table {
    let mut root = toml::Table::new();

    root.insert(
        "router".to_owned(),
        toml::Value::Table(toml::Table::from_iter([(
            "strategy".to_owned(),
            toml::Value::String(kebab_strategy(plan.strategy).to_owned()),
        )])),
    );

    root.insert(
        "security".to_owned(),
        toml::Value::Table(toml::Table::from_iter([(
            "sandbox".to_owned(),
            toml::Value::Boolean(plan.sandbox),
        )])),
    );

    if let Some(budget) = plan.max_cost_usd {
        root.insert(
            "limits".to_owned(),
            toml::Value::Table(toml::Table::from_iter([(
                "max_cost_usd".to_owned(),
                toml::Value::Float(budget),
            )])),
        );
    }

    if plan.memory {
        root.insert(
            "memory".to_owned(),
            toml::Value::Table(toml::Table::from_iter([
                ("enabled".to_owned(), toml::Value::Boolean(true)),
                (
                    "backend".to_owned(),
                    toml::Value::String("ai-memory-cli".to_owned()),
                ),
            ])),
        );
    }

    if !plan.agents.is_empty() {
        let mut agents = toml::Table::new();
        for agent in &plan.agents {
            let mut entry = toml::Table::new();
            entry.insert("protocol".to_owned(), toml::Value::String("acp".to_owned()));
            if let Some(command) = &agent.command {
                entry.insert("command".to_owned(), toml::Value::String(command.clone()));
            }
            if !agent.state.is_empty() {
                entry.insert(
                    "state".to_owned(),
                    toml::Value::Array(
                        agent
                            .state
                            .iter()
                            .map(|s| toml::Value::String(s.clone()))
                            .collect(),
                    ),
                );
            }
            agents.insert(agent.id.clone(), toml::Value::Table(entry));
        }
        root.insert("agents".to_owned(), toml::Value::Table(agents));
    }

    root
}

fn kebab_strategy(strategy: RoutingStrategy) -> &'static str {
    match strategy {
        RoutingStrategy::Balanced => "balanced",
        RoutingStrategy::QualityFirst => "quality-first",
        RoutingStrategy::CostFirst => "cost-first",
        RoutingStrategy::LatencyFirst => "latency-first",
        RoutingStrategy::LocalFirst => "local-first",
        RoutingStrategy::PrivacyFirst => "privacy-first",
        RoutingStrategy::Manual => "manual",
    }
}

/// A fresh configuration file, commented the way the project's docs are.
fn render_new_file(additions: &toml::Table) -> String {
    format!(
        "# CUMA configuration — written by `cuma setup`.\n\
         # Every setting and its default: docs/CONFIGURATION.md.\n\n{}",
        toml::to_string_pretty(additions).unwrap_or_default()
    )
}

/// Merge `additions` into `existing` at the text level, preserving comments:
/// whole missing sections are appended, a missing scalar inside an existing
/// table is inserted just under its header, and a key that exists is never
/// touched. Returns the merged text plus any keys that could not be merged
/// automatically, or `None` when there was nothing to add.
fn merge_text(existing: &str, additions: &toml::Table) -> Result<Option<(String, Vec<String>)>> {
    let have: toml::Table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        toml::from_str(existing).map_err(|err| {
            MetaAgentError::Configuration(format!(
                "the existing configuration does not parse ({err}); fix it, \
                 or remove it and run setup again"
            ))
        })?
    };

    let want = delta(additions, &have);
    if want.is_empty() {
        return Ok(None);
    }

    let mut text = existing.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }

    // Scalars that must be spliced into an already-declared top-level table:
    // (table, key, rendered line). Everything else appends.
    let mut inserts: Vec<(String, String, String)> = Vec::new();
    let mut appends = String::new();

    for (top, value) in &want {
        if !have.contains_key(top) {
            appends.push_str(&render_section(top, value)?);
            continue;
        }
        // `top` exists. A missing sub-table (a new [agents.<id>]) appends;
        // a missing scalar inserts under the [top] header.
        let Some(sub) = value.as_table() else {
            continue;
        };
        for (key, leaf) in sub {
            if leaf.is_table() {
                let nested = toml::Table::from_iter([(key.clone(), leaf.clone())]);
                appends.push_str(&render_section(top, &toml::Value::Table(nested))?);
            } else {
                inserts.push((
                    top.clone(),
                    key.clone(),
                    format!("{key} = {}", toml_repr(leaf)),
                ));
            }
        }
    }

    let mut manual = Vec::new();
    for (table, key, line) in inserts {
        if insert_under_header(&mut text, &table, &line).is_none() {
            manual.push(format!("{table}.{key}: `{line}`"));
        }
    }

    text.push_str(&appends);
    Ok(Some((text, manual)))
}

/// Keys present in `want` but absent in `have`, recursively. An existing key
/// always wins — setup merges, it does not clobber.
fn delta(want: &toml::Table, have: &toml::Table) -> toml::Table {
    let mut out = toml::Table::new();
    for (key, value) in want {
        match (value, have.get(key)) {
            (_, None) => {
                out.insert(key.clone(), value.clone());
            }
            (toml::Value::Table(want_sub), Some(toml::Value::Table(have_sub))) => {
                let sub = delta(want_sub, have_sub);
                if !sub.is_empty() {
                    out.insert(key.clone(), toml::Value::Table(sub));
                }
            }
            (_, Some(_)) => {}
        }
    }
    out
}

/// `[top]` or `[top.sub]` TOML for one missing section.
fn render_section(top: &str, value: &toml::Value) -> Result<String> {
    let table = toml::Table::from_iter([(top.to_owned(), value.clone())]);
    let mut text = String::from("\n");
    text.push_str(
        &toml::to_string_pretty(&table).map_err(|err| {
            MetaAgentError::Configuration(format!("cannot render [{top}]: {err}"))
        })?,
    );
    Ok(text)
}

/// The position just after the `[table]` header line, or `None` when no such
/// header exists (the table came from dotted keys or an inline table — too
/// rare to rewrite, and reported instead).
fn insert_under_header(text: &mut String, table: &str, line: &str) -> Option<()> {
    let mut offset = 0;
    let mut at = None;
    for raw in text.lines() {
        let line_end = offset + raw.len();
        let trimmed = raw.trim();
        let header = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .filter(|(name, after)| {
                !name.trim().is_empty() && !name.contains('[') && {
                    let after = after.trim();
                    after.is_empty() || after.starts_with('#')
                }
            })
            .map(|(name, _)| name.trim());
        if header == Some(table) {
            at = Some((line_end + 1).min(text.len()));
            break;
        }
        offset = line_end + 1;
    }
    text.insert_str(at?, &format!("{line}\n"));
    Some(())
}

/// A TOML scalar as text: strings escaped, arrays inline, floats always with
/// a fractional part.
fn toml_repr(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => format!("{s:?}"),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) if f.fract() == 0.0 => format!("{f:.1}"),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_repr).collect::<Vec<_>>().join(", ")
        ),
        toml::Value::Table(_) => "{ … }".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn plan(agents: &[(&str, Option<&str>)], strategy: RoutingStrategy) -> Plan {
        Plan {
            path: PathBuf::from("/tmp/config.toml"),
            agents: agents
                .iter()
                .map(|(id, command)| PlannedAgent {
                    id: (*id).to_owned(),
                    command: command.map(str::to_owned),
                    state: Vec::new(),
                })
                .collect(),
            strategy,
            sandbox: true,
            max_cost_usd: Some(10.0),
            memory: false,
            notes: Vec::new(),
        }
    }

    fn merged(existing: &str, additions: &toml::Table) -> String {
        merge_text(existing, additions).unwrap().unwrap().0
    }

    #[test]
    fn a_fresh_plan_renders_a_valid_config_file() {
        let additions = plan_to_toml(&plan(
            &[("claude-code", None), ("codex", Some("codex-acp"))],
            RoutingStrategy::Balanced,
        ));
        let text = render_new_file(&additions);
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.router.strategy, RoutingStrategy::Balanced);
        assert!(config.agents.contains_key("claude-code"));
        assert_eq!(config.agents["codex"].command.as_deref(), Some("codex-acp"));
        assert_eq!(config.limits.max_cost_usd, Some(10.0));
    }

    #[test]
    fn a_well_known_agent_writes_no_command_line() {
        let additions = plan_to_toml(&plan(&[("codex", None)], RoutingStrategy::Balanced));
        let text = render_new_file(&additions);
        assert!(!text.contains("command"), "{text}");
    }

    #[test]
    fn a_new_agent_is_appended_to_an_existing_file() {
        let existing = "[agents.claude-code]\nprotocol = \"acp\"\n";
        let additions = plan_to_toml(&plan(
            &[("claude-code", None), ("codex", Some("codex-acp"))],
            RoutingStrategy::Balanced,
        ));
        let text = merged(existing, &additions);
        Config::from_toml(&text).unwrap();
        assert!(text.contains("[agents.codex]"), "{text}");
        assert_eq!(text.matches("[agents.claude-code]").count(), 1);
    }

    #[test]
    fn an_existing_key_is_never_clobbered() {
        let existing = "[router]\nstrategy = \"cost-first\"\n";
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::Balanced));
        let config = Config::from_toml(&merged(existing, &additions)).unwrap();
        assert_eq!(config.router.strategy, RoutingStrategy::CostFirst);
    }

    #[test]
    fn a_missing_scalar_is_inserted_under_its_table_header() {
        let existing = "# comment\n[router]\nexclude_agents = [\"slow\"]\n[agents.codex]\nprotocol = \"acp\"\n";
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::QualityFirst));
        let text = merged(existing, &additions);
        assert!(
            text.find("strategy").unwrap() < text.find("[agents.codex]").unwrap(),
            "{text}"
        );
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.router.strategy, RoutingStrategy::QualityFirst);
        assert_eq!(config.router.exclude_agents, vec!["slow".to_owned()]);
    }

    #[test]
    fn an_up_to_date_file_is_left_alone() {
        let existing = "[router]\nstrategy = \"balanced\"\n[security]\nsandbox = true\n[limits]\nmax_cost_usd = 10.0\n";
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::Balanced));
        assert!(merge_text(existing, &additions).unwrap().is_none());
    }

    #[test]
    fn a_malformed_existing_file_is_an_error_not_a_rewrite() {
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::Balanced));
        assert!(merge_text("[router = [[[", &additions).is_err());
    }

    #[test]
    fn a_table_from_dotted_keys_still_merges() {
        // `[security]` exists only as a dotted key — no header to match and
        // nothing to duplicate.
        let existing = "security.sandbox = true\n";
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::Balanced));
        let text = merged(existing, &additions);
        Config::from_toml(&text).unwrap();
    }

    #[test]
    fn a_scalar_missing_from_a_dotted_key_table_is_reported_not_guessed() {
        // `security` exists via a dotted key, so there is no `[security]`
        // header to insert under: the merge must not fabricate one.
        let existing = "security.checkpoint_before_write = true\n";
        let additions = plan_to_toml(&plan(&[], RoutingStrategy::Balanced));
        let (_, manual) = merge_text(existing, &additions).unwrap().unwrap();
        assert_eq!(
            manual,
            vec!["security.sandbox: `sandbox = true`".to_owned()]
        );
    }

    #[test]
    fn the_executable_inside_a_binary_archive_is_its_basename() {
        assert_eq!(binary_basename("./bin/devin"), "devin");
        assert_eq!(binary_basename(".\\bin\\devin.exe"), "devin");
        assert_eq!(binary_basename("goose"), "goose");
    }

    #[test]
    fn a_package_name_loses_its_version_and_scope() {
        assert_eq!(
            package_bin_name("@zed-industries/claude-code-acp@0.81.2"),
            "claude-code-acp"
        );
        assert_eq!(package_bin_name("fast-agent-acp==0.3.1"), "fast-agent-acp");
        assert_eq!(package_bin_name("pi-acp"), "pi-acp");
    }
}
