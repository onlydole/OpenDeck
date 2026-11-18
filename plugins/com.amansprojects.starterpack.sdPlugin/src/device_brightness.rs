//! Non-spec OpenDeck-specific protocols are used in this file.
//!
//! On a dial, each click changes the brightness by STEP_PER_CLICK and the touch strip shows the new
//! value. The first click is applied at once; while the dial keeps turning, at most one absolute
//! "set" is sent per THROTTLE, plus a final one so the device always lands on the last value. This
//! keeps the dial responsive without flooding OpenDeck with a settings save for every click.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use openaction::*;

use serde::{Deserialize, Serialize};

const STEP_PER_CLICK: i32 = 5;
const MIN_BRIGHTNESS: i32 = 5;
const THROTTLE: Duration = Duration::from_millis(100);

#[derive(Serialize)]
struct DeviceBrightnessEvent {
	event: &'static str,
	action: String,
	value: u8,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct DeviceBrightnessSettings {
	action: Option<String>,
	value: Option<u8>,
}

/// Brightness OpenDeck was last told (`applied`), the value the dial is heading to (`target`),
/// when the last "set" went out, and whether a catch-up send is already scheduled.
struct DialState {
	applied: Option<i32>,
	target: Option<i32>,
	last_sent: Option<Instant>,
	catch_up_scheduled: bool,
}
static DIAL: Mutex<DialState> = Mutex::new(DialState {
	applied: None,
	target: None,
	last_sent: None,
	catch_up_scheduled: false,
});

/// OpenDeck's settings.json, in the app config directory for its "opendeck" identifier.
fn opendeck_settings_path() -> Option<std::path::PathBuf> {
	let base = if cfg!(target_os = "macos") {
		std::env::home_dir()?
			.join("Library")
			.join("Application Support")
	} else if cfg!(target_os = "windows") {
		std::path::PathBuf::from(std::env::var_os("APPDATA")?)
	} else {
		std::env::var_os("XDG_CONFIG_HOME")
			.map(std::path::PathBuf::from)
			.filter(|p| p.is_absolute())
			.or_else(|| std::env::home_dir().map(|h| h.join(".config")))?
	};
	Some(base.join("opendeck").join("settings.json"))
}

/// OpenDeck's saved global brightness (the starting point before this plugin has set one).
fn saved_brightness() -> i32 {
	opendeck_settings_path()
		.and_then(|p| std::fs::read_to_string(p).ok())
		.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
		.and_then(|v| v.get("brightness").and_then(|b| b.as_i64()))
		.map(|b| b as i32)
		.unwrap_or(50)
}

fn current_brightness() -> i32 {
	let mut dial = DIAL.lock().unwrap();
	let applied = *dial.applied.get_or_insert_with(saved_brightness);
	dial.target.unwrap_or(applied)
}

async fn show(instance: &Instance, value: i32) -> OpenActionResult<()> {
	instance
		.set_feedback(&serde_json::json!({
			"title": "Brightness",
			"value": format!("{value}%"),
			"indicator": { "value": value },
		}))
		.await
}

async fn send_set(value: i32) {
	let _ = send_arbitrary_json(DeviceBrightnessEvent {
		event: "deviceBrightness",
		action: "set".to_owned(),
		value: value as u8,
	})
	.await;
}

/// While the dial is turning, once per throttle window send whatever value it has reached; when a
/// window passes with no new value, the turn is over.
async fn catch_up() {
	loop {
		tokio::time::sleep(THROTTLE).await;
		let value = {
			let mut dial = DIAL.lock().unwrap();
			match dial.target {
				Some(v) if Some(v) != dial.applied => {
					dial.applied = Some(v);
					dial.last_sent = Some(Instant::now());
					v
				}
				_ => {
					dial.target = None; // turn finished; the next turn re-reads settings.json
					dial.catch_up_scheduled = false;
					return;
				}
			}
		};
		send_set(value).await;
	}
}

pub struct DeviceBrightnessAction;
#[async_trait]
impl Action for DeviceBrightnessAction {
	const UUID: &'static str = "com.amansprojects.starterpack.devicebrightness";
	type Settings = DeviceBrightnessSettings;

	async fn will_appear(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
	) -> OpenActionResult<()> {
		if instance.controller == "Encoder" {
			let _ = instance.set_feedback_layout("$B1".to_owned()).await;
			show(instance, current_brightness()).await?;
		}
		Ok(())
	}

	async fn key_up(
		&self,
		_instance: &Instance,
		settings: &Self::Settings,
	) -> OpenActionResult<()> {
		send_arbitrary_json(DeviceBrightnessEvent {
			event: "deviceBrightness",
			action: settings.action.as_deref().unwrap_or("set").to_owned(),
			value: settings.value.unwrap_or(50),
		})
		.await
	}

	async fn dial_up(
		&self,
		instance: &Instance,
		settings: &Self::Settings,
	) -> OpenActionResult<()> {
		self.key_up(instance, settings).await
	}

	async fn dial_rotate(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
		ticks: i16,
		_pressed: bool,
	) -> OpenActionResult<()> {
		let (value, send_now, schedule) = {
			let mut dial = DIAL.lock().unwrap();
			if dial.target.is_none()
				&& dial
					.last_sent
					.is_none_or(|t| t.elapsed() > Duration::from_secs(2))
			{
				// Start of a new turn after a pause: re-read the saved value in case it was changed in
				// OpenDeck's settings (not right after our own send, while OpenDeck may still be saving).
				dial.applied = Some(saved_brightness());
			}
			let applied = dial.applied.unwrap_or(50);
			let value = (dial.target.unwrap_or(applied) + ticks as i32 * STEP_PER_CLICK)
				.clamp(MIN_BRIGHTNESS, 100);
			dial.target = Some(value);
			let ready = dial.last_sent.is_none_or(|t| t.elapsed() >= THROTTLE);
			let send_now = ready && Some(value) != dial.applied;
			if send_now {
				dial.applied = Some(value);
				dial.last_sent = Some(Instant::now());
			}
			let schedule = !std::mem::replace(&mut dial.catch_up_scheduled, true);
			(value, send_now, schedule)
		};
		if send_now {
			send_set(value).await;
		}
		let _ = show(instance, value).await;
		if schedule {
			tokio::spawn(catch_up());
		}
		Ok(())
	}
}
