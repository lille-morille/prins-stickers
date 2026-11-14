use clap::Parser;
use prins::commands::{self, Cli};

fn main() {
    // Help and version go to stdout and exit 0. A usage mistake is an error like any other: one
    // line `prins: <message>` on stderr and exit 1 (clap's own default is a long text and exit 2).
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => {
            let _ = e.print();
            std::process::exit(0);
        }
        Err(e) => {
            let text = e.render().to_string();
            let first = text.lines().next().unwrap_or("usage error");
            let first = first.strip_prefix("error: ").unwrap_or(first);
            eprintln!("prins: {first} (see `prins --help`)");
            std::process::exit(1);
        }
    };
    if let Err(e) = commands::run(cli) {
        let message = format!("{e:#}");
        eprintln!("prins: {}", message.trim().replace('\n', " "));
        std::process::exit(1);
    }
}
