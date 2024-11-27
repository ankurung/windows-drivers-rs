// Copyright (c) Microsoft Corporation
// License: MIT OR Apache-2.0

//! Build script for the `wdk-sys` crate.
//!
//! This parses the WDK configuration from metadata provided in the build tree,
//! and generates the relevant bindings to WDK APIs.

use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
};

use bindgen::CodegenConfig;
use lazy_static::lazy_static;
use tracing::{info, info_span};
use tracing_subscriber::{
    filter::{LevelFilter, ParseError},
    EnvFilter,
};
use syn::{Item, ItemFn, ReturnType, Type, ForeignItem, ForeignItemFn};
use syn::__private::ToTokens;
use wdk_build::{
    find_top_level_cargo_manifest,
    BuilderExt,
    Config,
    ConfigError,
    DriverConfig,
    KMDFConfig,
    TryFromCargoMetadata,
    UMDFConfig,
    WDKMetadata,
};

const NUM_WDF_FUNCTIONS_PLACEHOLDER: &str =
    "<PLACEHOLDER FOR IDENTIFIER FOR VARIABLE CORRESPONDING TO NUMBER OF WDF FUNCTIONS>";
const WDF_FUNCTION_COUNT_DECLARATION_PLACEHOLDER: &str =
    "<PLACEHOLDER FOR DECLARATION OF wdf_function_count VARIABLE>";
const OUT_DIR_PLACEHOLDER: &str =
    "<PLACEHOLDER FOR LITERAL VALUE CONTAINING OUT_DIR OF wdk-sys CRATE>";
const WDFFUNCTIONS_SYMBOL_NAME_PLACEHOLDER: &str =
    "<PLACEHOLDER FOR LITERAL VALUE CONTAINING WDFFUNCTIONS SYMBOL NAME>";
const BINDINGS_CONTENTS_PLACEHOLDER: &str = "<PLACEHOLDER FOR BINDING CONTENTS>";

const WDF_FUNCTION_COUNT_DECLARATION_EXTERNAL_SYMBOL: &str = "
        // SAFETY: `crate::WdfFunctionCount` is generated as a mutable static, but is not supposed \
                                                              to be ever mutated by WDF.
        let wdf_function_count = unsafe { crate::WdfFunctionCount } as usize;";
const WDF_FUNCTION_COUNT_DECLARATION_TABLE_INDEX: &str = "
        let wdf_function_count = crate::_WDFFUNCENUM::WdfFunctionTableNumEntries as usize;";
        
// FIXME: replace lazy_static with std::Lazy once available: https://github.com/rust-lang/rust/issues/109736
lazy_static! {
    static ref WDF_FUNCTION_TABLE_TEMPLATE: String = format!(
        r#"
// FIXME: replace lazy_static with std::Lazy once available: https://github.com/rust-lang/rust/issues/109736
#[cfg(any(driver_type = "KMDF", driver_type = "UMDF"))]
lazy_static::lazy_static! {{
    #[allow(missing_docs)]
    pub static ref WDF_FUNCTION_TABLE: &'static [crate::WDFFUNC] = {{
        // SAFETY: `WdfFunctions` is generated as a mutable static, but is not supposed to be ever mutated by WDF.
        let wdf_function_table = unsafe {{ crate::WdfFunctions }};
{WDF_FUNCTION_COUNT_DECLARATION_PLACEHOLDER}

        // SAFETY: This is safe because:
        //         1. `WdfFunctions` is valid for reads for `{NUM_WDF_FUNCTIONS_PLACEHOLDER}` * `core::mem::size_of::<WDFFUNC>()`
        //            bytes, and is guaranteed to be aligned and it must be properly aligned.
        //         2. `WdfFunctions` points to `{NUM_WDF_FUNCTIONS_PLACEHOLDER}` consecutive properly initialized values of
        //            type `WDFFUNC`.
        //         3. WDF does not mutate the memory referenced by the returned slice for for its entire `'static' lifetime.
        //         4. The total size, `{NUM_WDF_FUNCTIONS_PLACEHOLDER}` * `core::mem::size_of::<WDFFUNC>()`, of the slice must be no
        //            larger than `isize::MAX`. This is proven by the below `debug_assert!`.
        unsafe {{
            debug_assert!(isize::try_from(wdf_function_count * core::mem::size_of::<crate::WDFFUNC>()).is_ok());
            core::slice::from_raw_parts(wdf_function_table, wdf_function_count)
        }}
    }};
}}"#
    );
    static ref CALL_UNSAFE_WDF_BINDING_TEMPLATE: String = format!(
        r#"
/// A procedural macro that allows WDF functions to be called by name.
///
/// This function parses the name of the WDF function, finds it function
/// pointer from the WDF function table, and then calls it with the
/// arguments passed to it
///
/// # Safety
/// Function arguments must abide by any rules outlined in the WDF
/// documentation. This macro does not perform any validation of the
/// arguments passed to it., beyond type validation.
///
/// # Examples
///
/// ```rust, no_run
/// use wdk_sys::*;
/// 
/// pub unsafe extern "system" fn driver_entry(
///     driver: &mut DRIVER_OBJECT,
///     registry_path: PCUNICODE_STRING,
/// ) -> NTSTATUS {{
/// 
///     let mut driver_config = WDF_DRIVER_CONFIG {{
///         Size: core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG,
///         ..WDF_DRIVER_CONFIG::default()
///     }};
///     let driver_handle_output = WDF_NO_HANDLE as *mut WDFDRIVER;
///
///     unsafe {{
///         call_unsafe_wdf_function_binding!(
///             WdfDriverCreate,
///             driver as PDRIVER_OBJECT,
///             registry_path,
///             WDF_NO_OBJECT_ATTRIBUTES,
///             &mut driver_config,
///             driver_handle_output,
///         )
///     }}
/// }}
/// ```
#[macro_export]
macro_rules! call_unsafe_wdf_function_binding {{
    ( $($tt:tt)* ) => {{
        $crate::__proc_macros::call_unsafe_wdf_function_binding! (
            r"{OUT_DIR_PLACEHOLDER}",
            $($tt)*
        )
    }}
}}"#
    );
    static ref TEST_STUBS_TEMPLATE: String = format!(
        r#"
use crate::WDFFUNC;

/// Stubbed version of the symbol that [`WdfFunctions`] links to so that test targets will compile
#[no_mangle]
pub static mut {WDFFUNCTIONS_SYMBOL_NAME_PLACEHOLDER}: *const WDFFUNC = core::ptr::null();
"#,
    );
    static ref BINDINGS_WITH_AUTOMOCK_TEMPLATE: String = format!(
        r#"
#[cfg_attr(feature = "test-stubs", mockall::automock)]
mod generated_bindings {{
use super::*;
{BINDINGS_CONTENTS_PLACEHOLDER}
}}
pub use generated_bindings::*;
"#,
);
}

fn initialize_tracing() -> Result<(), ParseError> {
    let tracing_filter = EnvFilter::default()
        // Show up to INFO level by default
        .add_directive(LevelFilter::INFO.into())
        // Silence various warnings originating from bindgen that are not currently actionable
        // FIXME: this currently sets the minimum log level to error for the listed modules. It
        // should actually be turning off logging (level=off) for specific warnings in these
        // modules, but a bug in the tracing crate's filtering is preventing this from working as expected. See https://github.com/tokio-rs/tracing/issues/2843.
        .add_directive("bindgen::codegen::helpers[{message}]=error".parse()?)
        .add_directive("bindgen::codegen::struct_layout[{message}]=error".parse()?)
        .add_directive("bindgen::ir::comp[{message}]=error".parse()?)
        .add_directive("bindgen::ir::context[{message}]=error".parse()?)
        .add_directive("bindgen::ir::ty[{message}]=error".parse()?)
        .add_directive("bindgen::ir::var[{message}]=error".parse()?);

    // Allow overriding tracing behaviour via `EnvFilter::DEFAULT_ENV` env var
    let tracing_filter =
        if let Ok(filter_directives_from_env_var) = env::var(EnvFilter::DEFAULT_ENV) {
            // Append each directive from the env var to the filter
            filter_directives_from_env_var.split(',').fold(
                tracing_filter,
                |tracing_filter, filter_directive| {
                    match filter_directive.parse() {
                        Ok(parsed_filter_directive) => {
                            tracing_filter.add_directive(parsed_filter_directive)
                        }
                        Err(parsing_error) => {
                            // Must use eprintln!() here as tracing is not yet initialized
                            eprintln!(
                                "Skipping filter directive, {}, which failed to be parsed from {} \
                                 obtained from {} with the following error: {}",
                                filter_directive,
                                filter_directives_from_env_var,
                                EnvFilter::DEFAULT_ENV,
                                parsing_error
                            );
                            tracing_filter
                        }
                    }
                },
            )
        } else {
            tracing_filter
        };

    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(tracing_filter)
        .init();

    Ok(())
}

fn generate_constants(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    Ok(bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config(CodegenConfig::VARS)
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("constants.rs"))?)
}

fn generate_types(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    Ok(bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config(CodegenConfig::TYPES)
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("types.rs"))?)
}

// Trait for exclusion patterns
trait ExclusionPattern {
    fn matches(&self, func: &ItemFn) -> bool;
    fn matches_foreign_fn(&self, func: &ForeignItemFn) -> bool;
}

// Pattern to exclude functions with `-> !`
struct NeverReturnPattern;
impl ExclusionPattern for NeverReturnPattern {
    fn matches(&self, func: &ItemFn) -> bool {
        matches!(func.sig.output, ReturnType::Type(_, ref ty) if matches!(**ty, Type::Never(_)))
    }
    
    fn matches_foreign_fn(&self, func: &ForeignItemFn) -> bool {
        matches!(func.sig.output, ReturnType::Type(_, ref ty) if matches!(**ty, Type::Never(_)))
    }
}

// Pattern to exclude variadic functions
struct CVariadicPattern;
impl ExclusionPattern for CVariadicPattern {
    fn matches(&self, func: &ItemFn) -> bool {
        func.sig.variadic.is_some()
    }
    
    fn matches_foreign_fn(&self, func: &ForeignItemFn) -> bool {
        func.sig.variadic.is_some()
    }
}

fn extract_file_root_and_extension(file_name: &str) -> (&str, &str) {
    if let Some(dot_pos) = file_name.rfind('.') {
        let base = &file_name[..dot_pos];
        let extension = &file_name[dot_pos + 1..];
        (base, extension)
    } else {
        (file_name,"rs") // Return rs as extension
    }
}

fn generate_base(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    let outfile_name = match &config.driver_config {
        DriverConfig::WDM | DriverConfig::KMDF(_) => "ntddk.rs",
        DriverConfig::UMDF(_) => "windows.rs",
    };

    let bindings = bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement())
        .generate()
        .expect("Bindings should succeed to generate")
        .to_string();

    let exclusion_patterns: Vec<Box<dyn ExclusionPattern>> = vec![
        Box::new(NeverReturnPattern),  // Exclude functions returning `!`
        Box::new(CVariadicPattern),    // Exclude functions with variadic arguments
    ];

    let syntax_tree = syn::parse_file(&bindings).map_err(ConfigError::from)?;

    let mut mockable_bindings = String::new();
    let mut non_mockable_bindings = String::new();
    
    for item in syntax_tree.items {
        let is_non_mockable = match item {
            Item::ForeignMod(ref foreign_mod) => {
                foreign_mod.items.iter().any(|item| {
                    matches!(item, ForeignItem::Fn(ref func)
                        if exclusion_patterns.iter().any(|pattern| pattern.matches_foreign_fn(func)))
                })
            }
            Item::Fn(ref func) => exclusion_patterns.iter().any(|pattern| pattern.matches(func)),
            _ => false,
        };
    
        let target_bindings = if is_non_mockable {
            &mut non_mockable_bindings
        } else {
            &mut mockable_bindings
        };
        
        target_bindings.push_str(&item.to_token_stream().to_string());
        target_bindings.push('\n');
    }

    // Generate bindings with automock attribute.
    let mock_enabled_bindings = BINDINGS_WITH_AUTOMOCK_TEMPLATE
                                .replace(BINDINGS_CONTENTS_PLACEHOLDER, &mockable_bindings);
    let syntax_tree = syn::parse_file(&mock_enabled_bindings).map_err(ConfigError::from)?;
    let pretty_output = prettyplease::unparse(&syntax_tree);
    let (base, extension) = extract_file_root_and_extension(outfile_name);
    let mocked_output_name = format!("{}_mocked.{}", base, extension);
    std::fs::write(out_path.join(mocked_output_name), pretty_output)?;

    // Generate bindings without automock attribute.
    let syntax_tree = syn::parse_file(&non_mockable_bindings).map_err(ConfigError::from)?;
    let pretty_output = prettyplease::unparse(&syntax_tree);
    Ok(std::fs::write(out_path.join(outfile_name), pretty_output)?)
}

fn generate_wdf(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    // As of NI WDK, this may generate an empty file due to no non-type and non-var
    // items in the wdf headers(i.e. functions are all inlined). This step is
    // intentionally left here in case older/newer WDKs have non-inlined functions
    // or new WDKs may introduce non-inlined functions.
    Ok(bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement())
        // Only generate for files that are prefixed with (case-insensitive) wdf (ie.
        // /some/path/WdfSomeHeader.h), to prevent duplication of code in ntddk.rs
        .allowlist_file("(?i).*wdf.*")
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("wdf.rs"))?)
}

fn generate_hid(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    let mut builder = bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement());

    // Only allowlist files in the hid-specific files declared for hid to
    // avoid duplicate definitions
    for header_file in [
        "hidclass.h",
        "hidpddi.h",
        "hidpi.h",
        "hidport.h",
        "hidsdi.h",
        "hidspicx.h",
        "kbdmou.h",
        "ntdd8042.h",
        "vhf.h",
    ] {
        builder = builder.allowlist_file(format!(".*{header_file}.*"));
    }

    Ok(builder
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("hid.rs"))?)
}

fn generate_parallel_ports(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    let mut builder = bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement());

    // Only allowlist files in the parallel ports-specific files declared for
    // parallel ports to avoid duplicate definitions
    for header_file in [
        "gpio.h",
        "gpioclx.h",
        "ntddpar.h",
        "ntddser.h",
        "parallel.h",
    ] {
        builder = builder.allowlist_file(format!(".*{header_file}.*"));
    }

    Ok(builder
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("parallel_ports.rs"))?)
}

fn generate_spb(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    let mut builder = bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement());

    // Only allowlist files in the usb-specific files declared for spb to
    // avoid duplicate definitions
    for header_file in ["spb.h", "spbcx.h", "reshub.h", "pwmutil.h"] {
        builder = builder.allowlist_file(format!(".*{header_file}.*"));
    }

    Ok(builder
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("spb.rs"))?)
}

fn generate_usb(out_path: &Path, config: &Config) -> Result<(), ConfigError> {
    let mut builder = bindgen::Builder::wdk_default(vec!["src/input.h"], config)?
        .with_codegen_config((CodegenConfig::TYPES | CodegenConfig::VARS).complement());

    // Only allowlist files in the usb-specific files declared for usb to
    // avoid duplicate definitions
    for header_file in [
        "usb.h",
        "usbbusif.h",
        "usbdlib.h",
        "usbfnattach.h",
        "usbfnbase.h",
        "usbfnioctl.h",
        "usbioctl.h",
        "usbspec.h",
    ] {
        builder = builder.allowlist_file(format!(".*{header_file}.*"));
    }

    Ok(builder
        .generate()
        .expect("Bindings should succeed to generate")
        .write_to_file(out_path.join("usb.rs"))?)
}

fn generate_wdf_function_table(out_path: &Path, config: &Config) -> std::io::Result<()> {
    const MINIMUM_MINOR_VERSION_TO_GENERATE_WDF_FUNCTION_COUNT: u8 = 25;

    let generated_file_path = out_path.join("wdf_function_table.rs");
    let mut generated_file = std::fs::File::create(&generated_file_path)?;

    let is_wdf_function_count_generated = match *config {
        Config {
            driver_config:
                DriverConfig::KMDF(KMDFConfig {
                    kmdf_version_major,
                    target_kmdf_version_minor,
                    ..
                }),
            ..
        } => {
            kmdf_version_major >= 1
                && target_kmdf_version_minor >= MINIMUM_MINOR_VERSION_TO_GENERATE_WDF_FUNCTION_COUNT
        }

        Config {
            driver_config:
                DriverConfig::UMDF(UMDFConfig {
                    umdf_version_major,
                    target_umdf_version_minor,
                    ..
                }),
            ..
        } => {
            umdf_version_major >= 2
                && target_umdf_version_minor >= MINIMUM_MINOR_VERSION_TO_GENERATE_WDF_FUNCTION_COUNT
        }

        _ => {
            unreachable!(
                "generate_wdf_function_table is only called with WDF driver configurations"
            )
        }
    };

    let wdf_function_table_code_snippet = if is_wdf_function_count_generated {
        WDF_FUNCTION_TABLE_TEMPLATE
            .replace(NUM_WDF_FUNCTIONS_PLACEHOLDER, "crate::WdfFunctionCount")
            .replace(
                WDF_FUNCTION_COUNT_DECLARATION_PLACEHOLDER,
                WDF_FUNCTION_COUNT_DECLARATION_EXTERNAL_SYMBOL,
            )
    } else {
        WDF_FUNCTION_TABLE_TEMPLATE
            .replace(
                NUM_WDF_FUNCTIONS_PLACEHOLDER,
                "crate::_WDFFUNCENUM::WdfFunctionTableNumEntries",
            )
            .replace(
                WDF_FUNCTION_COUNT_DECLARATION_PLACEHOLDER,
                WDF_FUNCTION_COUNT_DECLARATION_TABLE_INDEX,
            )
    };

    generated_file.write_all(wdf_function_table_code_snippet.as_bytes())?;
    Ok(())
}

/// Generates a `macros.rs` file in `OUT_DIR` which contains a
/// `call_unsafe_wdf_function_binding!` macro redirects to the
/// `wdk_macros::call_unsafe_wdf_function_binding` macro . This is required
/// in order to add an additional argument with the path to the file containing
/// generated types
fn generate_call_unsafe_wdf_function_binding_macro(out_path: &Path) -> std::io::Result<()> {
    let generated_file_path = out_path.join("call_unsafe_wdf_function_binding.rs");
    let mut generated_file = std::fs::File::create(&generated_file_path)?;
    generated_file.write_all(
        CALL_UNSAFE_WDF_BINDING_TEMPLATE
            .replace(
                OUT_DIR_PLACEHOLDER,
                out_path.join("types.rs").to_str().expect(
                    "path to file with generated type information should succesfully convert to a \
                     str",
                ),
            )
            .as_bytes(),
    )?;
    Ok(())
}

fn generate_test_stubs(out_path: &Path, config: &Config) -> std::io::Result<()> {
    let stubs_file_path = out_path.join("test_stubs.rs");
    let mut stubs_file = std::fs::File::create(&stubs_file_path)?;
    stubs_file.write_all(
        TEST_STUBS_TEMPLATE
            .replace(
                WDFFUNCTIONS_SYMBOL_NAME_PLACEHOLDER,
                &config.compute_wdffunctions_symbol_name().expect(
                    "KMDF and UMDF configs should always have a computable WdfFunctions symbol \
                     name",
                ),
            )
            .as_bytes(),
    )?;
    Ok(())
}

// TODO: most wdk-sys specific code should move to wdk-build in a wdk-sys
// specific module so it can be unit tested.

fn main() -> anyhow::Result<()> {
    initialize_tracing()?;

    let config = Config {
        driver_config: match WDKMetadata::try_from_cargo_metadata(find_top_level_cargo_manifest()) {
            Ok(wdk_metadata_configuration) => wdk_metadata_configuration.try_into()?,
            Err(<WDKMetadata as TryFromCargoMetadata>::Error::NoWDKConfigurationsDetected) => {
                // When building `wdk-sys`` standalone (i.e. without a driver crate), skip
                // binding generation
                tracing::warn!("No WDK configurations detected. Skipping WDK binding generation.");
                return Ok(());
            }
            Err(error) => {
                return Err(error.into());
            }
        },
        ..Config::default()
    };

    let out_path = PathBuf::from(
        env::var("OUT_DIR").expect("OUT_DIR should be exist in Cargo build environment"),
    );

    info_span!("bindgen").in_scope(|| {
        info!("Generating bindings to WDK");
        generate_constants(&out_path, &config)?;
        generate_types(&out_path, &config)?;
        generate_base(&out_path, &config)?;

        if let DriverConfig::KMDF(_) | DriverConfig::UMDF(_) = &config.driver_config {
            generate_wdf(&out_path, &config)?;
        }

        // FIXME: These should be properly gated by feature flags and have the correct
        // configuration considerations
        generate_hid(&out_path, &config)?;
        generate_spb(&out_path, &config)?;
        generate_parallel_ports(&out_path, &config)?;
        generate_usb(&out_path, &config)?;

        Ok::<(), ConfigError>(())
    })?;

    if let DriverConfig::KMDF(_) | DriverConfig::UMDF(_) = config.driver_config {
        // Compile a c library to expose symbols that are not exposed because of
        // __declspec(selectany)
        info_span!("cc").in_scope(|| {
            info!("Compiling wdf.c");
            let mut cc_builder = cc::Build::new();
            for (key, value) in config.get_preprocessor_definitions_iter() {
                cc_builder.define(&key, value.as_deref());
            }

            cc_builder
                .includes(config.get_include_paths()?)
                .file("src/wdf.c")
                .compile("wdf");
            Ok::<(), ConfigError>(())
        })?;

        info_span!("wdf_function_table.rs generation").in_scope(|| {
            generate_wdf_function_table(&out_path, &config)?;
            Ok::<(), std::io::Error>(())
        })?;

        info_span!("call_unsafe_wdf_function_binding.rs generation").in_scope(|| {
            generate_call_unsafe_wdf_function_binding_macro(&out_path)?;
            Ok::<(), std::io::Error>(())
        })?;

        info_span!("test_stubs.rs generation").in_scope(|| {
            generate_test_stubs(&out_path, &config)?;
            Ok::<(), std::io::Error>(())
        })?;
    }

    config.configure_library_build()?;
    Ok(config.export_config()?)
}
