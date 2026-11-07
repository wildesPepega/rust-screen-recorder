//! Lists PipeWire/PulseAudio outputs (sink monitors) and inputs.

use crate::config::{AudioDevice, AudioKind};

fn pactl_json(what: &str) -> Vec<serde_json::Value> {
    std::process::Command::new("pactl")
        .args(["-f", "json", "list", what])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
        .unwrap_or_default()
}

/// All devices that can be recorded, defaults first.
pub fn list_devices() -> Vec<AudioDevice> {
    let mut v = vec![AudioDevice::default_output(), AudioDevice::default_input()];
    for s in pactl_json("sinks") {
        let (Some(name), Some(desc)) = (s["name"].as_str(), s["description"].as_str()) else { continue };
        v.push(AudioDevice { name: format!("{name}.monitor"), label: desc.to_string(), kind: AudioKind::Output, volume: 100 });
    }
    for s in pactl_json("sources") {
        let (Some(name), Some(desc)) = (s["name"].as_str(), s["description"].as_str()) else { continue };
        if name.ends_with(".monitor") {
            continue;
        }
        v.push(AudioDevice { name: name.to_string(), label: desc.to_string(), kind: AudioKind::Input, volume: 100 });
    }
    v
}
