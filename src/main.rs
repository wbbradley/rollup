mod app;
mod github;
mod model;
mod ui;

fn main() -> anyhow::Result<()> {
    app::run()
}
