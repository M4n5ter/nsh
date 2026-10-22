//! Release runner for `nsh package --target tui`. Resources live in `nsh/`
//! beside the executable. Generated Rust apps call [`run_rust`] with their
//! native root; the feature `js` selects the bundle-based entry point.

use std::{
    io,
    path::{Path, PathBuf},
    process::ExitCode,
};

use nsh_platform_tui::{AppOptions, AppSource, Appearance, AssetBase, Limits, Root};

/// Runs the packaged JavaScript bundle until the app exits.
#[cfg(feature = "js")]
pub fn main() -> ExitCode {
    finish((|| {
        let (dir, options) = setup()?;
        // SAFETY: this bytecode is a resource of the app's own package.
        let bundle = unsafe { nsh_platform_tui::Bundle::read_trusted(&dir.join("bundle.nshc")) }?;
        nsh_platform_tui::run_app(AppSource::Bundle(Box::new(bundle)), options)
    })())
}

/// Entry point of a runner built for native roots only. Generated apps
/// invoke [`run_rust`] with their root component instead.
#[cfg(not(feature = "js"))]
pub fn main() -> ExitCode {
    eprintln!("error this runner loads Rust apps through run_rust");
    ExitCode::FAILURE
}

/// Runs a native Rust app with the package's resources and metadata.
pub fn run_rust(root: Root) -> ExitCode {
    finish(
        setup().and_then(|(_, options)| nsh_platform_tui::run_app(AppSource::Rust(root), options)),
    )
}

fn finish(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error {error}");
            ExitCode::FAILURE
        }
    }
}

fn setup() -> io::Result<(PathBuf, AppOptions)> {
    nsh_platform_tui::init_logging();
    let dir = match std::env::var_os("NSH_APP_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => resources(&std::env::current_exe()?.canonicalize()?)?,
    };
    let info = match std::fs::read_to_string(dir.join("app.json")) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(error) => return Err(error),
    };
    let mut options = AppOptions {
        asset_base: Some(AssetBase::Directory(dir.join("assets"))),
        limits: Limits::standalone(),
        script: std::env::var("NSH_SCRIPT").ok(),
        ..Default::default()
    };
    if let Some(name) = info.get("name").and_then(serde_json::Value::as_str) {
        options.title = name.into();
    }
    if let Some(id) = info.get("id").and_then(serde_json::Value::as_str) {
        options.app_id = id.into();
    }
    options.appearance = match info.get("appearance").and_then(serde_json::Value::as_str) {
        Some("light") => Appearance::Light,
        Some("dark") => Appearance::Dark,
        _ => Appearance::System,
    };
    // The development host and release runner share script and size flags,
    // making the same verification command usable against a packaged app.
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--script" => {
                options.script = Some(args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--script needs steps")
                })?)
            }
            "--trace" => options.trace = true,
            "--size" => {
                let size = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--size needs columnsxrows")
                })?;
                let (columns, rows) = size.split_once('x').ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--size uses columnsxrows")
                })?;
                options.size = (
                    columns.parse().map_err(io::Error::other)?,
                    rows.parse().map_err(io::Error::other)?,
                );
                if options.size.0 == 0 || options.size.1 == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--size needs nonzero dimensions",
                    ));
                }
            }
            "-v" | "--verbose" => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {other}"),
                ));
            }
        }
    }
    Ok((dir, options))
}

/// The package resource directory for an executable, including macOS app
/// bundles produced by an embedding host.
fn resources(executable: &Path) -> io::Result<PathBuf> {
    let parent = executable
        .parent()
        .ok_or_else(|| io::Error::other("the executable has no parent directory"))?;
    if parent.file_name().is_some_and(|name| name == "MacOS")
        && parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "Contents")
    {
        return Ok(parent.parent().unwrap().join("Resources/nsh"));
    }
    Ok(parent.join("nsh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_follow_the_executable() {
        assert_eq!(
            resources(Path::new("/opt/Counter/Counter")).unwrap(),
            Path::new("/opt/Counter/nsh")
        );
        assert_eq!(
            resources(Path::new(
                "/Applications/Counter.app/Contents/MacOS/Counter"
            ))
            .unwrap(),
            Path::new("/Applications/Counter.app/Contents/Resources/nsh")
        );
    }
}
