use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{KeyConfig, TimeConfig};
use crate::env_file::{ButtonMode, PinMap};
use crate::gpio_cdev::{GpioEdgeKind, GpioLine};
use crate::oled::OledSignal;
use crate::shutdown;

const BUTTON_CONSUMER: &str = "hat_button";
const BUTTON_DEBOUNCE: Duration = Duration::from_millis(10);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(200);
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const OUTPUT_POLL_DEBOUNCE: Duration = Duration::from_millis(40);

#[derive(Debug)]
pub struct ButtonRuntime {
    stop: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ButtonRuntime {
    pub fn start(
        pin_map: &PinMap,
        key_config: KeyConfig,
        time_config: TimeConfig,
        fan_enabled: Arc<AtomicBool>,
        oled_signal: Arc<OledSignal>,
    ) -> io::Result<Option<Self>> {
        let Some(button_chip) = pin_map.button_chip.as_deref() else {
            return Ok(None);
        };
        let Some(button_line) = pin_map.button_line else {
            return Ok(None);
        };

        let mode = pin_map.button_mode;
        let line = match mode {
            ButtonMode::Edge => GpioLine::request_input_edges(
                button_chip,
                button_line,
                BUTTON_DEBOUNCE,
                BUTTON_CONSUMER,
            )?,
            // The Penta HAT reads its button from an output held high, exactly
            // as the original Radxa implementation does.
            ButtonMode::OutputPoll => {
                GpioLine::request_output(button_chip, button_line, true, BUTTON_CONSUMER)?
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));

        let thread = {
            let stop = Arc::clone(&stop);
            let last_error = Arc::clone(&last_error);

            thread::spawn(move || {
                run_button_loop(
                    line,
                    mode,
                    key_config,
                    time_config,
                    fan_enabled,
                    oled_signal,
                    stop,
                    last_error,
                );
            })
        };

        Ok(Some(Self {
            stop,
            last_error,
            thread: Some(thread),
        }))
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }
}

impl Drop for ButtonRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_button_loop(
    mut line: GpioLine,
    mode: ButtonMode,
    key_config: KeyConfig,
    time_config: TimeConfig,
    fan_enabled: Arc<AtomicBool>,
    oled_signal: Arc<OledSignal>,
    stop: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
) {
    let mut classifier = ButtonClassifier::new(time_config);
    let mut output_classifier = OutputPollClassifier::new(time_config);

    while !stop.load(Ordering::SeqCst) && !shutdown::requested() {
        if mode == ButtonMode::OutputPoll {
            match line.read_value() {
                Ok(level) => {
                    if let Some(gesture) = output_classifier.handle_level(level, Instant::now())
                        && let Err(err) =
                            run_button_action(gesture, &key_config, &fan_enabled, &oled_signal)
                    {
                        store_error(&last_error, err);
                        break;
                    }
                }
                Err(err) => {
                    store_error(&last_error, err.to_string());
                    break;
                }
            }
            thread::sleep(OUTPUT_POLL_INTERVAL);
            continue;
        }

        let now = Instant::now();
        let timeout = classifier
            .next_wait(now)
            .unwrap_or(SHUTDOWN_POLL_INTERVAL)
            .min(SHUTDOWN_POLL_INTERVAL);

        match line.read_edge_event_timeout(timeout) {
            Ok(Some(event)) => {
                let edge = match event.kind {
                    GpioEdgeKind::Falling => ButtonEdge::Pressed,
                    GpioEdgeKind::Rising => ButtonEdge::Released,
                };

                if let Some(gesture) = classifier.handle_edge(edge, Instant::now())
                    && let Err(err) =
                        run_button_action(gesture, &key_config, &fan_enabled, &oled_signal)
                {
                    store_error(&last_error, err);
                    break;
                }
            }
            Ok(None) => {
                if let Some(gesture) = classifier.handle_timeout(Instant::now())
                    && let Err(err) =
                        run_button_action(gesture, &key_config, &fan_enabled, &oled_signal)
                {
                    store_error(&last_error, err);
                    break;
                }
            }
            Err(err) => {
                store_error(&last_error, err.to_string());
                break;
            }
        }
    }
}

#[derive(Debug)]
struct OutputPollClassifier {
    classifier: ButtonClassifier,
    stable_level: Option<bool>,
    candidate_level: Option<bool>,
    candidate_since: Option<Instant>,
}

impl OutputPollClassifier {
    fn new(time_config: TimeConfig) -> Self {
        Self {
            classifier: ButtonClassifier::new(time_config),
            stable_level: None,
            candidate_level: None,
            candidate_since: None,
        }
    }

    fn handle_level(&mut self, level: bool, now: Instant) -> Option<ButtonGesture> {
        let Some(stable_level) = self.stable_level else {
            self.stable_level = Some(level);
            return None;
        };

        if level == stable_level {
            self.candidate_level = None;
            self.candidate_since = None;
        } else if self.candidate_level != Some(level) {
            self.candidate_level = Some(level);
            self.candidate_since = Some(now);
        } else if self
            .candidate_since
            .is_some_and(|since| now.duration_since(since) >= OUTPUT_POLL_DEBOUNCE)
        {
            self.stable_level = Some(level);
            self.candidate_level = None;
            self.candidate_since = None;

            let edge = if level {
                ButtonEdge::Released
            } else {
                ButtonEdge::Pressed
            };
            if let Some(gesture) = self.classifier.handle_edge(edge, now) {
                return Some(gesture);
            }
        }

        self.classifier.handle_timeout(now)
    }
}

fn store_error(last_error: &Mutex<Option<String>>, error: String) {
    if let Ok(mut last_error) = last_error.lock() {
        *last_error = Some(error);
    }
}

fn run_button_action(
    gesture: ButtonGesture,
    key_config: &KeyConfig,
    fan_enabled: &AtomicBool,
    oled_signal: &OledSignal,
) -> Result<(), String> {
    let action = action_for_gesture(gesture, key_config);

    match action {
        ButtonAction::None => {}
        ButtonAction::Slider => {
            oled_signal.request();
            eprintln!("button: OLED next page requested");
        }
        ButtonAction::Switch => {
            let enabled = !fan_enabled.load(Ordering::SeqCst);
            fan_enabled.store(enabled, Ordering::SeqCst);
            eprintln!(
                "button: fan switch toggled {}",
                if enabled { "on" } else { "off" }
            );
        }
        ButtonAction::Reboot => run_system_command("reboot")?,
        ButtonAction::Poweroff => run_system_command("poweroff")?,
    }

    Ok(())
}

fn run_system_command(command: &str) -> Result<(), String> {
    let status = Command::new(command)
        .status()
        .map_err(|err| format!("failed to run {command}: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{command} exited with {status}"))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ButtonGesture {
    Click,
    Twice,
    Press,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ButtonAction {
    None,
    Slider,
    Switch,
    Reboot,
    Poweroff,
}

fn action_for_gesture(gesture: ButtonGesture, key_config: &KeyConfig) -> ButtonAction {
    let action = match gesture {
        ButtonGesture::Click => &key_config.click,
        ButtonGesture::Twice => &key_config.twice,
        ButtonGesture::Press => &key_config.press,
    };

    parse_action(action)
}

fn parse_action(action: &str) -> ButtonAction {
    match action.trim().to_ascii_lowercase().as_str() {
        "slider" => ButtonAction::Slider,
        "switch" => ButtonAction::Switch,
        "reboot" => ButtonAction::Reboot,
        "poweroff" => ButtonAction::Poweroff,
        "none" | "" => ButtonAction::None,
        unknown => {
            eprintln!("button: unknown action {unknown:?}; ignoring");
            ButtonAction::None
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ButtonEdge {
    Pressed,
    Released,
}

#[derive(Debug)]
pub struct ButtonClassifier {
    double_click: Duration,
    long_press: Duration,
    click_count: u8,
    wait_until: Option<Instant>,
    ignore_release: bool,
}

impl ButtonClassifier {
    pub fn new(time_config: TimeConfig) -> Self {
        Self {
            double_click: duration_from_seconds(time_config.twice),
            long_press: duration_from_seconds(time_config.press),
            click_count: 0,
            wait_until: None,
            ignore_release: false,
        }
    }

    pub fn handle_edge(&mut self, edge: ButtonEdge, now: Instant) -> Option<ButtonGesture> {
        if self.wait_until.is_some_and(|wait_until| now >= wait_until) {
            self.wait_until = None;

            if self.click_count == 0 {
                if edge != ButtonEdge::Released {
                    self.ignore_release = true;
                }
                return Some(ButtonGesture::Press);
            }

            self.click_count = 0;
            return Some(ButtonGesture::Click);
        }

        match edge {
            ButtonEdge::Pressed => {
                if self.click_count == 0 {
                    self.wait_until = Some(now + self.long_press);
                }
                None
            }
            ButtonEdge::Released => {
                if self.ignore_release {
                    self.ignore_release = false;
                    return None;
                }

                self.click_count = self.click_count.saturating_add(1);
                if self.click_count == 1 {
                    self.wait_until = Some(now + self.double_click);
                    None
                } else {
                    self.click_count = 0;
                    self.wait_until = None;
                    Some(ButtonGesture::Twice)
                }
            }
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Option<ButtonGesture> {
        if self.wait_until.is_none_or(|wait_until| now < wait_until) {
            return None;
        }

        self.wait_until = None;

        if self.click_count == 0 {
            self.ignore_release = true;
            Some(ButtonGesture::Press)
        } else {
            self.click_count = 0;
            Some(ButtonGesture::Click)
        }
    }

    pub fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.wait_until
            .map(|wait_until| wait_until.saturating_duration_since(now))
    }
}

fn duration_from_seconds(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> ButtonClassifier {
        ButtonClassifier::new(TimeConfig {
            twice: 0.7,
            press: 1.8,
        })
    }

    #[test]
    fn classifies_single_click_after_double_click_window() {
        let t0 = Instant::now();
        let mut classifier = classifier();

        assert_eq!(classifier.handle_edge(ButtonEdge::Pressed, t0), None);
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            classifier.handle_timeout(t0 + Duration::from_millis(801)),
            Some(ButtonGesture::Click)
        );
    }

    #[test]
    fn classifies_double_click_on_second_release() {
        let t0 = Instant::now();
        let mut classifier = classifier();

        assert_eq!(classifier.handle_edge(ButtonEdge::Pressed, t0), None);
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Pressed, t0 + Duration::from_millis(200)),
            None
        );
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(300)),
            Some(ButtonGesture::Twice)
        );
    }

    #[test]
    fn classifies_long_press_and_ignores_release() {
        let t0 = Instant::now();
        let mut classifier = classifier();

        assert_eq!(classifier.handle_edge(ButtonEdge::Pressed, t0), None);
        assert_eq!(
            classifier.handle_timeout(t0 + Duration::from_millis(1800)),
            Some(ButtonGesture::Press)
        );
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(1900)),
            None
        );
    }

    #[test]
    fn consumes_release_that_arrives_after_long_press_deadline() {
        let t0 = Instant::now();
        let mut classifier = classifier();

        assert_eq!(classifier.handle_edge(ButtonEdge::Pressed, t0), None);
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(1900)),
            Some(ButtonGesture::Press)
        );

        assert_eq!(
            classifier.handle_edge(ButtonEdge::Pressed, t0 + Duration::from_millis(2200)),
            None
        );
        assert_eq!(
            classifier.handle_edge(ButtonEdge::Released, t0 + Duration::from_millis(2300)),
            None
        );
        assert_eq!(
            classifier.handle_timeout(t0 + Duration::from_millis(3100)),
            Some(ButtonGesture::Click)
        );
    }

    #[test]
    fn maps_configured_actions_to_gestures() {
        let key_config = KeyConfig {
            click: "slider".to_string(),
            twice: "switch".to_string(),
            press: "poweroff".to_string(),
        };

        assert_eq!(
            action_for_gesture(ButtonGesture::Click, &key_config),
            ButtonAction::Slider
        );
        assert_eq!(
            action_for_gesture(ButtonGesture::Twice, &key_config),
            ButtonAction::Switch
        );
        assert_eq!(
            action_for_gesture(ButtonGesture::Press, &key_config),
            ButtonAction::Poweroff
        );
    }

    #[test]
    fn output_poll_classifier_debounces_and_classifies_single_click() {
        let t0 = Instant::now();
        let mut classifier = OutputPollClassifier::new(TimeConfig {
            twice: 0.7,
            press: 1.8,
        });

        assert_eq!(classifier.handle_level(true, t0), None);
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(20)),
            None
        );
        assert_eq!(
            classifier.handle_level(true, t0 + Duration::from_millis(40)),
            None
        );
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(60)),
            None
        );
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            classifier.handle_level(true, t0 + Duration::from_millis(120)),
            None
        );
        assert_eq!(
            classifier.handle_level(true, t0 + Duration::from_millis(160)),
            None
        );
        assert_eq!(
            classifier.handle_level(true, t0 + Duration::from_millis(861)),
            Some(ButtonGesture::Click)
        );
    }

    #[test]
    fn output_poll_classifier_classifies_double_click() {
        let t0 = Instant::now();
        let mut classifier = OutputPollClassifier::new(TimeConfig {
            twice: 0.7,
            press: 1.8,
        });

        assert_eq!(classifier.handle_level(true, t0), None);
        for (milliseconds, level) in [
            (20, false),
            (60, false),
            (80, true),
            (120, true),
            (200, false),
            (240, false),
            (260, true),
        ] {
            assert_eq!(
                classifier.handle_level(level, t0 + Duration::from_millis(milliseconds)),
                None
            );
        }
        assert_eq!(
            classifier.handle_level(true, t0 + Duration::from_millis(300)),
            Some(ButtonGesture::Twice)
        );
    }

    #[test]
    fn output_poll_classifier_detects_long_press() {
        let t0 = Instant::now();
        let mut classifier = OutputPollClassifier::new(TimeConfig {
            twice: 0.7,
            press: 1.8,
        });

        assert_eq!(classifier.handle_level(true, t0), None);
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(20)),
            None
        );
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(60)),
            None
        );
        assert_eq!(
            classifier.handle_level(false, t0 + Duration::from_millis(1860)),
            Some(ButtonGesture::Press)
        );
    }

    #[test]
    fn slider_action_signals_oled_runtime() {
        let fan_enabled = AtomicBool::new(true);
        let oled_signal = OledSignal::default();
        let key_config = KeyConfig {
            click: "slider".to_string(),
            twice: "switch".to_string(),
            press: "none".to_string(),
        };

        run_button_action(
            ButtonGesture::Click,
            &key_config,
            &fan_enabled,
            &oled_signal,
        )
        .unwrap();

        assert!(oled_signal.take_requested());
    }
}
