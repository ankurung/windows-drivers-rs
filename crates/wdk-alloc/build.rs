// Copyright (c) Microsoft Corporation
// License: MIT OR Apache-2.0

//! Build script for the `wdk-alloc` crate.
//!
//! Based on the [`wdk_build::Config`] parsed from the build tree, this build
//! script will provide the `wdk_alloc` crate with `cfg` settings to
//! conditionally compile code.

fn main() -> Result<(), wdk_build::ConfigError> {
    tracing_subscriber::fmt().pretty().init();

    match wdk_build::Config::from_env_auto() {
        Ok(config) => {
            config.configure_library_build()?;
            Ok(())
        }
        Err(wdk_build::ConfigFromEnvError::ConfigNotFound) => {
            // No WDK configurations will be detected if the crate is not being used in a
            // driver. This includes when building this crate standalone or in the
            // windows-drivers-rs workspace
            tracing::warn!("No WDK configurations detected.");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}