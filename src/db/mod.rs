pub mod collector;
pub mod dump;
pub mod queries;
pub mod schedule;
pub mod sql;
pub mod version;

use mysql_async::{Opts, OptsBuilder, PoolConstraints, PoolOpts, SslOpts};
use serde::{Deserialize, Serialize};

/// A saved connection target. Password is kept in memory only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    pub database: Option<String>,
    /// TLS. MySQL 8 defaults to `caching_sha2_password`, which refuses to send
    /// the password over a plaintext link unless the server's RSA key is used;
    /// enabling TLS avoids that path entirely.
    pub use_tls: bool,
    /// Skip server certificate verification (self-signed dev servers).
    pub tls_skip_verify: bool,
    /// Poll interval in milliseconds.
    pub interval_ms: u64,
    /// Upper bound on pooled connections. A monitor must not be the reason a
    /// busy server runs out of them.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_max_connections() -> usize {
    3
}

impl Default for ConnConfig {
    fn default() -> Self {
        Self {
            name: "local".into(),
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            database: None,
            use_tls: false,
            tls_skip_verify: true,
            interval_ms: 1000,
            max_connections: default_max_connections(),
        }
    }
}

impl ConnConfig {
    pub fn to_opts(&self) -> Opts {
        let mut b = OptsBuilder::default()
            .ip_or_hostname(self.host.clone())
            .tcp_port(self.port)
            .user(Some(self.user.clone()))
            .pass(Some(self.password.clone()))
            .db_name(self.database.clone())
            // A monitor issues the same handful of statements forever; the
            // server-side prepared-statement cache buys nothing here.
            .stmt_cache_size(0)
            .prefer_socket(false)
            // Keep one warm connection and cap the rest: the sampler, the
            // console and the browser share this pool, and an unbounded one
            // would let a slow server accumulate connections on every tick.
            .pool_opts(
                PoolOpts::default().with_constraints(
                    PoolConstraints::new(1, self.max_connections.clamp(1, 16))
                        .unwrap_or_else(|| PoolConstraints::new(1, 3).expect("valid constraints")),
                ),
            );

        if self.use_tls {
            b = b.ssl_opts(Some(
                SslOpts::default()
                    .with_danger_accept_invalid_certs(self.tls_skip_verify)
                    .with_danger_skip_domain_validation(self.tls_skip_verify),
            ));
        }
        Opts::from(b)
    }

    pub fn label(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}
