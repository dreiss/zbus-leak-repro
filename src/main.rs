use std::{
    os::fd::AsRawFd as _,
    path::PathBuf,
    sync::{
        atomic::{self, AtomicIsize},
        Arc,
    },
    time::{Duration, Instant},
};

use clap::Parser as _;
use futures_util::{future::join_all, StreamExt as _};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::AsyncReadExt as _,
    net::{UnixListener, UnixStream},
};
use tracing::field::Visit;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};
use zbus::{
    dbus_interface, dbus_proxy, fdo, zvariant::Type, Connection, ConnectionBuilder, Guid,
    MessageStream,
};

#[derive(Clone, Debug, clap::Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum Command {
    Server(ServerArgs),
    Client(ClientArgs),
}

#[derive(Clone, Debug, clap::Parser)]
struct ServerArgs {
    socket_path: PathBuf,
}

#[derive(Clone, Debug, clap::Parser)]
struct ClientArgs {
    socket_path: PathBuf,
}

struct TrackingState {
    pub outstanding: AtomicIsize,
    pub sending_regex: Regex,
    pub sent_regex: Regex,
}

struct TrackingLayer {
    state: Arc<TrackingState>,
}

impl TrackingLayer {
    fn new() -> Self {
        Self {
            state: Arc::new(TrackingState {
                outstanding: 0.into(),
                sending_regex: Regex::new(
                    r"Sending message: Msg \{ type: MethodReturn, reply-serial: (\d+),",
                )
                .unwrap(),
                sent_regex: Regex::new(r"Sent message with serial: (\d+)").unwrap(),
            }),
        }
    }

    fn visitor(&self) -> TrackingVisitor {
        TrackingVisitor {
            state: self.state.clone(),
        }
    }
}

struct TrackingVisitor {
    pub state: Arc<TrackingState>,
}

impl Visit for TrackingVisitor {
    fn record_debug(&mut self, _field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let message = format!("{value:?}");
        let mut should_print = false;
        if self.state.sending_regex.find(&message).is_some() {
            self.state
                .outstanding
                .fetch_add(1, atomic::Ordering::SeqCst);
            should_print = true;
        }
        if self.state.sent_regex.find(&message).is_some() {
            self.state
                .outstanding
                .fetch_sub(1, atomic::Ordering::SeqCst);
            should_print = true;
        }
        if should_print {
            eprintln!("Outstanding: {:?}", self.state.outstanding);
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TrackingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "zbus::connection" {
            event.record(&mut self.visitor());
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    match args.command {
        Command::Server(args) => run_server(args).await,
        Command::Client(args) => run_client(args).await,
    }
}

#[derive(Debug, Type, Clone, Serialize, Deserialize)]
pub struct OutItem {
    pub source: String,
    pub group: String,
    pub name: String,
    pub unit: String,
}

#[derive(Debug, Type, Clone, Serialize, Deserialize)]
pub struct Output {
    pub indicators: Vec<OutItem>,
}

const SERVICE_NAME: &'static str = "repro.daemon";
const SERVICE_PATH: &'static str = "/repro";

#[dbus_proxy(
    interface = "com.Repro",
    default_service = "repro.daemon",
    default_path = "/repro"
)]
pub trait Repro {
    fn get_data(&self, size: u32) -> zbus::Result<Output>;
}

struct ReproService;

impl ReproService {
    fn new() -> Self {
        Self {}
    }
}

#[dbus_interface(name = "com.Repro")]
impl ReproService {
    async fn get_data(&self, size: u32) -> fdo::Result<Output> {
        Ok(Output {
            indicators: vec![
                OutItem {
                    source: "daemon".to_owned(),
                    group: "some_info".to_owned(),
                    name: "ABCDEFGHIJKLMNOPQRSTUVWXYZ".to_owned(),
                    unit: "".to_owned(),
                };
                size as usize
            ],
        })
    }
}

pub async fn wait_for_stream_end(conn: Connection) -> anyhow::Result<()> {
    let mut stream = MessageStream::from(conn);
    tokio::spawn(async move { while stream.next().await.is_some() {} }).await?;
    Ok(())
}

const NUM_CALLS: usize = 24;
const ARG_SIZE: usize = 600;

async fn run_server(args: ServerArgs) {
    if fs::try_exists(&args.socket_path).await.unwrap() {
        fs::remove_file(&args.socket_path).await.unwrap();
    }
    let listen_socket = UnixListener::bind(&args.socket_path).unwrap();

    loop {
        let (stream, _addr) = listen_socket
            .accept()
            .await
            .expect("D-Bus socket accept failed.");
        let fd = stream.as_raw_fd();
        let _ = fd;
        tokio::spawn(async move { serve_dbus_connection(stream).await });
    }
}

async fn serve_dbus_connection(stream: UnixStream) {
    let tracing_layer = TrackingLayer::new();
    let tracing_state = tracing_layer.state.clone();
    tracing_subscriber::registry().with(tracing_layer).init();

    let guid = Guid::generate();
    let dbus_conn = ConnectionBuilder::unix_stream(stream)
        .server(&guid)
        .p2p()
        .name(SERVICE_NAME)
        .expect("Failed to register service")
        .serve_at(SERVICE_PATH, ReproService::new())
        .expect("Failed to serve path")
        .build()
        .await
        .unwrap();

    // At this point, the Connection has started a background task to handle incoming requests.
    // We need to keep our reference to the Connection because it will shut down if it's dropped.
    // Follow its MessageStream so we can detect when the peer disconnects
    // (which causes the stream to end).
    wait_for_stream_end(dbus_conn).await.unwrap();
    eprintln!("Peer disconnected and connection ended.");

    let wait_start = Instant::now();
    while tracing_state.outstanding.load(atomic::Ordering::SeqCst) > 0 {
        if Instant::now() - wait_start > Duration::from_millis(500) {
            eprintln!("Timed out waiting for socket to close.");
            eprintln!("Use lsof to see leaked file descriptor.");
            let _ = tokio::io::stdin().read_u8().await;
            std::process::exit(1);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_client(args: ClientArgs) {
    let raw_conn = UnixStream::connect(&args.socket_path)
        .await
        .expect("Couldn't connect to socket");
    let dbus_conn = ConnectionBuilder::unix_stream(raw_conn)
        .p2p()
        .build()
        .await
        .expect("Couldn't build D-Bus connection");

    let proxy = ReproProxy::new(&dbus_conn)
        .await
        .expect("Couldn't connect D-Bus proxy");

    let futs = (0..NUM_CALLS).map(|_| {
        let proxy = proxy.clone();
        async move { proxy.get_data(ARG_SIZE as u32).await }
    });

    let results = join_all(futs)
        .await
        .into_iter()
        .map(|r| r.expect("Failed to get data"))
        .collect::<Vec<_>>();

    for r in results {
        assert!(r.indicators.len() == ARG_SIZE);
    }

    // Now we'll close the stream to the peer.
}
