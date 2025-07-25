#[cfg(feature = "json-output")]
use std::{fs::File, path::PathBuf};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use quinn::{TokioRuntime, crypto::rustls::QuicClientConfig};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::sync::Semaphore;
use tracing::{debug, error, info};

use perf::{
    CongestionAlgorithm, bind_socket,
    noprotection::NoProtectionClientConfig,
    stats::{OpenStreamStats, Stats},
};

/// Connects to a QUIC perf server and maintains a specified pattern of requests until interrupted
#[derive(Parser)]
#[clap(name = "client")]
struct Opt {
    /// Host to connect to
    #[clap(default_value = "localhost:4433")]
    host: String,
    /// Override DNS resolution for host
    #[clap(long)]
    ip: Option<IpAddr>,
    /// Number of unidirectional requests to maintain concurrently
    #[clap(long, default_value = "0")]
    uni_requests: u64,
    /// Number of bidirectional requests to maintain concurrently
    #[clap(long, default_value = "1")]
    bi_requests: u64,
    /// Number of bytes to request
    #[clap(long, default_value = "1048576")]
    download_size: u64,
    /// Number of bytes to transmit, in addition to the request header
    #[clap(long, default_value = "1048576")]
    upload_size: u64,
    /// The time to run in seconds
    #[clap(long, default_value = "60")]
    duration: u64,
    /// The interval in seconds at which stats are reported
    #[clap(long, default_value = "1")]
    interval: u64,
    /// Send buffer size in bytes
    #[clap(long, default_value = "2097152")]
    send_buffer_size: usize,
    /// Receive buffer size in bytes
    #[clap(long, default_value = "2097152")]
    recv_buffer_size: usize,
    /// Specify the local socket address
    #[clap(long)]
    local_addr: Option<SocketAddr>,
    /// Whether to print connection statistics
    #[clap(long)]
    conn_stats: bool,
    /// File path to output JSON statistics to. If the file is '-', stdout will be used
    #[cfg(feature = "json-output")]
    #[clap(long)]
    json: Option<PathBuf>,
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "keylog")]
    keylog: bool,
    /// UDP payload size that the network must be capable of carrying
    #[clap(long, default_value = "1200")]
    initial_mtu: u16,
    /// Disable packet encryption/decryption (for debugging purpose)
    #[clap(long = "no-protection")]
    no_protection: bool,
    /// The initial round-trip-time (in msecs)
    #[clap(long)]
    initial_rtt: Option<u64>,
    /// Ack Frequency mode
    #[clap(long = "ack-frequency")]
    ack_frequency: bool,
    /// Congestion algorithm to use
    #[clap(long = "congestion")]
    cong_alg: Option<CongestionAlgorithm>,
    /// qlog output file
    #[cfg(feature = "qlog")]
    #[clap(long = "qlog")]
    qlog_file: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = Opt::parse();

    tracing_subscriber::fmt::init();

    if let Err(e) = run(opt).await {
        error!("{:#}", e);
    }
}

async fn run(opt: Opt) -> Result<()> {
    let mut host_parts = opt.host.split(':');
    let host_name = host_parts.next().unwrap();
    let host_port = host_parts
        .next()
        .map_or(Ok(443), |x| x.parse())
        .context("parsing port")?;
    let addr = match opt.ip {
        None => tokio::net::lookup_host(&opt.host)
            .await
            .context("resolving host")?
            .next()
            .unwrap(),
        Some(ip) => SocketAddr::new(ip, host_port),
    };

    info!("connecting to {} at {}", host_name, addr);

    let bind_addr = opt.local_addr.unwrap_or_else(|| {
        let unspec = if addr.is_ipv4() {
            Ipv4Addr::UNSPECIFIED.into()
        } else {
            Ipv6Addr::UNSPECIFIED.into()
        };
        SocketAddr::new(unspec, 0)
    });

    info!("local addr {:?}", bind_addr);

    let socket = bind_socket(bind_addr, opt.send_buffer_size, opt.recv_buffer_size)?;

    let endpoint = quinn::Endpoint::new(Default::default(), None, socket, Arc::new(TokioRuntime))?;

    let default_provider = rustls::crypto::ring::default_provider();
    let provider = Arc::new(rustls::crypto::CryptoProvider {
        cipher_suites: perf::PERF_CIPHER_SUITES.into(),
        ..default_provider
    });

    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new(provider))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"perf".to_vec()];

    if opt.keylog {
        crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    }

    let mut transport = quinn::TransportConfig::default();
    transport.initial_mtu(opt.initial_mtu);

    if let Some(initial_rtt) = opt.initial_rtt {
        transport.initial_rtt(Duration::from_millis(initial_rtt));
    }

    if opt.ack_frequency {
        transport.ack_frequency_config(Some(quinn::AckFrequencyConfig::default()));
    }

    if let Some(cong_alg) = opt.cong_alg {
        transport.congestion_controller_factory(cong_alg.build());
    }

    #[cfg(feature = "qlog")]
    if let Some(qlog_file) = &opt.qlog_file {
        let mut qlog = quinn::QlogConfig::default();
        qlog.writer(Box::new(File::create(qlog_file)?))
            .title(Some("perf-client".into()));
        transport.qlog_stream(qlog.into_stream());
    }

    let crypto = Arc::new(QuicClientConfig::try_from(crypto)?);
    let mut config = quinn::ClientConfig::new(match opt.no_protection {
        true => Arc::new(NoProtectionClientConfig::new(crypto)),
        false => crypto,
    });
    config.transport_config(Arc::new(transport));

    let stream_stats = OpenStreamStats::default();

    let connection = endpoint
        .connect_with(config, addr, host_name)?
        .await
        .context("connecting")?;

    info!("established");

    let drive_fut = async {
        tokio::try_join!(
            drive_uni(
                connection.clone(),
                stream_stats.clone(),
                opt.uni_requests,
                opt.upload_size,
                opt.download_size
            ),
            drive_bi(
                connection.clone(),
                stream_stats.clone(),
                opt.bi_requests,
                opt.upload_size,
                opt.download_size
            )
        )
    };

    let mut stats = Stats::default();

    let stats_fut = async {
        let interval_duration = Duration::from_secs(opt.interval);

        loop {
            let start = Instant::now();
            tokio::time::sleep(interval_duration).await;
            {
                stats.on_interval(start, &stream_stats);

                stats.print();
                if opt.conn_stats {
                    println!("{:?}\n", connection.stats());
                }
            }
        }
    };

    tokio::select! {
        _ = drive_fut => {}
        _ = stats_fut => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            connection.close(0u32.into(), b"interrupted");
        }
        // Add a small duration so the final interval can be reported
        _ = tokio::time::sleep(Duration::from_secs(opt.duration) + Duration::from_millis(200)) => {
            info!("shutting down");
            connection.close(0u32.into(), b"done");
        }
    }

    endpoint.wait_idle().await;

    #[cfg(feature = "json-output")]
    if let Some(path) = opt.json {
        stats.print_json(path.as_path())?;
    }

    Ok(())
}

async fn drain_stream(
    mut stream: quinn::RecvStream,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<()> {
    if download == 0 {
        return Ok(());
    }

    #[rustfmt::skip]
    let mut bufs = [
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
    ];
    let download_start = Instant::now();
    let recv_stream_stats = stream_stats.new_receiver(&stream, download);

    let mut first_byte = true;

    while let Some(size) = stream.read_chunks(&mut bufs[..]).await? {
        if first_byte {
            recv_stream_stats.on_first_byte(download_start.elapsed());
            first_byte = false;
        }
        let bytes_received = bufs[..size].iter().map(|b| b.len()).sum();
        recv_stream_stats.on_bytes(bytes_received);
    }

    if first_byte {
        recv_stream_stats.on_first_byte(download_start.elapsed());
    }
    recv_stream_stats.finish(download_start.elapsed());

    debug!("response finished on {}", stream.id());
    Ok(())
}

async fn drive_uni(
    connection: quinn::Connection,
    stream_stats: OpenStreamStats,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<()> {
    if concurrency == 0 {
        return Ok(());
    }

    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let send = connection.open_uni().await?;
        let stream_stats = stream_stats.clone();

        debug!("sending request on {}", send.id());
        let connection = connection.clone();
        tokio::spawn(async move {
            if let Err(e) = request_uni(send, connection, upload, download, stream_stats).await {
                error!("sending request failed: {:#}", e);
            }

            drop(permit);
        });
    }
}

async fn request_uni(
    send: quinn::SendStream,
    conn: quinn::Connection,
    upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<()> {
    request(send, upload, download, stream_stats.clone()).await?;
    let recv = conn.accept_uni().await?;
    drain_stream(recv, download, stream_stats).await?;
    Ok(())
}

async fn request(
    mut send: quinn::SendStream,
    mut upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<()> {
    let upload_start = Instant::now();
    send.write_all(&download.to_be_bytes()).await?;
    if upload == 0 {
        send.finish().unwrap();
        return Ok(());
    }

    let send_stream_stats = stream_stats.new_sender(&send, upload);

    static DATA: [u8; 1024 * 1024] = [42; 1024 * 1024];
    while upload > 0 {
        let chunk_len = upload.min(DATA.len() as u64);
        send.write_chunk(Bytes::from_static(&DATA[..chunk_len as usize]))
            .await
            .context("sending response")?;
        send_stream_stats.on_bytes(chunk_len as usize);
        upload -= chunk_len;
    }
    send.finish().unwrap();
    // Wait for stream to close
    _ = send.stopped().await;
    send_stream_stats.finish(upload_start.elapsed());

    debug!("upload finished on {}", send.id());
    Ok(())
}

async fn drive_bi(
    connection: quinn::Connection,
    stream_stats: OpenStreamStats,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<()> {
    if concurrency == 0 {
        return Ok(());
    }

    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let (send, recv) = connection.open_bi().await?;
        let stream_stats = stream_stats.clone();

        debug!("sending request on {}", send.id());
        tokio::spawn(async move {
            if let Err(e) = request_bi(send, recv, upload, download, stream_stats).await {
                error!("request failed: {:#}", e);
            }

            drop(permit);
        });
    }
}

async fn request_bi(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<()> {
    request(send, upload, download, stream_stats.clone()).await?;
    drain_stream(recv, download, stream_stats).await?;
    Ok(())
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Arc<Self> {
        Arc::new(Self(provider))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
