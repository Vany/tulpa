use std::error::Error;
use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::Endpoint;

const DEFAULT_BROWSE_TIMEOUT: Duration = Duration::from_millis(1_500);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Runtime settings for LAN discovery and configured tunnel fallback.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DiscoveryConfig {
    /// The Bonjour service type, including its `._tcp.local.` suffix.
    pub mdns_service: String,
    /// All SSH tunnel entry points, supplied by local configuration.
    pub tunnel_entry_points: Vec<Endpoint>,
    /// Maximum time spent collecting Bonjour results.
    pub browse_timeout: Duration,
    /// Maximum time for each TCP connection attempt.
    pub probe_timeout: Duration,
}

impl DiscoveryConfig {
    /// Creates discovery settings with bounded default timeouts.
    #[must_use]
    pub fn new(mdns_service: impl Into<String>, tunnel_entry_points: Vec<Endpoint>) -> Self {
        Self {
            mdns_service: mdns_service.into(),
            tunnel_entry_points,
            browse_timeout: DEFAULT_BROWSE_TIMEOUT,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

/// The source of a live endpoint selected at runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Route {
    /// A live endpoint resolved through LAN Bonjour/mDNS.
    LanBonjour,
    /// A live endpoint from the locally configured tunnel list.
    ConfiguredTunnel,
}

/// A live endpoint and the route used to find it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SelectedEndpoint {
    /// The address selected by this discovery run.
    pub endpoint: Endpoint,
    /// Whether the address came from Bonjour or configured tunnels.
    pub route: Route,
}

/// Discovers and probes a server on every call. Live Bonjour candidates win;
/// configured tunnel entry points are considered only if none answer.
///
/// A successful TCP connection establishes liveness, not trust. SSH must still
/// verify the configured host-key pin before authenticating.
///
/// # Errors
///
/// Returns an error for invalid discovery settings, mDNS failures, or when no
/// candidate accepts a TCP connection within the configured timeouts.
pub fn discover(config: &DiscoveryConfig) -> Result<SelectedEndpoint, DiscoveryError> {
    if config.browse_timeout.is_zero() || config.probe_timeout.is_zero() {
        return Err(DiscoveryError::InvalidTimeout);
    }
    let lan = browse_lan(config)?;
    select_live_or_tunnels(
        lan,
        config.tunnel_entry_points.clone(),
        config.probe_timeout,
        |endpoint| probe_tcp(endpoint, config.probe_timeout),
    )
}

fn browse_lan(config: &DiscoveryConfig) -> Result<Vec<Endpoint>, DiscoveryError> {
    if !config.mdns_service.ends_with("._tcp.local.") {
        return Err(DiscoveryError::InvalidServiceType);
    }
    let daemon = ServiceDaemon::new().map_err(|error| DiscoveryError::Mdns(error.to_string()))?;
    let events = daemon
        .browse(&config.mdns_service)
        .map_err(|error| DiscoveryError::Mdns(error.to_string()))?;
    let deadline = Instant::now()
        .checked_add(config.browse_timeout)
        .ok_or(DiscoveryError::InvalidTimeout)?;
    let mut endpoints = Vec::new();
    let browse_result = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break Ok(());
        };
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(service)) => {
                if service.port == 0 {
                    continue;
                }
                endpoints.extend(
                    service
                        .addresses
                        .iter()
                        .map(|address| Endpoint::new(address.to_string(), service.port)),
                );
            }
            Ok(_) => {}
            Err(flume::RecvTimeoutError::Timeout) => break Ok(()),
            Err(flume::RecvTimeoutError::Disconnected) => {
                break Err(DiscoveryError::Mdns(
                    "the mDNS event channel disconnected".to_owned(),
                ));
            }
        }
    };
    let stop_result = daemon
        .stop_browse(&config.mdns_service)
        .map_err(|error| DiscoveryError::Mdns(error.to_string()));
    let shutdown_result = daemon
        .shutdown()
        .map_err(|error| DiscoveryError::Mdns(error.to_string()));
    browse_result?;
    stop_result?;
    shutdown_result?;
    endpoints.sort_by(|left, right| {
        (&left.host, left.port)
            .cmp(&(&right.host, right.port))
            .reverse()
    });
    endpoints.dedup();
    Ok(endpoints)
}

fn select_live_or_tunnels(
    lan: Vec<Endpoint>,
    tunnels: Vec<Endpoint>,
    timeout: Duration,
    mut probe: impl FnMut(&Endpoint) -> bool,
) -> Result<SelectedEndpoint, DiscoveryError> {
    if timeout.is_zero() {
        return Err(DiscoveryError::InvalidTimeout);
    }
    let lan_candidates = lan.len();
    let tunnel_candidates = tunnels.len();
    if let Some(endpoint) = select_live(lan, &mut probe) {
        return Ok(SelectedEndpoint {
            endpoint,
            route: Route::LanBonjour,
        });
    }
    if let Some(endpoint) = select_live(tunnels, &mut probe) {
        return Ok(SelectedEndpoint {
            endpoint,
            route: Route::ConfiguredTunnel,
        });
    }
    Err(DiscoveryError::NoLiveEndpoint {
        lan_candidates,
        tunnel_candidates,
    })
}

fn select_live(
    candidates: Vec<Endpoint>,
    mut probe: impl FnMut(&Endpoint) -> bool,
) -> Option<Endpoint> {
    candidates
        .into_iter()
        .filter(|endpoint| endpoint.port != 0 && !endpoint.host.trim().is_empty())
        .find(|endpoint| probe(endpoint))
}

fn probe_tcp(endpoint: &Endpoint, timeout: Duration) -> bool {
    let Ok(addresses) = (endpoint.host.as_str(), endpoint.port).to_socket_addrs() else {
        return false;
    };
    addresses
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, timeout).is_ok())
}

/// A discovery configuration, mDNS, or live-probe failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiscoveryError {
    /// The Bonjour service type must end in `._tcp.local.`.
    InvalidServiceType,
    /// Discovery and probe timeouts must be nonzero.
    InvalidTimeout,
    /// The mDNS adapter failed. Addresses are deliberately omitted.
    Mdns(String),
    /// No discovered or configured endpoint accepted a connection.
    NoLiveEndpoint {
        /// The number of resolved LAN candidates, before probing.
        lan_candidates: usize,
        /// The number of configured tunnel candidates, before probing.
        tunnel_candidates: usize,
    },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidServiceType => {
                formatter.write_str("Bonjour service type must end in ._tcp.local.")
            }
            Self::InvalidTimeout => formatter.write_str("discovery timeouts must be nonzero"),
            Self::Mdns(error) => write!(formatter, "Bonjour discovery failed: {error}"),
            Self::NoLiveEndpoint {
                lan_candidates,
                tunnel_candidates,
            } => write!(
                formatter,
                "no live SSH endpoint ({lan_candidates} LAN candidates, {tunnel_candidates} tunnel candidates)"
            ),
        }
    }
}

impl Error for DiscoveryError {}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn endpoint(name: &str) -> Endpoint {
        Endpoint::new(name, 22)
    }

    #[test]
    fn live_lan_candidate_prevents_tunnel_probes() {
        let mut probed = Vec::new();
        let selected = select_live_or_tunnels(
            vec![endpoint("lan")],
            vec![endpoint("tunnel")],
            Duration::from_millis(10),
            |candidate| {
                probed.push(candidate.host.clone());
                true
            },
        );

        assert_eq!(
            selected.map(|selected| selected.route),
            Ok(Route::LanBonjour)
        );
        assert_eq!(probed, ["lan"]);
    }

    #[test]
    fn many_tunnels_are_probed_until_one_is_live() {
        let selected = select_live_or_tunnels(
            Vec::new(),
            vec![endpoint("first"), endpoint("second"), endpoint("third")],
            Duration::from_millis(10),
            |candidate| candidate.host == "second",
        );

        assert_eq!(
            selected.map(|selected| selected.endpoint),
            Ok(endpoint("second"))
        );
    }

    #[test]
    fn tcp_probe_uses_current_listeners() -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let live = Endpoint::new("127.0.0.1", listener.local_addr()?.port());
        let closed_listener = TcpListener::bind("127.0.0.1:0")?;
        let closed = Endpoint::new("127.0.0.1", closed_listener.local_addr()?.port());
        drop(closed_listener);

        let selected = select_live_or_tunnels(
            Vec::new(),
            vec![closed, live.clone()],
            Duration::from_millis(100),
            |candidate| probe_tcp(candidate, Duration::from_millis(100)),
        )?;

        assert_eq!(selected.endpoint, live);
        assert_eq!(selected.route, Route::ConfiguredTunnel);
        Ok(())
    }

    #[test]
    #[ignore = "advertises an ephemeral loopback Bonjour service on the local LAN"]
    fn resolves_a_real_bonjour_service() -> Result<(), Box<dyn std::error::Error>> {
        use mdns_sd::ServiceInfo;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let service_type = "_tulpa-spike._tcp.local.";
        let instance = format!("tulpa-spike-{}", std::process::id());
        let service = ServiceInfo::new(
            service_type,
            &instance,
            "localhost.local.",
            "127.0.0.1",
            port,
            None,
        )?;
        let fullname = service.get_fullname().to_owned();
        let advertiser = ServiceDaemon::new()?;
        advertiser.register(service)?;
        std::thread::sleep(Duration::from_millis(250));

        let result = discover(&DiscoveryConfig::new(service_type, Vec::new()));
        let unregister = advertiser.unregister(&fullname)?;
        let _unregister_result = unregister.recv_timeout(Duration::from_secs(1));
        let shutdown = advertiser.shutdown()?;
        let _shutdown_result = shutdown.recv_timeout(Duration::from_secs(1));
        let selected = result?;

        assert_eq!(selected.route, Route::LanBonjour);
        assert_eq!(selected.endpoint.port, port);
        Ok(())
    }
}
