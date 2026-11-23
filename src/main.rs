use std::process::ExitCode;

use clap::FromArgMatches;
use md2epub::cli::{Cli, Command, Flags, command_with_defaults};
use md2epub::{pipeline, user_config};

fn main() -> ExitCode {
    // The user config is read first so that --help can show the effective
    // defaults. A broken file must not stop `config reset`/`edit`/`path`,
    // so its error is reported only when a conversion needs it.
    let user = user_config::load();
    let defaults = user_config::help_defaults(user.as_ref().unwrap_or(&Default::default()));
    let footer = user_config::help_footer();
    let matches = command_with_defaults(&defaults, Some(&footer)).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    if let Some(Command::Config { action }) = &cli.command {
        return config_main(&cli, action.as_ref());
    }

    let user = match user {
        Ok(u) => u,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    match pipeline::run(&cli, &user) {
        Ok(report) => {
            for n in &report.notices {
                eprintln!("note: {n}");
            }
            if !report.verbose {
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
            }
            eprintln!(
                "wrote {} ({} chapters, {} warnings)",
                report.output.display(),
                report.chapters,
                report.warnings.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `md2epub config ...`.
fn config_main(cli: &Cli, action: Option<&md2epub::cli::ConfigAction>) -> ExitCode {
    if !cli.inputs.is_empty() || cli.flags != Flags::default() {
        eprintln!(
            "error: `md2epub config` takes no input files or conversion options \
             (use `md2epub config set <key> <value>` to change a default; \
             to convert a file named `config`, write `./config`)"
        );
        return ExitCode::FAILURE;
    }
    let result = user_config::config_path().and_then(|path| {
        let mut stdout = std::io::stdout().lock();
        user_config::run(action, &path, &mut stdout)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
