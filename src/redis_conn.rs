//! Shared single-node / cluster Redis connection plumbing.
//!
//! One implementation used by BOTH sides of the job pipeline — the worker
//! (consumer, [`crate::worker`]) and the HTTP jobs producer
//! ([`crate::queue`]). They were previously verbatim copies that had already
//! silently diverged (different operational response timeouts); any future
//! change to connection handling lands here once and applies to both.
//!
//! All commands issued through [`Conn`] are single-key (a stream, or one
//! `result:{id}`), so cluster routing never hits a cross-slot error.

use std::time::Duration;

use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::cluster::{ClusterClient, ClusterClientBuilder};
use redis::cluster_async::ClusterConnection;
use redis::{
    AsyncConnectionConfig, ClientTlsConfig, Cmd, ErrorKind, Pipeline, RedisFuture, RedisResult,
    TlsCertificates, Value,
};

/// Everything needed to build + open a connection. Both callers derive this
/// from their own env-backed config structs (same `BROWSER_HEADLESS_REDIS_*`
/// vars) but choose their own `response_timeout`: the worker must span its
/// `XREADGROUP BLOCK` window; the producer only issues fast commands.
pub(crate) struct RedisConnOpts {
    /// Single-node URL (`redis://…`), or comma-separated seed nodes when
    /// `cluster` is true.
    pub url: String,
    pub cluster: bool,
    /// PEM CA certificate to verify the server (private CA). `None` = system
    /// trust store. Use a `rediss://` URL for TLS.
    pub ca_cert_path: Option<String>,
    /// PEM client certificate + key for mutual TLS. Both or neither.
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
    /// TCP + TLS + auth handshake budget (also the handshake-command
    /// response timeout on single-node).
    pub connect_timeout: Duration,
    /// Operational per-command response timeout once connected. MUST exceed
    /// any blocking-read window the caller uses (`XREADGROUP BLOCK`).
    pub response_timeout: Duration,
}

/// Read a PEM file into bytes, mapping IO errors into a `RedisError` that
/// names the path.
fn read_pem(path: &str) -> RedisResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        redis::RedisError::from((
            ErrorKind::Io,
            "failed to read TLS PEM file",
            format!("{path}: {e}"),
        ))
    })
}

/// Build `TlsCertificates` from the configured PEM paths, or `None` when no
/// custom TLS material is set. mTLS requires BOTH client cert and key.
fn build_tls_certificates(opts: &RedisConnOpts) -> RedisResult<Option<TlsCertificates>> {
    if opts.ca_cert_path.is_none()
        && opts.client_cert_path.is_none()
        && opts.client_key_path.is_none()
    {
        return Ok(None);
    }
    let root_cert = opts.ca_cert_path.as_deref().map(read_pem).transpose()?;
    let client_tls = match (&opts.client_cert_path, &opts.client_key_path) {
        (Some(cert), Some(key)) => Some(ClientTlsConfig {
            client_cert: read_pem(cert)?,
            client_key: read_pem(key)?,
        }),
        (None, None) => None,
        _ => {
            return Err(redis::RedisError::from((
                ErrorKind::InvalidClientConfig,
                "incomplete mTLS config",
                "set BOTH BROWSER_HEADLESS_REDIS_CLIENT_CERT and \
                 BROWSER_HEADLESS_REDIS_CLIENT_KEY (or neither)"
                    .to_string(),
            )));
        }
    };
    Ok(Some(TlsCertificates {
        client_tls,
        root_cert,
    }))
}

/// The Redis client — single-node or cluster — that hands out [`Conn`]s.
pub(crate) enum Backend {
    Single(redis::Client),
    Cluster(ClusterClient),
}

impl Backend {
    pub(crate) fn new(opts: &RedisConnOpts) -> RedisResult<Self> {
        let certs = build_tls_certificates(opts)?;
        if opts.cluster {
            // Comma-separated seed nodes, e.g.
            // "redis://10.0.0.1:6379,redis://10.0.0.2:6379". The cluster
            // client bakes the timeouts into every connection (no
            // per-connection setter), so set them here.
            let nodes: Vec<String> = opts
                .url
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let mut builder = ClusterClientBuilder::new(nodes)
                .connection_timeout(opts.connect_timeout)
                .response_timeout(opts.response_timeout);
            if let Some(certs) = certs {
                builder = builder.certs(certs);
            }
            Ok(Backend::Cluster(builder.build()?))
        } else {
            match certs {
                Some(certs) => Ok(Backend::Single(redis::Client::build_with_tls(
                    opts.url.clone(),
                    certs,
                )?)),
                None => Ok(Backend::Single(redis::Client::open(opts.url.clone())?)),
            }
        }
    }

    /// Open a connection. On single-node, `connect_timeout` bounds the
    /// TCP+TLS+auth handshake (and its command responses); the long
    /// `response_timeout` is then applied for normal operation. Cluster
    /// bakes both in at build time.
    pub(crate) async fn connect(&self, opts: &RedisConnOpts) -> RedisResult<Conn> {
        match self {
            Backend::Single(c) => {
                let conn_cfg = AsyncConnectionConfig::new()
                    .set_connection_timeout(Some(opts.connect_timeout))
                    .set_response_timeout(Some(opts.connect_timeout));
                let mut conn = c
                    .get_multiplexed_async_connection_with_config(&conn_cfg)
                    .await?;
                conn.set_response_timeout(opts.response_timeout);
                Ok(Conn::Single(conn))
            }
            Backend::Cluster(c) => Ok(Conn::Cluster(c.get_async_connection().await?)),
        }
    }
}

/// A Redis connection that is either single-node (multiplexed) or cluster.
/// Both inner types implement [`ConnectionLike`], so `AsyncCommands` — and
/// every command the callers issue — works uniformly through this enum.
/// Cloning shares the underlying connection (both variants are designed for
/// concurrent shared use).
#[derive(Clone)]
pub(crate) enum Conn {
    Single(MultiplexedConnection),
    Cluster(ClusterConnection),
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Conn::Single(c) => c.req_packed_command(cmd),
            Conn::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Conn::Single(c) => c.req_packed_commands(cmd, offset, count),
            Conn::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Conn::Single(c) => c.get_db(),
            Conn::Cluster(c) => c.get_db(),
        }
    }
}
