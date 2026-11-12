//! `cuma doctor`: the installation's health, checked in parallel.
//!
//! Each check runs in its own task and reports the moment it finishes, rather
//! than all output arriving at the end. On a terminal the report is a board
//! redrawn in place — a spinner marks the checks still running, a verdict the
//! ones done — and piped output prints each report as it lands. `--json`
//! waits for every check and says it all at once.

use crate::harness;
use crate::output::render_latency;
use cuma_config::{Config, ConfigSource};
use cuma_core::error::Result;
use futures::FutureExt as _;
use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write as _};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How a check — or one of its detail lines — came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Ok,
    Warn,
    Fail,
}

/// The checks `cuma doctor` runs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    Configuration,
    Workspace,
    Database,
    Memory,
    Sandbox,
    Rtk,
    Skills,
    Security,
    Agents,
}

impl Check {
    const ALL: [Self; 9] = [
        Self::Configuration,
        Self::Workspace,
        Self::Database,
        Self::Memory,
        Self::Sandbox,
        Self::Rtk,
        Self::Skills,
        Self::Security,
        Self::Agents,
    ];

    /// Its name on the board.
    fn name(self) -> &'static str {
        match self {
            Self::Configuration => "Configuration",
            Self::Workspace => "Workspace",
            Self::Database => "Database",
            Self::Memory => "Memory",
            Self::Sandbox => "Sandbox",
            Self::Rtk => "RTK",
            Self::Skills => "Skills",
            Self::Security => "Security",
            Self::Agents => "Agents",
        }
    }

    /// Its key in `--json` output.
    fn slug(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Workspace => "workspace",
            Self::Database => "database",
            Self::Memory => "memory",
            Self::Sandbox => "sandbox",
            Self::Rtk => "rtk",
            Self::Skills => "skills",
            Self::Security => "security",
            Self::Agents => "agents",
        }
    }

    /// Shown, dimmed, while the check is still running.
    fn pending(self) -> &'static str {
        match self {
            Self::Configuration => "resolving sources",
            Self::Workspace => "detecting version control",
            Self::Database => "opening the runtime store",
            Self::Memory => "probing the backend",
            Self::Sandbox => "probing runtimes",
            Self::Rtk => "measuring savings",
            Self::Skills => "reading installed skills",
            Self::Security => "checking policy",
            Self::Agents => "negotiating with agents",
        }
    }
}

/// A line under a check's summary, coloured by its own verdict.
#[derive(Debug)]
struct Detail {
    verdict: Verdict,
    text: String,
}

/// What a check concluded.
#[derive(Debug)]
struct Report {
    check: Check,
    verdict: Verdict,
    summary: String,
    details: Vec<Detail>,
    elapsed: Duration,
}

impl Report {
    fn new(check: Check, summary: impl Into<String>) -> Self {
        Self {
            check,
            verdict: Verdict::Ok,
            summary: summary.into(),
            details: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }

    /// Add a line under the summary; a serious one escalates the check.
    fn detail(&mut self, verdict: Verdict, text: impl Into<String>) {
        self.verdict = self.verdict.max(verdict);
        self.details.push(Detail {
            verdict,
            text: text.into(),
        });
    }

    /// The check itself warns or fails, not only a line in it.
    fn raise(&mut self, verdict: Verdict) {
        self.verdict = self.verdict.max(verdict);
    }
}

/// A report for a check that could not run at all.
fn failed(check: Check, text: impl Into<String>) -> Report {
    let mut report = Report::new(check, "could not run");
    report.detail(Verdict::Fail, text);
    report
}

/// A warning's text, mapped to the topical check that reported it.
///
/// `build_orchestrator` produces some of the same warnings the topical checks
/// report on their own — a sandbox shortfall, an unreachable memory backend.
/// The first check to reach a fact claims it, and the agents check then skips
/// the duplicate rather than saying the same thing twice.
type Claimed = Arc<Mutex<BTreeMap<String, Check>>>;

/// Everything a check needs: the merged configuration, the workspace it
/// serves, and the claim map above.
#[derive(Clone)]
struct Ctx {
    config: Config,
    workspace: PathBuf,
    claimed: Claimed,
}

impl Ctx {
    /// Record that `check` already reported `text`.
    fn claim(&self, check: Check, text: &str) {
        if let Ok(mut claimed) = self.claimed.lock() {
            claimed.insert(text.to_owned(), check);
        }
    }
}

/// ANSI colouring — off when stdout is not a terminal, the terminal is dumb,
/// or `NO_COLOR` / `CLICOLOR=0` asked for plain text. `FORCE_COLOR` /
/// `CLICOLOR_FORCE` force it on, piped or not.
#[derive(Clone, Copy)]
struct Paint {
    enabled: bool,
    tty: bool,
}

impl Paint {
    fn detect() -> Self {
        fn flag(name: &str) -> bool {
            std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
        }
        let forced = flag("FORCE_COLOR") || flag("CLICOLOR_FORCE");
        let plain = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var("CLICOLOR").ok().as_deref() == Some("0")
            || std::env::var("TERM").ok().as_deref() == Some("dumb");
        let tty = std::io::stdout().is_terminal();
        Self {
            enabled: forced || (tty && !plain),
            tty,
        }
    }

    fn wrap(&self, code: &str, text: impl std::fmt::Display) -> String {
        let text = text.to_string();
        if self.enabled && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    fn ok(&self, text: impl std::fmt::Display) -> String {
        self.wrap("32", text)
    }

    fn warn(&self, text: impl std::fmt::Display) -> String {
        self.wrap("33", text)
    }

    fn fail(&self, text: impl std::fmt::Display) -> String {
        self.wrap("31", text)
    }

    fn accent(&self, text: impl std::fmt::Display) -> String {
        self.wrap("36", text)
    }

    fn dim(&self, text: impl std::fmt::Display) -> String {
        self.wrap("2", text)
    }

    fn bold(&self, text: impl std::fmt::Display) -> String {
        self.wrap("1", text)
    }

    fn verdict(&self, verdict: Verdict, text: impl std::fmt::Display) -> String {
        match verdict {
            Verdict::Ok => self.ok(text),
            Verdict::Warn => self.warn(text),
            Verdict::Fail => self.fail(text),
        }
    }
}

/// Frames for the spinner a pending check shows.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The marker a finished check shows.
fn icon(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "✓",
        Verdict::Warn => "!",
        Verdict::Fail => "✗",
    }
}

/// The visible width of `line`, ANSI escapes excluded.
fn visible_len(line: &str) -> usize {
    let mut len = 0;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = c != 'm';
        } else if c == '\x1b' {
            escaped = true;
        } else {
            len += 1;
        }
    }
    len
}

/// The live board: the whole report, redrawn over itself as checks land.
struct Board {
    paint: Paint,
    /// Screen rows the last frame occupied — how far to rewind the cursor.
    rows: usize,
    /// Terminal width for the wrap estimate.
    columns: usize,
    frame: usize,
}

impl Board {
    fn new(paint: Paint, columns: usize) -> Self {
        Self {
            paint,
            rows: 0,
            columns,
            frame: 0,
        }
    }

    /// The title above the board; drawn once and never redrawn.
    fn header(&self, workspace: &Path) {
        println!();
        println!(
            "  {} {}",
            self.paint.bold("cuma doctor"),
            self.paint.dim(format!("— {}", workspace.display()))
        );
        println!();
    }

    /// Draw the whole board, rewinding over the previous frame.
    fn draw(&mut self, reports: &BTreeMap<Check, Report>, claimed: &Claimed) {
        let frame = self.render(reports, claimed);
        let mut out = std::io::stdout().lock();
        if self.rows > 0 {
            let _ = write!(out, "\x1b[{}A\x1b[J", self.rows);
        }
        let _ = write!(out, "{frame}");
        let _ = out.flush();
        self.rows = frame
            .lines()
            .map(|line| {
                let visible = visible_len(line).max(1);
                visible.div_ceil(self.columns.max(1))
            })
            .sum();
        self.frame += 1;
    }

    /// The board as text: one line per check plus its details.
    fn render(&self, reports: &BTreeMap<Check, Report>, claimed: &Claimed) -> String {
        let claims = claimed.lock().map(|c| c.clone()).unwrap_or_default();
        let mut out = String::new();
        for check in Check::ALL {
            match reports.get(&check) {
                None => out.push_str(&format!(
                    "  {} {:<13} {}\n",
                    self.paint.accent(SPINNER[self.frame % SPINNER.len()]),
                    check.name(),
                    self.paint.dim(check.pending())
                )),
                Some(report) => {
                    out.push_str(&format!(
                        "  {} {:<13} {} {}\n",
                        self.paint.verdict(report.verdict, icon(report.verdict)),
                        check.name(),
                        report.summary,
                        self.paint.dim(format!(
                            "({})",
                            render_latency(Some(report.elapsed.as_millis() as u64))
                        ))
                    ));
                    out.push_str(&self.details(report, &claims));
                }
            }
        }
        out
    }

    /// A report's detail lines, minus the ones another check already claimed.
    fn details(&self, report: &Report, claims: &BTreeMap<String, Check>) -> String {
        let mut out = String::new();
        for detail in &report.details {
            if claims
                .get(&detail.text)
                .is_some_and(|owner| *owner != report.check)
            {
                continue;
            }
            out.push_str(&format!(
                "      {} {}\n",
                self.paint.dim("›"),
                self.paint.verdict(detail.verdict, &detail.text)
            ));
        }
        out
    }

    /// In a pipe there is no board to redraw: print each report as it lands.
    fn print(&self, report: &Report, claimed: &Claimed) {
        let claims = claimed.lock().map(|c| c.clone()).unwrap_or_default();
        println!(
            "  {} {:<13} {} {}",
            self.paint.verdict(report.verdict, icon(report.verdict)),
            report.check.name(),
            report.summary,
            self.paint.dim(format!(
                "({})",
                render_latency(Some(report.elapsed.as_millis() as u64))
            ))
        );
        print!("{}", self.details(report, &claims));
    }

    /// The verdict over all of them.
    fn summary(&self, reports: &BTreeMap<Check, Report>) {
        let total = Check::ALL.len();
        let fails = reports
            .values()
            .filter(|r| r.verdict == Verdict::Fail)
            .count();
        let warns = reports
            .values()
            .filter(|r| r.verdict == Verdict::Warn)
            .count();
        let passed = total - fails - warns;

        println!();
        if fails == 0 && warns == 0 {
            println!(
                "  {}",
                self.paint
                    .ok(format!("✓ everything checks out — {total} checks passed"))
            );
        } else if fails == 0 {
            println!(
                "  {}",
                self.paint.warn(format!(
                    "! {passed}/{total} checks passed — {warns} warning{} need attention",
                    if warns == 1 { "" } else { "s" }
                ))
            );
        } else {
            println!(
                "  {}",
                self.paint.fail(format!(
                    "✗ {fails}/{total} checks failed — {passed} passed, {warns} warning{}",
                    if warns == 1 { "" } else { "s" }
                ))
            );
        }
        println!();
    }
}

/// The configuration layers that loaded.
fn configuration(sources: &[ConfigSource]) -> Report {
    let mut report = Report::new(
        Check::Configuration,
        match sources {
            [ConfigSource::Default] => "built-in defaults".to_owned(),
            [_] => "1 source".to_owned(),
            many => format!("{} sources", many.len()),
        },
    );
    for source in sources {
        report.detail(
            Verdict::Ok,
            match source {
                ConfigSource::Default => "built-in defaults".to_owned(),
                ConfigSource::File(path) => path.display().to_string(),
                ConfigSource::Environment => "CUMA_* environment".to_owned(),
            },
        );
    }
    report
}

/// Whether the workspace is versioned well enough to recover an agent's work.
async fn workspace_check(ctx: Ctx) -> Report {
    let git = cuma_workspace::GitWorkspace::detect(&ctx.workspace)
        .await
        .is_repository();
    if !git {
        let mut report = Report::new(Check::Workspace, "not a git repository");
        report.detail(Verdict::Warn, "agents' changes will not be recoverable");
        return report;
    }
    let mut report = Report::new(Check::Workspace, "git repository");
    if ctx.config.security.checkpoint_before_write {
        report.detail(Verdict::Ok, "checkpointing before writes");
    } else {
        report.detail(
            Verdict::Warn,
            "NOT checkpointing — security.checkpoint_before_write is off",
        );
    }
    report
}

/// The runtime database sessions and routing history live in.
async fn database(ctx: Ctx) -> Report {
    let path = harness::database_path(&ctx.config, &ctx.workspace);
    let claimed = ctx.claimed.clone();
    tokio::task::spawn_blocking(move || {
        match cuma_persistence::RuntimeStore::open(&path) {
            Ok(store) => {
                let mut report = Report::new(
                    Check::Database,
                    format!(
                        "{} — {} sessions",
                        path.display(),
                        store.session_count().unwrap_or(0)
                    ),
                );
                // The same warning `build_orchestrator` records, claimed here
                // so it is not repeated under Agents.
                if let Err(err) = store.load_routing_history() {
                    let text = format!("could not load routing history: {err}");
                    claim(&claimed, Check::Database, &text);
                    report.detail(Verdict::Warn, text);
                }
                report
            }
            Err(err) => {
                let text = format!("could not open the runtime database: {err}");
                claim(&claimed, Check::Database, &text);
                let mut report =
                    Report::new(Check::Database, format!("{} — cannot open", path.display()));
                report.detail(Verdict::Fail, text);
                report
            }
        }
    })
    .await
    .unwrap_or_else(|err| failed(Check::Database, format!("check aborted: {err}")))
}

/// Whether the long-term memory backend answers.
async fn memory(ctx: Ctx) -> Report {
    if !ctx.config.memory.enabled {
        return Report::new(Check::Memory, "disabled");
    }
    let backend = ctx.config.memory.backend.clone();
    let store = harness::memory_store(&ctx.config, &ctx.workspace);
    if store.is_available().await {
        Report::new(Check::Memory, format!("{backend} is reachable"))
    } else {
        // `build_orchestrator` records this same fact as a warning; claiming
        // its exact text keeps the agents check from listing it twice.
        ctx.claim(Check::Memory, harness::MEMORY_UNREACHABLE);
        let mut report = Report::new(
            Check::Memory,
            format!("enabled, but {backend} is not reachable"),
        );
        report.detail(Verdict::Warn, "running without recall");
        report
    }
}

/// What confines the commands CUMA runs, and the sandboxes agents run in.
async fn sandbox(ctx: Ctx) -> Report {
    use cuma_sandbox::SandboxProvider;

    let sandbox = cuma_workspace::Sandbox::detect(&ctx.config.security);
    let describe = sandbox.status().describe();
    let mut report = Report::new(
        Check::Sandbox,
        describe
            .strip_prefix("sandbox: ")
            .unwrap_or(&describe)
            .to_owned(),
    );
    // Requested but unavailable is exactly the case an operator must not
    // discover by having something escape.
    if !(sandbox.status().is_active()
        || matches!(sandbox.status(), cuma_workspace::SandboxStatus::Disabled))
    {
        report.raise(Verdict::Fail);
    }

    let registry = harness::sandbox_registry(&ctx.config);
    if !registry.enabled() {
        report.detail(Verdict::Warn, "agents: unconfined (sandbox disabled)");
        return report;
    }

    // Plugins first: a plugin only says what it enforces after a probe.
    let mut probed = Vec::new();
    harness::probe_plugins(&ctx.config, &registry, &mut probed).await;
    for warning in probed {
        ctx.claim(Check::Sandbox, &warning);
        report.detail(Verdict::Warn, warning);
    }

    let mut groups: BTreeMap<String, (Arc<dyn SandboxProvider>, Vec<&str>)> = BTreeMap::new();
    for (id, agent) in &ctx.config.agents {
        if !agent.enabled || !agent.protocol.eq_ignore_ascii_case("acp") {
            continue;
        }
        if let Some(provider) = registry.for_agent(id) {
            groups
                .entry(provider.name().to_owned())
                .or_insert_with(|| (provider, Vec::new()))
                .1
                .push(id);
        }
    }
    if groups.is_empty()
        && let Some(provider) = registry.default_provider()
    {
        groups.insert(provider.name().to_owned(), (provider, Vec::new()));
    }

    for (provider, agents) in groups.into_values() {
        if let Some(shortfall) = provider.shortfall(&ctx.config.security.network_allowlist) {
            ctx.claim(Check::Sandbox, &shortfall);
            report.detail(Verdict::Warn, shortfall);
        } else if agents.is_empty() {
            report.detail(Verdict::Ok, provider.describe());
        } else {
            report.detail(
                Verdict::Ok,
                format!("{} [{}]", provider.describe(), agents.join(", ")),
            );
        }
    }
    report
}

/// Whether command output is filtered before it costs tokens.
async fn rtk(ctx: Ctx) -> Report {
    let rtk_config = ctx.config.rtk.clone();
    let workspace = ctx.workspace.clone();
    tokio::task::spawn_blocking(move || {
        let rtk = cuma_workspace::Rtk::detect(&rtk_config);
        let describe = rtk.status().describe();
        let mut report = Report::new(
            Check::Rtk,
            describe
                .strip_prefix("RTK: ")
                .unwrap_or(&describe)
                .to_owned(),
        );
        if rtk.status().is_fatal() {
            report.raise(Verdict::Fail);
        } else if matches!(rtk.status(), cuma_workspace::RtkStatus::Unverified { .. }) {
            report.raise(Verdict::Warn);
        }
        if rtk.status().is_active() {
            match rtk.measured_gain(Some(&workspace)) {
                Some(gain) if gain.total_commands > 0 => report.detail(
                    Verdict::Ok,
                    format!(
                        "measured here: {} commands, {} tokens saved ({:.0}% on average)",
                        gain.total_commands, gain.total_saved, gain.avg_savings_pct
                    ),
                ),
                _ => report.detail(Verdict::Ok, "has not filtered anything here yet"),
            }
            // CUMA wraps the commands it runs itself; agents run their own
            // shell commands, which only RTK's per-agent hooks can reach.
            report.detail(
                Verdict::Ok,
                "agents filter their own shell output once hooked — `rtk init -g` for \
                 Claude Code, `rtk init -g --agent <name>` for others",
            );
        }
        report
    })
    .await
    .unwrap_or_else(|err| failed(Check::Rtk, format!("check aborted: {err}")))
}

/// Installed skills and the warnings loading them produced.
async fn skills(ctx: Ctx) -> Report {
    if !ctx.config.skills.enabled {
        return Report::new(Check::Skills, "disabled");
    }
    let skills_config = ctx.config.skills.clone();
    let workspace = ctx.workspace.clone();
    let built =
        tokio::task::spawn_blocking(move || cuma_skills::from_config(&skills_config, &workspace))
            .await;
    match built {
        Err(err) => failed(Check::Skills, format!("check aborted: {err}")),
        Ok(Err(err)) => {
            let text = format!("skills are unavailable: {err}");
            ctx.claim(Check::Skills, &text);
            let mut report = Report::new(Check::Skills, "unavailable");
            report.detail(Verdict::Warn, text);
            report
        }
        Ok(Ok((manager, warnings))) => {
            let mut report = Report::new(Check::Skills, "");
            for warning in warnings {
                ctx.claim(Check::Skills, &warning);
                report.detail(Verdict::Warn, warning);
            }
            let installed = manager.installed().await;
            let enabled = installed.iter().filter(|skill| skill.enabled).count();
            report.summary = match installed.len() {
                0 => "none installed".to_owned(),
                n => format!("{n} installed, {enabled} enabled"),
            };
            report
        }
    }
}

/// The guardrails deciding what agents may do unsupervised.
fn security(ctx: &Ctx) -> Report {
    if ctx.config.security.allow_destructive_operations {
        let mut report = Report::new(Check::Security, "destructive operations ALLOWED");
        report.detail(
            Verdict::Warn,
            "security.allow_destructive_operations is on; agents may run destructive commands",
        );
        report
    } else {
        Report::new(Check::Security, "destructive operations denied")
    }
}

/// The agents themselves: discovery, negotiation, health.
async fn agents(ctx: Ctx) -> Report {
    let (orchestrator, warnings) =
        match harness::build_orchestrator(ctx.config.clone(), ctx.workspace.clone()).await {
            Ok(pair) => pair,
            Err(err) => {
                return failed(Check::Agents, format!("could not build the harness: {err}"));
            }
        };

    let snapshot = orchestrator.agents().snapshot().await;
    let mut report = if snapshot.is_empty() {
        let mut report = Report::new(Check::Agents, "no agents available");
        report.detail(Verdict::Fail, harness::no_agents_reason(&ctx.config).await);
        report
    } else {
        let mut report = Report::new(
            Check::Agents,
            format!(
                "{} registered, {} routable",
                snapshot.len(),
                snapshot.routable().count()
            ),
        );
        for agent in snapshot.all() {
            if !agent.is_routable() {
                report.detail(
                    Verdict::Warn,
                    format!(
                        "agent {} is not routable ({:?}){}",
                        agent.id,
                        agent.health.state,
                        agent
                            .health
                            .last_error
                            .as_ref()
                            .map_or(String::new(), |e| format!(": {e}"))
                    ),
                );
            }
        }
        report
    };
    // Whatever building the harness noticed that no topical check claimed.
    for warning in warnings {
        report.detail(Verdict::Warn, warning);
    }
    report
}

/// Record that `check` already reported `text` — the non-async form for the
/// blocking checks, which have no `Ctx` inside their closure.
fn claim(claimed: &Claimed, check: Check, text: &str) {
    if let Ok(mut claimed) = claimed.lock() {
        claimed.insert(text.to_owned(), check);
    }
}

/// Run one check on its own task and post its report the moment it lands.
fn spawn<F>(tx: &mpsc::UnboundedSender<Report>, check: Check, future: F)
where
    F: std::future::Future<Output = Report> + Send + 'static,
{
    let tx = tx.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let mut report = AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| failed(check, "the check panicked"));
        report.elapsed = started.elapsed();
        let _ = tx.send(report);
    });
}

/// `verdict` as JSON writes it.
fn status(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "ok",
        Verdict::Warn => "warn",
        Verdict::Fail => "fail",
    }
}

/// Check the installation.
pub async fn run(
    config: Config,
    workspace: PathBuf,
    sources: Vec<ConfigSource>,
    json: bool,
) -> Result<()> {
    let claimed = Claimed::default();
    let ctx = Ctx {
        config,
        workspace: workspace.clone(),
        claimed: claimed.clone(),
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<Report>();
    let security_ctx = ctx.clone();
    spawn(
        &tx,
        Check::Configuration,
        async move { configuration(&sources) },
    );
    spawn(&tx, Check::Workspace, workspace_check(ctx.clone()));
    spawn(&tx, Check::Database, database(ctx.clone()));
    spawn(&tx, Check::Memory, memory(ctx.clone()));
    spawn(&tx, Check::Sandbox, sandbox(ctx.clone()));
    spawn(&tx, Check::Rtk, rtk(ctx.clone()));
    spawn(&tx, Check::Skills, skills(ctx.clone()));
    spawn(&tx, Check::Security, async move { security(&security_ctx) });
    spawn(&tx, Check::Agents, agents(ctx));
    drop(tx);

    // The board redraws in place only where a redraw is meaningful: a real
    // terminal whose width is known. Everywhere else reports print as they
    // land, or wait for the JSON document.
    let paint = Paint::detect();
    let columns = crossterm::terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(0);
    let live = paint.tty && !json && columns > 0;
    let mut board = Board::new(paint, columns);

    if !json {
        board.header(&workspace);
    }

    let mut reports: BTreeMap<Check, Report> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(90));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if live {
        board.draw(&reports, &claimed);
    }

    while reports.len() < Check::ALL.len() {
        tokio::select! {
            got = rx.recv() => {
                let Some(report) = got else { break };
                if !live && !json {
                    board.print(&report, &claimed);
                }
                reports.insert(report.check, report);
                if live {
                    board.draw(&reports, &claimed);
                }
            }
            _ = tick.tick(), if live => board.draw(&reports, &claimed),
        }
    }

    // A check whose task died before reporting still gets a verdict.
    let mut missing = false;
    for check in Check::ALL {
        if let std::collections::btree_map::Entry::Vacant(slot) = reports.entry(check) {
            slot.insert(failed(check, "no report arrived"));
            missing = true;
        }
    }
    if live && missing {
        board.draw(&reports, &claimed);
    }

    if json {
        let claims = claimed.lock().map(|c| c.clone()).unwrap_or_default();
        let checks: Vec<serde_json::Value> = Check::ALL
            .iter()
            .filter_map(|check| reports.get(check))
            .map(|report| {
                let details: Vec<serde_json::Value> = report
                    .details
                    .iter()
                    .filter(|d| {
                        claims
                            .get(&d.text)
                            .is_none_or(|owner| *owner == report.check)
                    })
                    .map(|d| serde_json::json!({ "status": status(d.verdict), "text": d.text }))
                    .collect();
                serde_json::json!({
                    "name": report.check.slug(),
                    "status": status(report.verdict),
                    "summary": report.summary,
                    "elapsed_ms": report.elapsed.as_millis() as u64,
                    "details": details,
                })
            })
            .collect();
        let healthy = reports.values().all(|r| r.verdict == Verdict::Ok);
        let problems: Vec<String> = reports
            .values()
            .filter(|r| r.verdict != Verdict::Ok)
            .flat_map(|r| {
                let summary = (r.verdict == Verdict::Fail)
                    .then(|| format!("{}: {}", r.check.slug(), r.summary));
                let details = r
                    .details
                    .iter()
                    .filter(|d| d.verdict != Verdict::Ok)
                    .filter(|d| claims.get(&d.text).is_none_or(|owner| *owner == r.check))
                    .map(|d| format!("{}: {}", r.check.slug(), d.text));
                summary.into_iter().chain(details)
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "healthy": healthy,
                "checks": checks,
                "problems": problems,
            }))
            .unwrap_or_default()
        );
    } else {
        board.summary(&reports);
    }

    Ok(())
}
