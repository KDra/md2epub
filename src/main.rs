use std::process::ExitCode;

use clap::Parser;
use md2epub::cli::Cli;
use md2epub::pipeline;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match pipeline::run(&cli) {
        Ok(report) => {
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
