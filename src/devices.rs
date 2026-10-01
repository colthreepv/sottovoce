//! WASAPI device discovery and stable device choices.

use std::str::FromStr;

use cpal::traits::{DeviceTrait, HostTrait};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceChoice {
    FollowDefault,
    Pinned(String),
}

impl From<Option<String>> for DeviceChoice {
    fn from(id: Option<String>) -> Self {
        id.filter(|id| !id.trim().is_empty())
            .map_or(Self::FollowDefault, Self::Pinned)
    }
}

#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub default_format: Option<String>,
}

pub fn inputs() -> Result<Vec<AudioDevice>, String> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    host.input_devices()
        .map_err(|e| e.to_string())?
        .map(|device| describe(device, default_id.as_ref(), true))
        .collect()
}

pub fn outputs() -> Result<Vec<AudioDevice>, String> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().and_then(|d| d.id().ok());
    host.output_devices()
        .map_err(|e| e.to_string())?
        .map(|device| describe(device, default_id.as_ref(), false))
        .collect()
}

fn describe(
    device: cpal::Device,
    default_id: Option<&cpal::DeviceId>,
    input: bool,
) -> Result<AudioDevice, String> {
    let id = device.id().map_err(|e| e.to_string())?;
    let name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| device.to_string());
    let default_format = if input {
        device
            .default_input_config()
            .map(|format| format!("{format:?}"))
    } else {
        device
            .default_output_config()
            .map(|format| format!("{format:?}"))
    }
    .ok();
    Ok(AudioDevice {
        is_default: default_id == Some(&id),
        id: id.to_string(),
        name,
        default_format,
    })
}

pub fn find(id: &str) -> Option<cpal::Device> {
    let id = cpal::DeviceId::from_str(id).ok()?;
    cpal::default_host().device_by_id(&id)
}

pub(crate) fn choose_pinned<T>(
    choice: &DeviceChoice,
    pinned: Option<T>,
    default: Option<T>,
    pinned_available: bool,
) -> Option<(T, bool)> {
    match choice {
        DeviceChoice::FollowDefault => default.map(|device| (device, false)),
        DeviceChoice::Pinned(_) if pinned_available => pinned.map(|device| (device, true)),
        DeviceChoice::Pinned(_) => default.map(|device| (device, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_choice_uses_default_when_missing_and_recovers_when_returned() {
        let pinned_choice = DeviceChoice::Pinned("mic-id".into());
        assert_eq!(
            choose_pinned(&pinned_choice, Some("mic"), Some("default"), true),
            Some(("mic", true))
        );
        assert_eq!(
            choose_pinned(&pinned_choice, None::<&str>, Some("default"), false),
            Some(("default", false))
        );
        assert_eq!(
            choose_pinned(&pinned_choice, Some("mic"), Some("default"), true),
            Some(("mic", true))
        );
    }

    #[test]
    fn follow_default_ignores_pinned_device() {
        assert_eq!(
            choose_pinned(
                &DeviceChoice::FollowDefault,
                Some("mic"),
                Some("default"),
                true
            ),
            Some(("default", false))
        );
    }
}
