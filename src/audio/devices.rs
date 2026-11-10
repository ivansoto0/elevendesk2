//! Audio device enumeration. Devices are identified by name, matching the
//! original app's settings, because that is what users see and pick.

use std::collections::HashSet;

use rodio::{
	CpalError, DeviceTrait,
	cpal::{default_host, traits::HostTrait},
};

fn names<I>(devices: Result<I, CpalError>) -> Vec<String>
where
	I: Iterator,
	I::Item: DeviceTrait,
{
	let mut names: Vec<String> = devices
		.map(|devices| {
			devices
				.filter_map(|device| device.description().ok().map(|description| description.name().to_string()))
				.collect()
		})
		.unwrap_or_default();
	let mut seen = HashSet::new();
	names.retain(|name| seen.insert(name.clone()));
	names
}

pub fn output_device_names() -> Vec<String> {
	names(default_host().output_devices())
}

pub fn input_device_names() -> Vec<String> {
	names(default_host().input_devices())
}
