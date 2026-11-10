use std::{env, path::Path};

fn main() {
	println!("cargo:rerun-if-changed=elevendesk2.manifest");
	if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
		&& env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
	{
		let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("elevendesk2.manifest");
		// GetWindowSubclass is exported by Common Controls v6, selected by this manifest.
		println!("cargo:rustc-link-arg-bin=elevendesk2=/MANIFEST:EMBED");
		println!("cargo:rustc-link-arg-bin=elevendesk2=/MANIFESTINPUT:{}", manifest.display());
	}
}
