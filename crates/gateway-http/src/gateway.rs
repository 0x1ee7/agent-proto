//! Gateway state, card refresh, and the library entry point (RFC-0008 4.2, 4.10).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a_client::{Client, Directory};
use a2a_core::envelope::Clock;
use a2a_types::ids::{AgentId, TenantId};
use bus_api::bus::Bus;

use crate::cards::{self, Cards};
use crate::config::{ConfigError, GatewayConfig};
use crate::limits::RateLimiter;

/// Failure to construct a gateway.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration violates RFC-0008 4.1 or 4.9.
    #[error("invalid gateway configuration: {0}")]
    Config(#[from] ConfigError),
    /// The bus client could not be built.
    #[error("bus client could not be built: {0}")]
    Client(#[from] a2a_client::ClientError),
    /// The gateway identity could not be formed.
    #[error("gateway identity is invalid: {0}")]
    Identity(#[from] a2a_types::ids::IdError),
}

/// State shared by every request and the refresh loop.
pub struct Shared<B: Bus> {
    /// Bus client; owned so streaming tasks can hold it across requests.
    pub client: Arc<Client<B>>,
    /// Validated configuration.
    pub config: GatewayConfig,
    /// Derived card snapshot.
    pub cards: Cards,
    /// Per-peer request rate limiter (keyed by peer name).
    pub peer_limit: RateLimiter,
    /// Loopback request rate limiter (keyed by the `loopback` origin).
    pub loopback_limit: RateLimiter,
    /// Failed-authentication limiter (keyed by source address).
    pub failed_auth: RateLimiter,
}

/// The HTTP gateway over a bus `B`. Cheap to clone; clones share state.
pub struct HttpGateway<B: Bus> {
    shared: Arc<Shared<B>>,
}

impl<B: Bus> Clone for HttpGateway<B> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<B: Bus> std::fmt::Debug for HttpGateway<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpGateway")
            .field("listen", &self.shared.config.listen)
            .finish_non_exhaustive()
    }
}

impl<B: Bus + Send + Sync + 'static> HttpGateway<B> {
    /// Validates the configuration and builds the bus client under the gateway identity.
    ///
    /// # Errors
    /// Returns [`StartError`] when the configuration is invalid or the client cannot be built.
    pub fn new(
        bus: B,
        config: GatewayConfig,
        client_config: a2a_client::Config,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StartError> {
        config.validate()?;
        let client = Client::new(
            bus,
            clock,
            client_config,
            TenantId::parse("local")?,
            AgentId::gateway(),
        )?;
        let shared = Shared {
            client: Arc::new(client),
            cards: Cards::new(),
            peer_limit: RateLimiter::new(config.peer_rate),
            loopback_limit: RateLimiter::new(config.loopback_rate),
            failed_auth: RateLimiter::new(config.failed_auth_rate),
            config,
        };
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    /// Rebuilds the card snapshot from the bus directory (0008-018).
    ///
    /// On failure the previous snapshot stays in place.
    ///
    /// # Errors
    /// Returns the bus or wire error of the directory sync.
    pub async fn refresh(&self) -> Result<(), a2a_client::ClientError> {
        let shared = &self.shared;
        let mut directory = Directory::new(&shared.client);
        directory.sync().await?;
        let listings = directory.all(true)?;
        let mut snapshot = HashMap::new();
        for listing in listings {
            let agent = listing.card.agent();
            if !shared.config.is_exposed(agent) {
                continue;
            }
            let serves_peers = shared
                .config
                .peers
                .iter()
                .any(|peer| peer.agents.contains(agent));
            let card = cards::derive(
                listing.card.card(),
                agent,
                &shared.config.public_base_url,
                &shared.config,
                serves_peers,
            );
            snapshot.insert(agent.as_str().to_owned(), Arc::new(cards::view(&card)));
        }
        shared.cards.replace(snapshot);
        Ok(())
    }

    /// Starts the background card refresh loop, running every `refresh_interval`.
    ///
    /// A failed refresh keeps the last good snapshot and retries on the next tick.
    #[must_use]
    pub fn spawn_refresh(&self) -> tokio::task::JoinHandle<()> {
        let gateway = self.clone();
        let period: Duration = gateway.shared.config.refresh_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            loop {
                ticker.tick().await;
                let _ignored = gateway.refresh().await;
            }
        })
    }

    /// Shared state, for the server and operation modules.
    pub(crate) fn shared(&self) -> &Arc<Shared<B>> {
        &self.shared
    }
}
