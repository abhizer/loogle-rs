use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
};

use clap::Parser;
use color_eyre::eyre::Result;

use loogle::{model::Model, server};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Event, EventKind, RecursiveMode, Watcher,
};

#[derive(Parser, Debug)]
#[command(
    name = "Loogle",
    author = "Abhinav Gyawali",
    about = "A local search engine for your notes, Local Google - Loogle"
)]
struct Args {
    /// Directory to index and search files in
    #[arg(short, long, env = "DATA_DIR")]
    data_dir: PathBuf,
    /// Index file
    #[arg(short, long, env = "LOOGLE_INDEX")]
    index: PathBuf,
    /// Port
    #[arg(long, env = "LOOGLE_PORT")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    color_eyre::install()?;

    let args = Args::parse();

    tracing_subscriber::fmt::init();
    tracing::info!("logger initialized");

    println!("got path: {}", args.data_dir.display());

    let socket_addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = TcpListener::bind(socket_addr)?;

    let model = Model::load_index_or_from_data_dir(&args.index, &args.data_dir).await?;
    model.save().await?;

    let (tx, rx) = flume::bounded(1);

    let tx = tx;
    tracing::info!("watching for changes in: {}", &args.data_dir.display());
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: Result<Event, _>| match res {
            Ok(event)
                if matches!(
                    event.kind,
                    EventKind::Create(CreateKind::File)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(RemoveKind::File | RemoveKind::Folder)
                ) =>
            {
                _ = tx.send(event);
            }
            _ => (),
        },
        notify::Config::default(),
    )
    .unwrap();

    watcher
        .watch(&args.data_dir, RecursiveMode::Recursive)
        .unwrap();

    tracing::info!("starting web server");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
                std::process::exit(0);
        },
        _ = server::init(listener, model, rx) => {},
    }

    Ok(())
}
