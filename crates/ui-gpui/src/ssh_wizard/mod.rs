//! The SSH install wizard (an overlay): picks or takes a host, checks it, uploads this Gantry through ssh, verifies it,
//! offers lingering, then asks the workspace to save and connect the host. The same overlay installs Gantry on a saved
//! host (NotInstalled card) and updates it (UpdateNeeded card: upload, then a restart question). It saves nothing
//! itself; its steps are `daemon::remote_install` runs (`run.rs`), and its logic is `app-core::host_install`.

pub(crate) mod run;

use std::path::Path;

use app_core::host_install::{CheckResult, GantryOnRemote, Linger, Probe, Step, check, next_step, upload_path};
use app_core::host_state::FailKind;
use app_core::settings::{DEFAULT_REMOTE_COMMAND, SshAuth, SshHost, parse_port};
use app_core::ssh_config::{aliases, validate_name, validate_target, wildcard_match};
use app_core::text_field::TextField;
use app_core::version::parse_version_output;
use daemon::askpass::AskPrompt;
use daemon::ssh::{AttemptHandle, Share, SshHostSpec};
use gpui::{
    App, ClickEvent, Context, Div, ElementId, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, Render, SharedString, Stateful, Task, Window, div, prelude::*, px, rgb,
};

use crate::field;
use crate::settings_store::SettingsStore;
use crate::terminal_view::FONT_FAMILY;
use crate::theme::{FadeKind, RADIUS_LARGE, RADIUS_SMALL, Theme, fade_in, theme};

use run::{Job, Msg, Questions};

/// What the wizard was opened for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WizardStart {
    /// A new host.
    Add,
    /// Install Gantry on this saved host (its card says it is not installed).
    Install(SshHost),
    /// Update Gantry on this saved host, then restart its daemon.
    Update(SshHost),
}

/// What the wizard asks the workspace to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WizardEvent {
    /// Take the wizard down; `draft` is the name its sign-in questions were asked under.
    Closed {
        /// The host name used in the dialogs.
        draft: String,
    },
    /// The host is ready: save it when `add`, then connect it.
    Finished {
        /// The host.
        host: SshHost,
        /// It is new: save it in `sshHosts` first.
        add: bool,
        /// Its daemon was just stopped to update it: connecting may meet the old daemon for a moment.
        restarted: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Target,
    Name,
    Port,
    Command,
}

/// The wizard overlay.
pub(crate) struct SshWizard {
    focus_handle: FocusHandle,
    start: WizardStart,
    step: Step,
    questions: Questions,
    /// The ssh connection every step of this run shares, so a password is asked once.
    share: Option<Share>,
    /// The names of the saved hosts (a new name must differ).
    existing: Vec<String>,
    aliases: Vec<String>,
    target: TextField,
    name: TextField,
    command: TextField,
    port: TextField,
    /// The user typed in the name field, so it is no longer filled from the target.
    name_edited: bool,
    advanced: bool,
    /// How to sign in to the host.
    auth: SshAuth,
    active: Field,
    check: Option<CheckResult>,
    /// Bytes of the upload sent, and its size.
    sent: u64,
    total: u64,
    /// The remote's `--version` line after an install.
    verified: Option<String>,
    /// What the running step is called, while one runs.
    working: Option<&'static str>,
    /// Why the last step failed.
    error: Option<String>,
    job: Option<(AttemptHandle, Task<()>)>,
    linger_tick: bool,
    /// What `loginctl enable-linger` said when it failed.
    linger_note: Option<String>,
}

impl EventEmitter<WizardEvent> for SshWizard {}

impl Focusable for SshWizard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Drop for SshWizard {
    fn drop(&mut self) {
        if let Some((handle, _)) = &self.job {
            handle.cancel();
        }
        self.close_share();
    }
}

fn text(content: &str) -> TextField {
    let mut field = TextField::default();
    field.insert(content);
    field
}

/// The text of an `Include` of an ssh config: `~` is the home folder, a relative path is relative to `ssh_dir`, and
/// `*` or `?` in the file name match files of that folder (their texts joined, in name order). `None` when nothing
/// was read.
fn include_text(name: &str, ssh_dir: &Path, home: &Path) -> Option<String> {
    let expanded = match name.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => ssh_dir.join(name),
    };
    let pattern = expanded.file_name()?.to_string_lossy().into_owned();
    if !pattern.contains(['*', '?']) {
        return std::fs::read_to_string(&expanded).ok();
    }
    let mut files: Vec<_> = std::fs::read_dir(expanded.parent()?)
        .ok()?
        .flatten()
        .filter(|entry| wildcard_match(&pattern, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    let texts: Vec<String> = files.iter().filter_map(|path| std::fs::read_to_string(path).ok()).collect();
    (!texts.is_empty()).then(|| texts.concat())
}

/// The aliases of `~/.ssh/config`, with one level of `Include`.
fn ssh_config_aliases() -> Vec<String> {
    let Some(home) = std::env::home_dir() else { return Vec::new() };
    let dir = home.join(".ssh");
    let Ok(config) = std::fs::read_to_string(dir.join("config")) else { return Vec::new() };
    aliases(&config, &mut |name| include_text(name, &dir, &home))
}

/// The suggested name of a target: an alias as it is, `user@host` as the host (its first label for a DNS name, whole
/// for an IP address).
fn suggested_name(target: &str) -> String {
    let host = target.rsplit('@').next().unwrap_or(target);
    if target.contains('@') && host.parse::<std::net::IpAddr>().is_err() {
        host.split('.').next().unwrap_or(host).to_string()
    } else {
        host.to_string()
    }
}

/// Where `gantryd`, the window-less Gantry the wizard uploads, sits: beside the file this Gantry started from (which a
/// reinstall may have replaced: the path then ends in " (deleted)").
fn headless_beside(exe: &std::path::Path) -> std::path::PathBuf {
    let name = exe.to_string_lossy();
    let exe = std::path::PathBuf::from(name.strip_suffix(" (deleted)").unwrap_or(&name));
    exe.with_file_name("gantryd")
}

/// The file to upload: `gantryd` beside this Gantry, provided it is this Gantry's version (after a reinstall the file
/// beside the running one may be newer).
fn headless_binary() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|_| "Could not find this Gantry's own file.".to_string())?;
    let path = headless_beside(&exe);
    if !path.is_file() {
        return Err(format!("{} is missing: install Gantry again.", path.display()));
    }
    let output =
        claude_index::process::hide_window(&mut std::process::Command::new(&path)).arg("--version").output().ok();
    let found = output
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| parse_version_output(text.trim()))
        .map(|version| version.to_string());
    match found {
        Some(version) if version == protocol::build::VERSION => Ok(path),
        Some(version) => Err(format!(
            "The installed gantryd is {version}, this Gantry is {}: restart Gantry, then try again.",
            protocol::build::VERSION
        )),
        None => Err(format!("{} does not run: install Gantry again.", path.display())),
    }
}

/// Why this Gantry cannot upload itself to a host whose remote command is `command`, if it cannot.
pub(crate) fn upload_block(command: &str) -> Option<String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Some("This Gantry cannot be uploaded to a Linux x86_64 host: install it by hand.".into());
    }
    upload_path(command).err()
}

/// The file to upload to a host whose remote command is `command`, or why nothing can be uploaded.
pub(crate) fn upload_preflight(command: &str) -> Result<std::path::PathBuf, String> {
    match upload_block(command) {
        Some(reason) => Err(reason),
        None => headless_binary(),
    }
}

/// What a failed step says.
pub(crate) fn describe(kind: &FailKind) -> String {
    match kind {
        FailKind::NeedsInput => "Sign-in was cancelled or refused.".into(),
        FailKind::NotInstalled => "A command was not found on the host.".into(),
        FailKind::UpdateNeeded { .. } => "The host answered unexpectedly.".into(),
        FailKind::Other { reason } => reason.clone(),
    }
}

impl SshWizard {
    /// A wizard for `start`; `existing` are the names of the saved hosts. Install and Update begin by checking the host.
    pub(crate) fn new(
        start: WizardStart,
        existing: Vec<String>,
        prompts: async_channel::Sender<AskPrompt>,
        runtime_dir: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let (target, name, command, auth, port, step) = match &start {
            WizardStart::Add => (
                String::new(),
                String::new(),
                DEFAULT_REMOTE_COMMAND.to_string(),
                SshAuth::Automatic,
                String::new(),
                Step::Host,
            ),
            WizardStart::Install(host) | WizardStart::Update(host) => (
                host.target.clone(),
                host.name.clone(),
                host.command.clone(),
                host.auth,
                host.port.map(|port| port.to_string()).unwrap_or_default(),
                Step::Check,
            ),
        };
        let mut wizard = SshWizard {
            focus_handle: cx.focus_handle(),
            aliases: if start == WizardStart::Add { ssh_config_aliases() } else { Vec::new() },
            start,
            step,
            share: Share::new(&runtime_dir),
            questions: Questions { prompts, runtime_dir },
            existing,
            target: text(&target),
            name: text(&name),
            command: text(&command),
            port: text(&port),
            name_edited: false,
            advanced: false,
            auth,
            active: Field::Target,
            check: None,
            sent: 0,
            total: 0,
            verified: None,
            working: None,
            error: None,
            job: None,
            linger_tick: true,
            linger_note: None,
        };
        if wizard.step == Step::Check {
            wizard.begin_check(cx);
        }
        wizard
    }

    /// The name the sign-in dialogs use for this wizard's host.
    pub(crate) fn draft_name(&self) -> String {
        self.name.text().trim().to_string()
    }

    fn fixed(&self) -> bool {
        self.start != WizardStart::Add
    }

    fn spec(&self) -> SshHostSpec {
        SshHostSpec {
            name: self.draft_name(),
            target: self.target.text().trim().to_string(),
            command: self.command.text().trim().to_string(),
            auth: self.auth,
            port: parse_port(self.port.text()).ok().flatten(),
            share: self.share.clone(),
        }
    }

    /// Hand the shared connection over (to the final connect); the wizard no longer closes it.
    pub(crate) fn take_share(&mut self) -> Option<Share> {
        self.share.take()
    }

    /// Close the shared connection's master (best effort, off the UI thread).
    fn close_share(&mut self) {
        if let Some(share) = self.share.take() {
            let target = self.target.text().trim().to_string();
            std::thread::spawn(move || share.close("ssh", &target, false));
        }
    }

    fn name_error(&self) -> Option<String> {
        if self.fixed() {
            return None;
        }
        let existing: Vec<&str> = self.existing.iter().map(String::as_str).collect();
        validate_name(self.name.text(), &existing).err()
    }

    fn target_error(&self) -> Option<String> {
        validate_target(self.target.text().trim()).err()
    }

    fn command_error(&self) -> Option<String> {
        self.command.text().trim().is_empty().then(|| "Enter the remote command".to_string())
    }

    fn port_error(&self) -> Option<String> {
        parse_port(self.port.text()).err()
    }

    fn host_ready(&self) -> bool {
        self.name_error().is_none()
            && self.target_error().is_none()
            && self.command_error().is_none()
            && self.port_error().is_none()
    }

    fn sync_name(&mut self) {
        if !self.name_edited && !self.fixed() {
            self.name = text(&suggested_name(self.target.text().trim()));
        }
    }

    /// Why the upload cannot start, if it cannot.
    fn install_block(&self) -> Option<String> {
        upload_block(self.command.text().trim())
    }

    /// Replace the running step with `job`, listening to its messages.
    fn run(&mut self, job: Job, label: &'static str, cx: &mut Context<Self>) {
        if let Some((handle, _)) = self.job.take() {
            handle.cancel();
        }
        self.working = Some(label);
        self.error = None;
        let Job { handle, messages } = job;
        let task = cx.spawn(async move |this, cx| {
            while let Ok(message) = messages.recv().await {
                if this.update(cx, |wizard, cx| wizard.on_message(message, cx)).is_err() {
                    return;
                }
            }
        });
        self.job = Some((handle, task));
        cx.notify();
    }

    fn begin_check(&mut self, cx: &mut Context<Self>) {
        self.step = Step::Check;
        self.check = None;
        let job = run::probe(self.spec(), &self.questions);
        self.run(job, "Checking the host…", cx);
    }

    fn begin_install(&mut self, cx: &mut Context<Self>) {
        if self.install_block().is_some() {
            return;
        }
        let local = match headless_binary() {
            Ok(local) => local,
            Err(message) => {
                self.error = Some(message);
                return;
            }
        };
        self.step = Step::Install;
        self.sent = 0;
        self.total = std::fs::metadata(&local).map_or(0, |meta| meta.len());
        self.verified = None;
        let job = run::install(self.spec(), &self.questions, local);
        self.run(job, "Uploading…", cx);
    }

    fn on_message(&mut self, message: Msg, cx: &mut Context<Self>) {
        match message {
            Msg::Progress(sent) => self.sent = sent,
            Msg::Probed(result) => {
                self.working = None;
                match result {
                    Ok(probe) => self.checked(&probe),
                    Err(kind) => self.error = Some(describe(&kind)),
                }
            }
            Msg::Installed(result) => {
                self.working = None;
                match result {
                    Ok(line) => self.verified = Some(line),
                    Err(kind) => self.error = Some(describe(&kind)),
                }
            }
            Msg::LingerDone(result) => {
                self.working = None;
                self.job = None;
                match result {
                    Ok(()) => self.finish(cx),
                    // Stay on Finish: the note says what to run, and the button saves anyway.
                    Err(reason) => self.linger_note = Some(reason),
                }
            }
            Msg::Stopped(result) => {
                self.working = None;
                match result {
                    Ok(()) => {
                        self.job = None;
                        self.finish(cx);
                    }
                    Err(kind) => self.error = Some(describe(&kind)),
                }
            }
        }
        cx.notify();
    }

    fn checked(&mut self, probe: &Probe) {
        self.check = Some(check(probe, protocol::build::VERSION));
    }

    /// Tell the workspace the host is ready.
    fn finish(&mut self, cx: &mut Context<Self>) {
        let add = self.start == WizardStart::Add;
        if add && cx.global::<SettingsStore>().broken() {
            self.error = Some("Not saved: fix settings.json first.".into());
            return;
        }
        let spec = self.spec();
        let restarted = matches!(self.start, WizardStart::Update(_));
        cx.emit(WizardEvent::Finished {
            host: SshHost {
                name: spec.name,
                target: spec.target,
                command: spec.command,
                auth: spec.auth,
                port: spec.port,
            },
            add,
            restarted,
        });
    }

    /// The workspace could not save the host: say why and stay open.
    pub(crate) fn refuse(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some((handle, _)) = self.job.take() {
            handle.cancel();
        }
        self.close_share();
        cx.emit(WizardEvent::Closed { draft: self.draft_name() });
    }

    /// The primary button of the current step.
    fn advance(&mut self, cx: &mut Context<Self>) {
        if self.working.is_some() {
            return;
        }
        if self.error.is_some() {
            self.retry(cx);
            return;
        }
        match self.step {
            Step::Host => {
                if self.host_ready() {
                    self.begin_check(cx);
                }
            }
            Step::Check => {
                let Some(result) = self.check.clone() else { return };
                if !result.supported {
                    return;
                }
                match next_step(Step::Check, &result) {
                    Step::Install => self.begin_install(cx),
                    step => self.step = step,
                }
            }
            Step::Install => {
                if self.verified.is_some()
                    && let Some(result) = &self.check
                {
                    self.step = next_step(Step::Install, result);
                }
            }
            Step::Finish => self.finish_click(cx),
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.linger_note = None;
        match self.step {
            Step::Host => {}
            Step::Check => self.begin_check(cx),
            Step::Install => self.begin_install(cx),
            Step::Finish => self.finish_click(cx),
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.working.is_some() {
            return;
        }
        self.linger_note = None;
        self.error = None;
        match self.step {
            Step::Host => {}
            Step::Check => {
                if self.fixed() {
                    // Nothing before the check of a saved host.
                    self.close(cx);
                    return;
                }
                self.step = Step::Host;
            }
            Step::Install | Step::Finish => self.step = Step::Check,
        }
        cx.notify();
    }

    /// Finish's button: restart (Update), enable lingering first when ticked, else hand over the host.
    fn finish_click(&mut self, cx: &mut Context<Self>) {
        if matches!(self.start, WizardStart::Update(_)) {
            let job = run::stop(self.spec(), &self.questions);
            self.run(job, "Stopping the daemon…", cx);
            return;
        }
        if self.linger_note.is_none()
            && self.linger_tick
            && self.check.as_ref().is_some_and(|check| check.linger == Linger::Off)
        {
            let job = run::linger(self.spec(), &self.questions);
            self.run(job, "Turning lingering on…", cx);
        } else {
            self.finish(cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" => self.close(cx),
            "enter" => self.advance(cx),
            "tab" if self.step == Step::Host => {
                let fields = if self.advanced {
                    vec![Field::Target, Field::Name, Field::Port, Field::Command]
                } else {
                    vec![Field::Target, Field::Name]
                };
                let at = fields.iter().position(|field| *field == self.active).unwrap_or(0);
                let next = if keystroke.modifiers.shift {
                    (at + fields.len() - 1) % fields.len()
                } else {
                    (at + 1) % fields.len()
                };
                self.active = fields[next];
            }
            _ if self.step == Step::Host && self.working.is_none() => {
                let active = self.active;
                let field = match active {
                    Field::Target => &mut self.target,
                    Field::Name => &mut self.name,
                    Field::Port => &mut self.port,
                    Field::Command => &mut self.command,
                };
                if !field::edit(field, keystroke, cx) {
                    return;
                }
                match active {
                    Field::Target => self.sync_name(),
                    Field::Name => self.name_edited = true,
                    Field::Port | Field::Command => {}
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn pick_alias(&mut self, alias: &str) {
        self.target = text(alias);
        self.name_edited = false;
        self.sync_name();
        self.active = Field::Name;
    }
}

fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(theme.ui(32.))
        .px(theme.ui(14.))
        .flex()
        .items_center()
        .rounded(px(RADIUS_SMALL))
        .border_1()
        .border_color(rgb(if primary { theme.tokens.accent } else { theme.tokens.line }))
        .when(primary, |b| {
            b.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)).font_weight(FontWeight::SEMIBOLD)
        })
        .when(!primary, |b| b.bg(rgb(theme.tokens.raised)))
        .when(enabled, |b| b.cursor_pointer().hover(|style| style.border_color(rgb(theme.tokens.accent))))
        .when(!enabled, |b| b.opacity(0.45))
        .child(label.into())
}

fn text_box(theme: &Theme, content: Div) -> Div {
    div()
        .h(theme.ui(30.))
        .px(theme.ui(10.))
        .flex()
        .items_center()
        .rounded(px(RADIUS_SMALL))
        .border_1()
        .border_color(rgb(theme.tokens.line))
        .bg(rgb(theme.tokens.ground))
        .child(content.flex_1().h_full())
}

fn check_box(theme: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .size(theme.ui(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(RADIUS_SMALL / 2.))
        .border_1()
        .border_color(rgb(if on { theme.tokens.accent } else { theme.tokens.line }))
        .text_size(theme.ui(11.))
        .when(on, |b| b.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)))
        .child(if on { "✓" } else { "" })
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

impl SshWizard {
    fn title(&self) -> String {
        match (&self.start, self.step) {
            (WizardStart::Add, Step::Host) => "Add an SSH host".into(),
            (WizardStart::Install(host), _) => format!("Install Gantry on {}", host.name),
            (WizardStart::Update(host), _) => format!("Update Gantry on {}", host.name),
            (WizardStart::Add, _) => format!("Set up {}", self.draft_name()),
        }
    }

    fn lede(&self) -> String {
        match self.step {
            Step::Host => "Pick a host from your ssh config or type user@host. Gantry installs itself there.".into(),
            Step::Check => format!("Looking at {}.", self.target.text().trim()),
            Step::Install => "Uploading this Gantry and checking that it runs.".into(),
            Step::Finish => match self.start {
                WizardStart::Update(_) => "The new Gantry is in place.".into(),
                _ => "Gantry is ready on the host.".into(),
            },
        }
    }

    fn render_host(&self, cx: &mut Context<Self>) -> Div {
        let theme = theme(cx).clone();
        let label = |text: &'static str| div().text_xs().text_color(rgb(theme.tokens.muted)).child(text);
        let focused = |field: Field| self.active == field;
        let target = field::render(
            &self.target,
            "user@host or an ssh alias",
            focused(Field::Target),
            &theme,
            field::on_press(cx, |this, offset, clicks, _, cx| {
                this.active = Field::Target;
                this.target.press(offset, clicks);
                cx.notify();
            }),
        );
        let name = field::render(
            &self.name,
            "Name in Gantry",
            focused(Field::Name),
            &theme,
            field::on_press(cx, |this, offset, clicks, _, cx| {
                this.active = Field::Name;
                this.name.press(offset, clicks);
                cx.notify();
            }),
        );
        let port = field::render(
            &self.port,
            "default",
            focused(Field::Port),
            &theme,
            field::on_press(cx, |this, offset, clicks, _, cx| {
                this.active = Field::Port;
                this.port.press(offset, clicks);
                cx.notify();
            }),
        );
        let command = field::render(
            &self.command,
            DEFAULT_REMOTE_COMMAND,
            focused(Field::Command),
            &theme,
            field::on_press(cx, |this, offset, clicks, _, cx| {
                this.active = Field::Command;
                this.command.press(offset, clicks);
                cx.notify();
            }),
        );
        let problem = |message: Option<String>| {
            message.map(|message| div().text_xs().text_color(rgb(theme.tokens.muted)).child(format!("⚠ {message}")))
        };
        let typed = !self.target.text().is_empty();
        let name_problem = typed.then(|| self.name_error()).flatten();
        let target_problem = typed.then(|| self.target_error()).flatten();
        let aliases = (!self.aliases.is_empty()).then(|| {
            div().flex().flex_col().gap(theme.ui(6.)).child(label("FROM ~/.ssh/CONFIG")).child(
                div().flex().flex_wrap().gap(theme.ui(6.)).children(self.aliases.iter().enumerate().map(
                    |(index, alias)| {
                        let chosen = alias.clone();
                        button(("ssh-wizard-alias", index), alias.clone(), false, true, &theme)
                            .h(theme.ui(26.))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.pick_alias(&chosen);
                                cx.notify();
                            }))
                    },
                )),
            )
        });
        div()
            .flex()
            .flex_col()
            .gap(theme.ui(10.))
            .px(theme.ui(22.))
            .pb(theme.ui(12.))
            .children(aliases)
            .child(label("HOST"))
            .child(text_box(&theme, target))
            .children(problem(target_problem))
            .child(label("NAME"))
            .child(text_box(&theme, name))
            .children(problem(name_problem))
            .child(label("SIGN IN"))
            .child(self.auth_choice(&theme, cx))
            .child(
                div()
                    .id("ssh-wizard-advanced")
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .child(if self.advanced { "▾ Advanced" } else { "▸ Advanced" })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.advanced = !this.advanced;
                        cx.notify();
                    })),
            )
            .when(self.advanced, |body| {
                body.child(label("PORT"))
                    .child(text_box(&theme, port))
                    .children(problem(self.port_error()))
                    .child(label("REMOTE COMMAND"))
                    .child(text_box(&theme, command))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.tokens.muted))
                            .child("Only a plain path (~/… or /…) can be installed to."),
                    )
            })
    }

    /// The sign-in method chips: Automatic, Password, SSH key.
    fn auth_choice(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let options = [(SshAuth::Automatic, "Automatic"), (SshAuth::Password, "Password"), (SshAuth::Key, "SSH key")];
        let hint = match self.auth {
            SshAuth::Automatic => "ssh decides: keys first, then a password.",
            SshAuth::Password => "Only a password. Keys are not tried.",
            SshAuth::Key => "Only keys (the agent and your default keys). No password.",
        };
        div()
            .flex()
            .flex_col()
            .gap(theme.ui(6.))
            .child(div().flex().gap(theme.ui(6.)).children(options.into_iter().enumerate().map(
                |(index, (auth, name))| {
                    let on = self.auth == auth;
                    div()
                        .id(("ssh-wizard-auth", index))
                        .cursor_pointer()
                        .px(theme.ui(10.))
                        .py(theme.ui(3.))
                        .rounded(px(RADIUS_SMALL))
                        .text_sm()
                        .border_1()
                        .border_color(rgb(if on { theme.tokens.accent } else { theme.tokens.line }))
                        .text_color(rgb(if on { theme.tokens.accent } else { theme.tokens.muted }))
                        .child(name)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.auth = auth;
                            cx.notify();
                        }))
                },
            )))
            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(hint))
    }

    fn line(&self, theme: &Theme, ok: bool, text: String) -> Div {
        div()
            .flex()
            .items_start()
            .gap(theme.ui(8.))
            .child(div().flex_none().w(theme.ui(14.)).text_color(rgb(theme.tokens.accent)).child(if ok {
                "✓"
            } else {
                "•"
            }))
            .child(div().flex_1().min_w_0().child(text))
    }

    fn render_check(&self, cx: &mut Context<Self>) -> Div {
        let theme = theme(cx).clone();
        let mut body = div().flex().flex_col().gap(theme.ui(8.)).px(theme.ui(22.)).pb(theme.ui(12.));
        if let Some(check) = &self.check {
            let os = if check.supported { "Linux x86_64" } else { "not Linux x86_64" };
            body = body.child(self.line(&theme, check.supported, format!("System: {os}")));
            if !check.supported {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme.tokens.muted))
                        .child("Gantry installs on Linux x86_64 hosts only. Nothing was uploaded."),
                );
            } else {
                let local = protocol::build::VERSION;
                let gantry = match &check.gantry {
                    GantryOnRemote::Missing => "Gantry: not installed".to_string(),
                    GantryOnRemote::Older(version) => format!("Gantry: {version} (this one is {local})"),
                    GantryOnRemote::Same(version) => format!("Gantry: {version}, the same as this one"),
                    GantryOnRemote::Newer(version) => format!("Gantry: {version}, newer than this one ({local})"),
                };
                body = body.child(self.line(&theme, !matches!(check.gantry, GantryOnRemote::Missing), gantry));
                let linger = match check.linger {
                    Linger::On => "Lingering: on, sessions survive logout",
                    Linger::Off => "Lingering: off, sessions end with the last login",
                    Linger::Unknown => "Lingering: unknown (no systemd)",
                };
                body = body.child(self.line(&theme, check.linger == Linger::On, linger.to_string()));
                if !matches!(check.gantry, GantryOnRemote::Same(_))
                    && let Some(reason) = self.install_block()
                {
                    body = body.child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(format!("⚠ {reason}")));
                }
            }
        }
        body
    }

    fn render_install(&self, cx: &mut Context<Self>) -> Div {
        let theme = theme(cx).clone();
        let fraction = if self.total == 0 { 0.0 } else { (self.sent as f32 / self.total as f32).min(1.0) };
        let mut body = div().flex().flex_col().gap(theme.ui(8.)).px(theme.ui(22.)).pb(theme.ui(12.));
        body = body
            .child(div().h(theme.ui(8.)).w_full().rounded(px(RADIUS_SMALL)).bg(rgb(theme.tokens.ground)).child(
                div().h_full().w(gpui::relative(fraction)).rounded(px(RADIUS_SMALL)).bg(rgb(theme.tokens.accent)),
            ))
            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(format!(
                "{} of {}",
                megabytes(self.sent),
                megabytes(self.total)
            )));
        if let Some(line) = &self.verified {
            body = body.child(self.line(&theme, true, format!("The host runs: {line}")));
        }
        body
    }

    fn render_finish(&self, cx: &mut Context<Self>) -> Div {
        let theme = theme(cx).clone();
        let mut body = div().flex().flex_col().gap(theme.ui(10.)).px(theme.ui(22.)).pb(theme.ui(12.));
        if let Some(line) = &self.verified {
            body = body.child(self.line(&theme, true, format!("The host runs: {line}")));
        }
        if let WizardStart::Update(host) = &self.start {
            return body.child(div().child(format!("Restarting ends any sessions running on {}.", host.name)));
        }
        let off = self.check.as_ref().is_some_and(|check| check.linger == Linger::Off);
        if off {
            body = body.child(
                div()
                    .id("ssh-wizard-linger")
                    .flex()
                    .items_center()
                    .gap(theme.ui(8.))
                    .cursor_pointer()
                    .child(check_box(&theme, self.linger_tick))
                    .child("Keep sessions running after logout (loginctl enable-linger)")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.linger_tick = !this.linger_tick;
                        cx.notify();
                    })),
            );
        }
        if let Some(note) = &self.linger_note {
            body = body.child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(format!(
                "⚠ Lingering was not turned on: {note}. Run on the host: loginctl enable-linger $(id -un)"
            )));
        }
        body
    }

    fn primary_label(&self) -> Option<String> {
        if self.error.is_some() {
            return Some("Retry".into());
        }
        if self.working.is_some() {
            return None;
        }
        Some(match self.step {
            Step::Host => "Next".into(),
            Step::Check => match self.check.as_ref().map(|check| &check.gantry)? {
                GantryOnRemote::Missing => "Install Gantry".into(),
                GantryOnRemote::Older(_) => "Update Gantry".into(),
                GantryOnRemote::Newer(_) => "Replace with this version".into(),
                GantryOnRemote::Same(_) => "Next".into(),
            },
            Step::Install => "Continue".into(),
            Step::Finish => match self.start {
                WizardStart::Add if self.linger_note.is_some() => "Save and connect anyway".into(),
                WizardStart::Add => "Save and connect".into(),
                WizardStart::Install(_) if self.linger_note.is_some() => "Connect anyway".into(),
                WizardStart::Install(_) => "Connect".into(),
                WizardStart::Update(_) => "Restart".into(),
            },
        })
    }

    fn primary_enabled(&self) -> bool {
        if self.error.is_some() {
            return true;
        }
        match self.step {
            Step::Host => self.host_ready(),
            Step::Check => self.check.as_ref().is_some_and(|check| {
                check.supported && (matches!(check.gantry, GantryOnRemote::Same(_)) || self.install_block().is_none())
            }),
            Step::Install => self.verified.is_some(),
            Step::Finish => true,
        }
    }
}

impl Render for SshWizard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let body = match self.step {
            Step::Host => self.render_host(cx),
            Step::Check => self.render_check(cx),
            Step::Install => self.render_install(cx),
            Step::Finish => self.render_finish(cx),
        };
        let working = self
            .working
            .map(|label| div().px(theme.ui(22.)).pb(theme.ui(10.)).text_color(rgb(theme.tokens.muted)).child(label));
        let error = self.error.clone().map(|error| {
            div().px(theme.ui(22.)).pb(theme.ui(10.)).child(
                div()
                    .p(theme.ui(10.))
                    .rounded(px(RADIUS_SMALL))
                    .bg(rgb(theme.tokens.ground))
                    .font_family(FONT_FAMILY)
                    .text_xs()
                    .child(error),
            )
        });
        let primary = self.primary_label();
        let enabled = self.primary_enabled();
        let can_back = self.working.is_none()
            && match self.step {
                Step::Host => false,
                Step::Check => !self.fixed() || self.error.is_some(),
                Step::Install => true,
                Step::Finish => !matches!(self.start, WizardStart::Update(_)) || self.error.is_some(),
            };
        let footer = div()
            .flex()
            .items_center()
            .gap(theme.ui(10.))
            .px(theme.ui(22.))
            .py(theme.ui(14.))
            .border_t_1()
            .border_color(rgb(theme.tokens.line))
            .child(
                button("ssh-wizard-cancel", "Cancel", false, true, &theme)
                    .text_color(rgb(theme.tokens.muted))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close(cx))),
            )
            .child(div().flex_1())
            .when(can_back, |footer| {
                footer.child(
                    button("ssh-wizard-back", "Back", false, true, &theme)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.back(cx))),
                )
            })
            .children(primary.map(|label| {
                button("ssh-wizard-next", label, true, enabled, &theme).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        if enabled {
                            this.advance(cx);
                        }
                    },
                ))
            }));
        let card = div()
            .key_context("SshWizard")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .w(theme.ui(560.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.surface))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS_LARGE))
            .overflow_hidden()
            .shadow_lg()
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.ui(4.))
                    .px(theme.ui(22.))
                    .pt(theme.ui(20.))
                    .pb(theme.ui(12.))
                    .child(div().text_size(theme.ui(18.)).font_weight(FontWeight::SEMIBOLD).child(self.title()))
                    .child(div().text_color(rgb(theme.tokens.muted)).child(self.lede())),
            )
            .child(body)
            .children(working)
            .children(error)
            .child(footer);
        fade_in(card, "fade-ssh-wizard", FadeKind::Card, cx.global::<SettingsStore>().settings().animate_ui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The upload needs a Linux x86_64 Gantry; elsewhere `upload_block` refuses every command.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn upload_block_refuses_a_command_with_shell_syntax() {
        assert!(upload_block("env X=1 gantryd").is_some());
        assert!(upload_block("$HOME/gantryd").is_some());
    }

    // As above: Linux x86_64 only.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn upload_block_accepts_a_plain_path() {
        assert_eq!(upload_block("~/.local/bin/gantryd"), None);
    }

    #[test]
    fn the_suggested_name_is_the_alias_or_the_host_part() {
        assert_eq!(suggested_name("mainpc"), "mainpc");
        assert_eq!(suggested_name("ido@build.example.com"), "build");
        assert_eq!(suggested_name("ido@10.0.0.12"), "10.0.0.12");
        assert_eq!(suggested_name("10.0.0.12"), "10.0.0.12");
        assert_eq!(suggested_name(""), "");
    }

    #[test]
    fn includes_expand_home_globs_and_relative_paths() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(ssh.join("config.d/a.conf"), "Host a\n").unwrap();
        std::fs::write(ssh.join("config.d/b.conf"), "Host b\n").unwrap();
        std::fs::write(ssh.join("config.d/skip.txt"), "Host skipped\n").unwrap();
        std::fs::write(ssh.join("extra"), "Host c\n").unwrap();
        std::fs::write(home.path().join("elsewhere"), "Host d\n").unwrap();
        let read = |name: &str| include_text(name, &ssh, home.path());
        assert_eq!(read("config.d/*.conf").as_deref(), Some("Host a\nHost b\n"));
        assert_eq!(read("extra").as_deref(), Some("Host c\n"));
        assert_eq!(read("~/elsewhere").as_deref(), Some("Host d\n"));
        assert_eq!(read("missing"), None);
        assert_eq!(read("nodir/*").as_deref(), None);
    }

    #[test]
    fn gantryd_is_looked_for_beside_the_file_gantry_started_from() {
        let beside = |exe: &str| headless_beside(std::path::Path::new(exe));
        assert_eq!(beside("/home/me/.local/bin/gantry"), std::path::Path::new("/home/me/.local/bin/gantryd"));
        assert_eq!(beside("/home/me/.local/bin/gantry (deleted)"), std::path::Path::new("/home/me/.local/bin/gantryd"));
    }

    #[test]
    fn a_failed_step_says_why() {
        assert_eq!(describe(&FailKind::Other { reason: "Connection refused".into() }), "Connection refused");
        assert!(describe(&FailKind::NeedsInput).contains("cancelled"));
    }
}
