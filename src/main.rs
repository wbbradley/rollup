mod app;
mod github;
mod model;
mod report;
mod ui;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => app::run(),
        Some("report") => report::run(),
        Some(other) => {
            eprintln!("rollup: unknown subcommand '{other}'");
            eprintln!("usage: rollup [report]");
            std::process::exit(2);
        }
    }
}
