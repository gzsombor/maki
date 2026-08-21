mod cli;
mod cmd;
mod print;
mod sdk_mode;
mod setup;
mod update;

use clap::Parser;

use cli::Cli;

fn main() {
    color_eyre::install().ok();
    let cli = Cli::parse();
    #[cfg(all(feature = "sandbox", target_os = "linux"))]
    {
        // Must happen before any child is forked or re-execed: the inner
        // instance rebuilds its state from this registry.
        maki_tools::install_child_workload();
        if cli.sandbox_inner {
            maki_sandbox::child::child_inner_main();
        }
    }
    if let Err(e) = cmd::dispatch(cli) {
        print_error(&e);
        std::process::exit(1);
    }
}

fn print_error(e: &color_eyre::Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let causes: Vec<_> = e.chain().skip(1).collect();
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}
