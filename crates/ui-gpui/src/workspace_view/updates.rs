//! Click to update: the one update run, what this Gantry can install, and the `UpdateRow` global.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use app_core::self_update::{self, Offer, Platform, Run, auto_start, install_target, origin_of, pick};
use gpui::Context;

use super::WorkspaceView;
use crate::release_check;
use crate::self_update::{Failure, UpdateRow};
use crate::settings_store::SettingsStore;

impl WorkspaceView {
    /// What the release card offers for the newest release: nothing without one, else whether and how this Gantry can
    /// install it.
    pub(super) fn update_offer(&self) -> Offer {
        let Some(online) = self.online_notice() else { return Offer::None };
        let Some(platform) = Platform::current() else { return Offer::NotHere(None) };
        // A debug build only updates from the test server (`GANTRY_UPDATE_URL`), whose address its files may use.
        let test_server = cfg!(debug_assertions) && std::env::var("GANTRY_UPDATE_URL").is_ok_and(|url| !url.is_empty());
        if cfg!(debug_assertions) && !test_server {
            return Offer::NotHere(None);
        }
        let endpoint = release_check::endpoint();
        let extra_origin = if test_server { origin_of(&endpoint) } else { None };
        // A binary an install replaced reads as "<path> (deleted)": its path is the new file's.
        let exe = std::env::current_exe().map(|exe| {
            let text = exe.to_string_lossy();
            text.strip_suffix(" (deleted)").map_or_else(|| exe.clone(), PathBuf::from)
        });
        let (Some(exe), Some(home)) = (
            exe.and_then(|exe| exe.canonicalize()).ok(),
            std::env::home_dir().and_then(|home| home.canonicalize().ok()),
        ) else {
            return Offer::NotHere(None);
        };
        let release = self.releases.iter().find(|release| release.version == online.latest);
        self_update::offer(
            install_target(platform, &exe, &home, writable),
            release.and_then(|release| pick(release, platform, extra_origin)),
        )
    }

    /// Downloads and installs the newest release, unless a run is already going or it cannot be installed here.
    pub(super) fn start_self_update(&mut self, cx: &mut Context<Self>) {
        if self.self_update.busy() {
            return;
        }
        let (Some(online), Offer::Install(target, pick)) = (self.online_notice(), self.update_offer()) else {
            self.publish_update_row(cx);
            return;
        };
        let version = online.latest;
        self.self_update_generation += 1;
        let generation = self.self_update_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.self_update_cancel = Some(cancel.clone());
        self.self_update = Run::Downloading { version, percent: 0 };
        self.publish_update_row(cx);
        cx.notify();
        let (sender, progress) = async_channel::unbounded::<u8>();
        let work = cx.background_executor().spawn(async move {
            crate::self_update::run(&pick, &target, version, &cancel, &mut |percent| {
                let _ = sender.try_send(percent);
            })
        });
        cx.spawn(async move |this, cx| {
            // The channel closes when the run returns (its sender is dropped with it).
            while let Ok(percent) = progress.recv().await {
                if this.update(cx, |view, cx| view.self_update_progress(generation, percent, cx)).is_err() {
                    return;
                }
            }
            let result = work.await;
            let _ = this.update(cx, |view, cx| view.self_update_finished(generation, result, cx));
        })
        .detach();
    }

    /// Starts the update by itself when `autoInstallUpdates` is on and the newest release can be installed here; at
    /// most once per version in this window, so a failed or cancelled run waits for a click.
    pub(super) fn auto_update(&mut self, cx: &mut Context<Self>) {
        let enabled = cx.global::<SettingsStore>().settings().auto_install_updates;
        let latest = self.online_notice().map(|online| online.latest);
        if auto_start(enabled, &self.self_update, latest, self.auto_update_tried, || self.update_offer()) {
            self.auto_update_tried = latest;
            self.start_self_update(cx);
        }
    }

    /// Stops the download: the run is Idle at once, even while a stalled connection keeps the engine waiting, and
    /// whatever that run answers later is ignored (it never installs once cancelled).
    pub(super) fn cancel_self_update(&mut self, cx: &mut Context<Self>) {
        if let (Run::Downloading { .. }, Some(cancel)) = (&self.self_update, self.self_update_cancel.take()) {
            cancel.store(true, Ordering::Relaxed);
            self.self_update_generation += 1;
            self.self_update = Run::Idle;
            self.publish_update_row(cx);
            cx.notify();
        }
    }

    /// A new download percentage of run `generation`; the whole file is in at 100, then it is checked and installed.
    fn self_update_progress(&mut self, generation: u64, percent: u8, cx: &mut Context<Self>) {
        if generation != self.self_update_generation {
            return;
        }
        let Run::Downloading { version, .. } = self.self_update else { return };
        self.self_update =
            if percent >= 100 { Run::Installing { version } } else { Run::Downloading { version, percent } };
        self.publish_update_row(cx);
        cx.notify();
    }

    /// The answer of run `generation`, ignored when that run was cancelled. A good install is the installed version,
    /// so the install notice and its Restart and update take over (on Linux the binary watcher reports the same
    /// version), and an open release card becomes the update card.
    fn self_update_finished(
        &mut self,
        generation: u64,
        result: Result<app_core::version::Version, Failure>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.self_update_generation {
            return;
        }
        self.self_update_cancel = None;
        self.self_update = match result {
            Ok(version) => {
                self.installed = Some(version);
                if self.release_card.is_some() {
                    self.update_card = self.release_card.take();
                    self.update_error = None;
                }
                Run::Done(version)
            }
            Err(Failure::Cancelled) => Run::Idle,
            Err(Failure::At(step, detail)) => Run::Failed { step, detail },
        };
        self.publish_update_row(cx);
        cx.notify();
    }

    /// Publishes what the release card and Settings show, when it changed.
    pub(super) fn publish_update_row(&mut self, cx: &mut Context<Self>) {
        let row = UpdateRow {
            latest: self.releases.iter().map(|release| release.version).max(),
            // A running update keeps the offer it started from: no probe file in the install folder on every percent.
            offer: if self.self_update.busy() { cx.global::<UpdateRow>().offer.clone() } else { self.update_offer() },
            run: self.self_update.clone(),
            page: self
                .online_notice()
                .and_then(|online| online.newer.first().map(|release| release.url.clone()))
                .filter(|url| !url.is_empty()),
            restart: self.update_notice(),
            restart_error: self.update_error.clone(),
        };
        if *cx.global::<UpdateRow>() != row {
            cx.set_global(row);
        }
    }
}

/// Whether a file can be made (and removed) in `dir`.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".gantry-update-probe-{}", std::process::id()));
    let made = std::fs::File::create(&probe).is_ok();
    made && std::fs::remove_file(&probe).is_ok()
}
