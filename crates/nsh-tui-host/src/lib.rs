//! Development host for terminal apps. It loads a developer-selected
//! bundle or connects to `nsh dev`, retaining state across hot updates.
//! App-specific generated hosts register their UI sets before [`main`].

use std::{io, path::PathBuf, process::ExitCode};

use nsh_platform_tui::{AppOptions, AppSource, AssetBase, Bundle, Limits};

/// Runs the development host with process arguments. `--size` uses
/// terminal columns and rows; `--script` executes headlessly. `--app-id`
/// names the application; `nsh run` supplies its `nsh.json` identifier.
pub fn main() -> ExitCode {
    nsh_platform_tui::init_logging();
    match arguments(std::env::args().skip(1))
        .and_then(|(source, options)| nsh_platform_tui::run_app(source, options))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error {error}");
            ExitCode::FAILURE
        }
    }
}

fn arguments(args: impl IntoIterator<Item = String>) -> io::Result<(AppSource, AppOptions)> {
    let mut source = None;
    let mut options = AppOptions {
        limits: Limits::standalone(),
        ..Default::default()
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let missing =
            || io::Error::new(io::ErrorKind::InvalidInput, format!("{arg} needs a value"));
        match arg.as_str() {
            "--dev-server" => source = Some(AppSource::DevServer(args.next().ok_or_else(missing)?)),
            "--bundle" => {
                let path = PathBuf::from(args.next().ok_or_else(missing)?);
                // SAFETY: the developer explicitly selected this bundle
                // on the command line of a development host.
                let bundle = unsafe { Bundle::read_trusted(&path) }?;
                options.asset_base = path
                    .parent()
                    .map(|dir| AssetBase::Directory(dir.join("assets")));
                source = Some(AppSource::Bundle(Box::new(bundle)));
            }
            "--trace" => options.trace = true,
            "--script" => options.script = Some(args.next().ok_or_else(missing)?),
            "--size" => {
                let size = args.next().ok_or_else(missing)?;
                let (columns, rows) = size.split_once('x').ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--size uses columnsxrows, for example 120x40",
                    )
                })?;
                let columns = columns.parse::<u16>().map_err(io::Error::other)?;
                let rows = rows.parse::<u16>().map_err(io::Error::other)?;
                if columns == 0 || rows == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--size needs nonzero columns and rows",
                    ));
                }
                options.size = (columns, rows);
            }
            "--title" => options.title = args.next().ok_or_else(missing)?,
            "--app-id" => {
                options.app_id = args
                    .next()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(missing)?;
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
    let source = source.unwrap_or_else(|| {
        use nsh_dev::protocol::{DEFAULT_PORT, WS_PATH};
        AppSource::DevServer(format!("ws://127.0.0.1:{DEFAULT_PORT}{WS_PATH}"))
    });
    Ok((source, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_size_and_bad_flags_are_validated() {
        let args = ["--size", "80x24", "--script", "quit"].map(str::to_owned);
        let (_, options) = arguments(args).unwrap();
        assert_eq!(options.size, (80, 24));
        assert_eq!(options.script.as_deref(), Some("quit"));
        assert!(arguments(["--size", "0x24"].map(str::to_owned)).is_err());
        assert!(arguments(["--script"].map(str::to_owned)).is_err());
        assert!(arguments(["--unknown"].map(str::to_owned)).is_err());
    }

    #[test]
    fn the_cli_can_name_the_app_in_the_terminal_host() {
        let (_, options) = arguments(["--app-id", "com.acme.orders"].map(str::to_owned)).unwrap();
        assert_eq!(options.app_id, "com.acme.orders");
        assert!(arguments(["--app-id"].map(str::to_owned)).is_err());
        assert!(arguments(["--app-id", ""].map(str::to_owned)).is_err());
    }
}
