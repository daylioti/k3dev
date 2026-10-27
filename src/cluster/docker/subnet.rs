//! Keeps k3dev's Docker networks reachable from the host.
//!
//! Docker only avoids subnets that overlap routes in the main routing table.
//! VPN clients such as Tailscale install routes in their own tables, which
//! policy rules consult before main, so Docker can hand out a subnet whose
//! traffic the host then sends into the VPN instead of to the bridge, and
//! published ports stop reaching the containers.

use anyhow::{anyhow, Context, Result};
use bollard::models::{Ipam, IpamConfig, NetworkCreateRequest};
use bollard::query_parameters::InspectNetworkOptions;
use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;

use super::DockerManager;

/// Network option naming the bridge interface; absent, Docker uses `br-<id>`
const BRIDGE_NAME_OPTION: &str = "com.docker.network.bridge.name";

/// An IPv4 prefix such as `172.20.0.0/16`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4Net {
    addr: Ipv4Addr,
    len: u8,
}

impl Ipv4Net {
    /// Parse `a.b.c.d/len`, or a bare address as a /32
    fn parse(s: &str) -> Option<Self> {
        let (addr, len) = match s.split_once('/') {
            Some((addr, len)) => (addr, len.parse().ok()?),
            None => (s, 32),
        };
        if len > 32 {
            return None;
        }
        Some(Self {
            addr: addr.parse().ok()?,
            len,
        })
    }

    fn mask(len: u8) -> u32 {
        u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0)
    }

    /// Whether the two prefixes share any address
    fn overlaps(&self, other: &Ipv4Net) -> bool {
        let mask = Self::mask(self.len.min(other.len));
        u32::from(self.addr) & mask == u32::from(other.addr) & mask
    }

    /// The address Docker gives the first container: the one after the gateway
    fn first_container_addr(&self) -> Ipv4Addr {
        Ipv4Addr::from((u32::from(self.addr) & Self::mask(self.len)).saturating_add(2))
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

/// Where the host sends traffic for an address, per `ip route get`
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Route {
    device: String,
    table: Option<String>,
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.table {
            Some(table) => write!(f, "{} (table {})", self.device, table),
            None => write!(f, "{}", self.device),
        }
    }
}

impl DockerManager {
    /// The subnet of a bridge network, with the route that sends host traffic
    /// for its containers somewhere other than its bridge. None when the bridge
    /// gets that traffic, or when that cannot be told: the bridge lives outside
    /// this host's network namespace (remote, rootless or VM-backed Docker), or
    /// `ip` is unavailable.
    pub(crate) async fn network_shadowed_by(&self, name: &str) -> Option<(Ipv4Net, Route)> {
        let network = self
            .client
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
            .ok()?;
        let bridge = match network
            .options
            .as_ref()
            .and_then(|options| options.get(BRIDGE_NAME_OPTION))
        {
            Some(bridge) => bridge.clone(),
            None => format!("br-{}", network.id.as_deref()?.get(..12)?),
        };
        if !Path::new("/sys/class/net").join(&bridge).exists() {
            return None;
        }
        let subnet = network
            .ipam?
            .config?
            .iter()
            .find_map(|config| Ipv4Net::parse(config.subnet.as_deref()?))?;
        let route = route_to(subnet.first_container_addr()).await?;
        (route.device != bridge).then_some((subnet, route))
    }

    /// Recreate a network that nothing is attached to yet on a subnet no host
    /// route overlaps
    pub(super) async fn recreate_on_free_subnet(
        &self,
        name: &str,
        shadowed: Ipv4Net,
        route: &Route,
    ) -> Result<()> {
        let routes = host_routes().await.context("Failed to read host routes")?;
        self.remove_network(name).await?;

        for subnet in free_subnets(&routes) {
            let request = NetworkCreateRequest {
                name: name.to_string(),
                ipam: Some(Ipam {
                    config: Some(vec![IpamConfig {
                        subnet: Some(subnet.to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            // A network created since the routes were read may hold the subnet
            match self.client.create_network(request).await {
                Ok(_) => {
                    tracing::info!(
                        "Network {} moved from {} (routed via {}) to {}",
                        name,
                        shadowed,
                        route,
                        subnet
                    );
                    return Ok(());
                }
                Err(e) => tracing::debug!("Subnet {} unavailable for {}: {}", subnet, name, e),
            }
        }

        Err(anyhow!(
            "Docker gave network {} subnet {}, which the host routes via {}, \
             and no subnet in Docker's default pools is free",
            name,
            shadowed,
            route
        ))
    }
}

/// Destination prefixes of the routes in every table, None when `ip` fails
async fn host_routes() -> Option<Vec<Ipv4Net>> {
    let output = ip(&["-4", "route", "show", "table", "all"]).await?;
    Some(parse_route_prefixes(&output))
}

/// Where the host sends traffic for `addr`
async fn route_to(addr: Ipv4Addr) -> Option<Route> {
    parse_route_get(&ip(&["-4", "route", "get", &addr.to_string()]).await?)
}

async fn ip(args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(args)
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Subnets of Docker's default local address pools, in its allocation order,
/// that overlap none of `routes`. Any overlap counts, whichever table or rule
/// the route sits behind: there are plenty of candidates to spare.
fn free_subnets(routes: &[Ipv4Net]) -> Vec<Ipv4Net> {
    let wide = (17..=31).map(|octet| Ipv4Net {
        addr: Ipv4Addr::new(172, octet, 0, 0),
        len: 16,
    });
    let narrow = (0..16).map(|i| Ipv4Net {
        addr: Ipv4Addr::new(192, 168, i * 16, 0),
        len: 20,
    });
    wide.chain(narrow)
        .filter(|candidate| !routes.iter().any(|route| route.overlaps(candidate)))
        .collect()
}

/// Route types that `ip route` prints before the destination; plain unicast
/// routes carry none
const ROUTE_TYPES: &[&str] = &[
    "unicast",
    "local",
    "broadcast",
    "multicast",
    "throw",
    "unreachable",
    "blackhole",
    "prohibit",
    "nat",
    "anycast",
];

/// Destination prefixes of `ip -4 route show table all` output
fn parse_route_prefixes(output: &str) -> Vec<Ipv4Net> {
    output
        .lines()
        // Indented lines are the nexthops of the route above
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let first = words.next()?;
            let destination = if ROUTE_TYPES.contains(&first) {
                // A throw route only hands the lookup on to the next table
                if first == "throw" {
                    return None;
                }
                words.next()?
            } else {
                first
            };
            // Every subnet overlaps a default route
            Ipv4Net::parse(destination).filter(|prefix| prefix.len > 0)
        })
        .collect()
}

/// The device and table of `ip -4 route get` output
fn parse_route_get(output: &str) -> Option<Route> {
    let words: Vec<&str> = output.lines().next()?.split_whitespace().collect();
    let value_of = |key: &str| {
        let i = words.iter().position(|word| *word == key)?;
        words.get(i + 1).map(|value| value.to_string())
    };
    Some(Route {
        device: value_of("dev")?,
        table: value_of("table"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        Ipv4Net::parse(s).unwrap()
    }

    #[test]
    fn parses_prefixes_and_bare_addresses() {
        assert_eq!(net("172.20.0.0/16").to_string(), "172.20.0.0/16");
        assert_eq!(net("10.1.2.3").to_string(), "10.1.2.3/32");
        assert_eq!(Ipv4Net::parse("172.20.0.0/33"), None);
        assert_eq!(Ipv4Net::parse("default"), None);
    }

    #[test]
    fn overlap_is_symmetric_and_covers_nesting() {
        let docker = net("172.20.0.0/16");
        assert!(docker.overlaps(&net("172.20.0.0/24")));
        assert!(net("172.20.0.0/24").overlaps(&docker));
        assert!(docker.overlaps(&net("172.16.0.0/12")));
        assert!(!docker.overlaps(&net("172.21.0.0/16")));
        assert!(!net("192.168.0.0/20").overlaps(&net("192.168.16.0/20")));
    }

    #[test]
    fn first_container_addr_follows_the_gateway() {
        assert_eq!(
            net("172.20.0.0/16").first_container_addr(),
            Ipv4Addr::new(172, 20, 0, 2)
        );
        assert_eq!(
            net("192.168.32.0/20").first_container_addr(),
            Ipv4Addr::new(192, 168, 32, 2)
        );
    }

    #[test]
    fn route_prefixes_come_from_every_table() {
        let output = "\
default via 192.168.1.1 dev eth0 proto dhcp src 192.168.1.10 metric 100
172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1
172.18.0.0/16 dev br-0123456789ab proto kernel scope link src 172.18.0.1 linkdown
192.168.1.0/24 dev eth0 proto kernel scope link src 192.168.1.10 metric 100
172.20.0.0/24 dev tailscale0 table 52
100.100.100.100 dev tailscale0 table 52
throw 172.21.0.0/16 table 52
unreachable 172.22.0.0/16 table 99
10.9.0.0/16 proto static
\tnexthop via 192.168.1.2 dev eth0 weight 1
\tnexthop via 192.168.1.3 dev eth0 weight 1
local 127.0.0.0/8 dev lo table local proto kernel scope host src 127.0.0.1
broadcast 172.17.255.255 dev docker0 table local proto kernel scope link src 172.17.0.1
";
        let prefixes: Vec<String> = parse_route_prefixes(output)
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(
            prefixes,
            [
                "172.17.0.0/16",
                "172.18.0.0/16",
                "192.168.1.0/24",
                "172.20.0.0/24",
                "100.100.100.100/32",
                "172.22.0.0/16",
                "10.9.0.0/16",
                "127.0.0.0/8",
                "172.17.255.255/32",
            ]
        );
    }

    #[test]
    fn free_subnets_skip_routed_ranges_in_docker_order() {
        let routes = [
            net("172.17.0.0/16"),
            net("172.18.0.0/16"),
            // A VPN route in its own table, narrower than a Docker /16
            net("172.19.0.0/24"),
        ];
        let free = free_subnets(&routes);
        assert_eq!(free.first(), Some(&net("172.20.0.0/16")));
        assert!(!free.contains(&net("172.19.0.0/16")));
    }

    #[test]
    fn free_subnets_fall_back_to_the_192_168_pool() {
        let free = free_subnets(&[net("172.16.0.0/12"), net("192.168.1.0/24")]);
        assert_eq!(free.first(), Some(&net("192.168.16.0/20")));
        assert_eq!(free.len(), 15);
    }

    #[test]
    fn route_get_reports_the_device_and_table() {
        let output = "172.20.0.2 dev tailscale0 table 52 src 100.64.0.1 uid 1000 \n    cache \n";
        let route = parse_route_get(output).unwrap();
        assert_eq!(route.device, "tailscale0");
        assert_eq!(route.to_string(), "tailscale0 (table 52)");

        let output = "172.20.0.2 dev br-0123456789ab src 172.20.0.1 uid 1000 \n    cache \n";
        assert_eq!(
            parse_route_get(output).unwrap().to_string(),
            "br-0123456789ab"
        );

        let output = "1.1.1.1 via 192.168.1.1 dev eth0 src 192.168.1.10 uid 1000 \n";
        assert_eq!(parse_route_get(output).unwrap().device, "eth0");

        assert_eq!(parse_route_get(""), None);
    }
}
