//! The Alerts section: what raises an alert, and how.

use app_core::session_tree::Tone;
use app_core::settings::{AlertSettings, AlertTone, EventAlerts, Settings};
use gpui::{Context, Div, MouseButton, Window, canvas, div, px, rgb};

use super::*;
use crate::alerts;
use crate::theme::{RADIUS_SMALL, Theme};

/// An event that can alert, a row of the Events group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Event {
    NeedsInput,
    Finished,
}

impl Event {
    const ALL: [Event; 2] = [Event::NeedsInput, Event::Finished];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Event::NeedsInput => "Needs input",
            Event::Finished => "Finished",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Event::NeedsInput => "Claude asks for permission or a reply.",
            Event::Finished => "Busy, then idle.",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Event::NeedsInput => Tone::NeedsInput,
            Event::Finished => Tone::Done,
        }
    }

    fn tones(self) -> [AlertTone; 4] {
        match self {
            Event::NeedsInput => AlertTone::NEEDS_INPUT,
            Event::Finished => AlertTone::FINISHED,
        }
    }

    pub(super) fn alerts(self, alerts: &mut AlertSettings) -> &mut EventAlerts {
        match self {
            Event::NeedsInput => &mut alerts.needs_input,
            Event::Finished => &mut alerts.finished,
        }
    }
}

const TONES: [(AlertTone, &str); 8] = [
    (AlertTone::ClassicBlip, "Classic blip"),
    (AlertTone::GentleRise, "Gentle rise"),
    (AlertTone::SoftBloop, "Soft bloop"),
    (AlertTone::SoftDrop, "Soft drop"),
    (AlertTone::FallingBlip, "Falling blip"),
    (AlertTone::SoftFall, "Soft fall"),
    (AlertTone::SoftFlutter, "Soft flutter"),
    (AlertTone::WarmHum, "Warm hum"),
];

fn tone_label(tone: AlertTone) -> &'static str {
    TONES.iter().find(|(candidate, _)| *candidate == tone).map_or("", |(_, label)| label)
}

const FINISHED_AFTER: Stepped = Stepped {
    name: "finished-after",
    get: |s| s.alerts.finished_after_seconds as f32,
    set: |s, value| s.alerts.finished_after_seconds = value as u32,
    delta: 1.0,
    min: 0.0,
    max: 600.0,
    unit: "s",
};

const READ_AFTER: Stepped = Stepped {
    name: "read-after",
    get: |s| s.alerts.read_after_seconds as f32,
    set: |s, value| s.alerts.read_after_seconds = value as u32,
    delta: 1.0,
    min: 0.0,
    max: 60.0,
    unit: "s",
};

/// The volume (0 to 1, whole percents) under pointer position `x` on a slider spanning `left..left + width`.
fn volume_at(x: f32, left: f32, width: f32) -> f32 {
    if width <= 0.0 {
        return 0.0;
    }
    (((x - left) / width).clamp(0.0, 1.0) * 100.0).round() / 100.0
}

impl SettingsView {
    pub(super) fn alerts(
        &mut self,
        theme: &Theme,
        settings: &Settings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let events = self.events_group(theme, settings, window, cx);
        let visibility = self.visibility_group(theme, settings, window, cx);
        let sound = self.sound_group(theme, settings, window, cx);
        div().flex().flex_col().gap(theme.ui(26.)).child(events).child(visibility).child(sound)
    }

    /// One row per event with its channel switches, tone and Play, then the "Finished after" threshold.
    fn events_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut copy = settings.alerts;
        let mut rows: Vec<Div> = Event::ALL
            .into_iter()
            .map(|event| {
                let channels = *event.alerts(&mut copy);
                let index = event.index();
                let labelled = |label: &'static str, switch: Stateful<Div>| {
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(theme.ui(4.))
                        .child(muted(theme, label))
                        .child(switch)
                };
                let badge = self.switch(
                    "badge",
                    index,
                    channels.badge,
                    window,
                    cx,
                    change(move |s| {
                        let e = event.alerts(&mut s.alerts);
                        e.badge = !e.badge;
                    }),
                );
                let notification = self.switch(
                    "notification",
                    index,
                    channels.notification,
                    window,
                    cx,
                    change(move |s| {
                        let e = event.alerts(&mut s.alerts);
                        e.notification = !e.notification;
                    }),
                );
                let sound = self.switch(
                    "sound",
                    index,
                    channels.sound,
                    window,
                    cx,
                    change(move |s| {
                        let e = event.alerts(&mut s.alerts);
                        e.sound = !e.sound;
                    }),
                );
                let options = event
                    .tones()
                    .into_iter()
                    .map(|tone| {
                        (
                            tone_label(tone).to_string(),
                            tone == channels.tone,
                            change(move |s| event.alerts(&mut s.alerts).tone = tone),
                        )
                    })
                    .collect();
                let picker = self.dropdown(
                    "tone",
                    index,
                    Popover::Tone(event),
                    tone_label(channels.tone).into(),
                    theme.ui(170.),
                    options,
                    window,
                    cx,
                );
                let tone = channels.tone;
                let play = handler(move |_, _, cx| {
                    let volume = cx.global::<SettingsStore>().settings().alerts.volume;
                    alerts::play(tone, volume);
                });
                let play = self.button("play", index, "▶ Play", window, cx, play);
                let control = div()
                    .flex()
                    .items_end()
                    .gap(theme.ui(14.))
                    .child(labelled("Badge", badge))
                    .child(labelled("Notification", notification))
                    .child(labelled("Sound", sound))
                    .child(picker)
                    .child(play);
                let reset = self.reset_slot(
                    "reset-event",
                    index,
                    settings,
                    move |s| {
                        let mut alerts = s.alerts;
                        *event.alerts(&mut alerts)
                    },
                    move |s, value| *event.alerts(&mut s.alerts) = value,
                    window,
                    cx,
                );
                let label = div()
                    .flex()
                    .items_center()
                    .gap(theme.ui(10.))
                    .child(div().flex_none().size(theme.ui(10.)).rounded_full().bg(rgb(theme.tone(event.tone()).dot)))
                    .child(row_label(event.label(), muted(theme, event.description())));
                row_shell(theme, label, control, reset)
            })
            .collect();
        let after = self.stepper(&FINISHED_AFTER, settings, window, cx);
        let after_reset = self.reset_slot(
            "reset-finished-after",
            0,
            settings,
            |s| s.alerts.finished_after_seconds,
            |s, v| s.alerts.finished_after_seconds = v,
            window,
            cx,
        );
        rows.push(row(
            theme,
            "Finished after",
            "How long a session must be busy before going idle counts as finished.",
            after,
            after_reset,
        ));
        if cfg!(target_os = "macos") {
            rows.push(stacked_row(
                theme,
                None,
                muted(
                    theme,
                    "Notifications need your permission on macOS: allow Gantry in System Settings > Notifications. Gantry is listed there after its first notification.",
                ),
            ));
        }
        group(theme, "Events", Some("Badge, notification and sound for each event."), rows)
    }

    fn visibility_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let alerts = settings.alerts;
        let visible = self.switch(
            "when",
            0,
            alerts.when_visible,
            window,
            cx,
            change(|s| s.alerts.when_visible = !s.alerts.when_visible),
        );
        let visible_reset = self.reset_slot(
            "reset-when",
            0,
            settings,
            |s| s.alerts.when_visible,
            |s, v| s.alerts.when_visible = v,
            window,
            cx,
        );
        let background = self.switch(
            "when",
            1,
            alerts.only_when_unfocused,
            window,
            cx,
            change(|s| s.alerts.only_when_unfocused = !s.alerts.only_when_unfocused),
        );
        let background_reset = self.reset_slot(
            "reset-when",
            1,
            settings,
            |s| s.alerts.only_when_unfocused,
            |s, v| s.alerts.only_when_unfocused = v,
            window,
            cx,
        );
        let read_after = self.stepper(&READ_AFTER, settings, window, cx);
        let read_after_reset = self.reset_slot(
            "reset-read-after",
            0,
            settings,
            |s| s.alerts.read_after_seconds,
            |s, v| s.alerts.read_after_seconds = v,
            window,
            cx,
        );
        group(
            theme,
            "Visibility",
            None,
            vec![
                row(
                    theme,
                    "Alert for visible panes",
                    "Off: a pane you can see in the active window raises nothing.",
                    visible,
                    visible_reset,
                ),
                row(
                    theme,
                    "Notify only in the background",
                    "Notifications wait until Gantry is not focused. Badges and sounds still follow Events.",
                    background,
                    background_reset,
                ),
                row(
                    theme,
                    "Mark as read after",
                    "How long a pane with an alert must be on screen before its badge clears. 0 clears it at once.",
                    read_after,
                    read_after_reset,
                ),
            ],
        )
    }

    fn sound_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let alerts = settings.alerts;
        let sounds = self.switch(
            "play-sounds",
            0,
            alerts.play_sounds,
            window,
            cx,
            change(|s| s.alerts.play_sounds = !s.alerts.play_sounds),
        );
        let sounds_reset = self.reset_slot(
            "reset-play-sounds",
            0,
            settings,
            |s| s.alerts.play_sounds,
            |s, v| s.alerts.play_sounds = v,
            window,
            cx,
        );
        let slider = self.volume_slider(theme, alerts.volume, window, cx);
        let volume_reset =
            self.reset_slot("reset-volume", 0, settings, |s| s.alerts.volume, |s, v| s.alerts.volume = v, window, cx);
        group(
            theme,
            "Sound",
            None,
            vec![
                row(theme, "Play sounds", "Off mutes every alert sound.", sounds, sounds_reset),
                row(theme, "Volume", "All sounds share one volume.", slider, volume_reset),
                stacked_row(
                    theme,
                    None,
                    muted(
                        theme,
                        "Claude Code also sends its own notifications for the same events, so you may get two. Turn them off in Claude Code's settings if you prefer Gantry's.",
                    ),
                ),
            ],
        )
    }

    /// A draggable slider (written on release; arrow keys write at once) and the percentage.
    fn volume_slider(&mut self, theme: &Theme, volume: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let shown = self.volume_drag.unwrap_or(volume);
        let handle = self.stop("volume", 0, cx);
        let focused = handle.is_focused(window);
        let dragging = self.volume_drag.is_some();
        let entity = cx.entity();
        let bounds_cell = self.slider_bounds.clone();
        let knob = theme.ui(16.);
        let rail = div()
            .relative()
            .mx(knob / 2.)
            .w_full()
            .h(theme.ui(6.))
            .rounded_full()
            .bg(rgb(theme.tokens.line))
            .child(div().h_full().w(relative(shown)).rounded_full().bg(rgb(theme.tokens.accent)))
            .child(
                div()
                    .absolute()
                    .top(theme.ui(-5.))
                    .left(relative(shown))
                    .ml(-knob / 2.)
                    .size(knob)
                    .rounded_full()
                    .bg(rgb(theme.tokens.accent)),
            )
            .child(
                canvas(
                    |bounds, _, _| bounds,
                    move |bounds, _, window, _| {
                        bounds_cell.set(bounds);
                        if !dragging {
                            return;
                        }
                        let moving = entity.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                moving.update(cx, |this, cx| {
                                    this.volume_drag = Some(volume_at(
                                        f32::from(event.position.x),
                                        f32::from(bounds.left()),
                                        f32::from(bounds.size.width),
                                    ));
                                    cx.notify();
                                });
                            }
                        });
                        let released = entity.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                released.update(cx, |this, cx| {
                                    if let Some(volume) = this.volume_drag.take() {
                                        SettingsStore::update(cx, |s| s.alerts.volume = volume);
                                    }
                                    cx.notify();
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        let slider = div()
            .id("volume")
            .track_focus(&handle)
            .tab_stop(true)
            .flex_1()
            .min_w_0()
            .h(theme.ui(28.))
            .flex()
            .items_center()
            .cursor_pointer()
            .rounded(px(RADIUS_SMALL))
            .border_1()
            .border_color(if focused { rgb(theme.tokens.text) } else { rgba(0) })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let bounds = this.slider_bounds.get();
                    this.volume_drag = Some(volume_at(
                        f32::from(event.position.x),
                        f32::from(bounds.left()),
                        f32::from(bounds.size.width),
                    ));
                    this.stop("volume", 0, cx).focus(window, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                let delta = match event.keystroke.key.as_str() {
                    "left" | "down" => -5.0,
                    "right" | "up" => 5.0,
                    _ => return,
                };
                let percent = (cx.global::<SettingsStore>().settings().alerts.volume * 100.0).round();
                let volume = step(percent, delta, 0.0, 100.0) / 100.0;
                SettingsStore::update(cx, |s| s.alerts.volume = volume);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(rail);
        div().w(theme.ui(240.)).flex_none().flex().items_center().gap(theme.ui(12.)).child(slider).child(
            div()
                .flex_none()
                .w(theme.ui(44.))
                .text_color(rgb(theme.tokens.muted))
                .child(format!("{:.0}%", shown * 100.0)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slider_value_follows_the_pointer_in_whole_percents() {
        assert_eq!(volume_at(150.0, 100.0, 200.0), 0.25);
        assert_eq!(volume_at(50.0, 100.0, 200.0), 0.0);
        assert_eq!(volume_at(900.0, 100.0, 200.0), 1.0);
        assert_eq!(volume_at(100.0, 100.0, 0.0), 0.0);
    }
}
