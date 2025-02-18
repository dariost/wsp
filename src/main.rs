use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::select;
use tokio::{
    net::{TcpListener, TcpStream},
    spawn,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async_with_config};

const BUFFER_SIZE: usize = 1024;

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Proxy traffic to a WebSocket server
    #[clap(name = "client")]
    Client(Client),

    /// Proxy traffic from WebSocket clients
    #[clap(name = "server")]
    Server(Server),
}

#[derive(Parser)]
struct Client {
    /// The WebSocket server URL to connect to
    url: String,
}

#[derive(Parser)]
struct Server {
    /// Bind address for the WebSocket server
    #[clap(short, long, default_value = "127.0.0.1")]
    addr: String,

    /// Bind port for the WebSocket server
    #[clap(short, long, default_value = "6449")]
    port: u16,

    /// The TCP socket to forward traffic to
    #[clap(short, long)]
    connect: String,
}

fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match args.cmd {
            Command::Client(client) => client.run().await,
            Command::Server(server) => server.run().await,
        }
    })
}

impl Client {
    async fn run(self) -> Result<(), anyhow::Error> {
        let (mut stream, _) = connect_async_with_config(self.url, None, true).await?;
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut buffer = vec![0; BUFFER_SIZE];
        loop {
            select! {
                msg = stream.next() => {
                    let msg = match msg {
                        None => break,
                        Some(Err(e)) => return Err(e.into()),
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(Message::Ping(p))) => {
                            stream.send(Message::Pong(p)).await?;
                            continue;
                        }
                        Some(Ok(Message::Binary(msg))) => msg,
                        Some(Ok(_)) => continue,
                    };
                    stdout.write_all(&msg).await?;
                }
                size = stdin.read(&mut buffer) => {
                    let size = size?;
                    if size == 0 {
                        break;
                    }
                    stream.send(Message::Binary(buffer[..size].to_vec().into())).await?;
                }
            }
        }
        Ok(())
    }
}

impl Server {
    async fn run(self) -> Result<(), anyhow::Error> {
        let listener = TcpListener::bind((self.addr, self.port)).await?;
        let addr = self.connect.parse()?;
        while let Ok((stream, _)) = listener.accept().await {
            spawn(async move {
                if let Err(e) = forward(stream, addr).await {
                    tracing::warn!("Error while forwarding: {}", e);
                }
            });
        }
        Ok(())
    }
}

async fn forward(stream: TcpStream, addr: SocketAddr) -> Result<(), anyhow::Error> {
    let mut inbound = accept_async(stream).await?;
    let mut outbound = TcpStream::connect(addr).await?;
    outbound.set_nodelay(true)?;
    loop {
        select! {
            msg = inbound.next() => {
                let msg = match msg {
                    None => break,
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(p))) => {
                        inbound.send(Message::Pong(p)).await?;
                        continue;
                    }
                    Some(Ok(Message::Binary(msg))) => msg,
                    Some(Ok(_)) => continue,
                };
                outbound.write_all(&msg).await?;
            }
            _ = outbound.readable() => {
                let mut buf = vec![0; BUFFER_SIZE];
                let n = outbound.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                inbound.send(Message::Binary(buf[..n].to_vec().into())).await?;
            }
        }
    }
    Ok(())
}
