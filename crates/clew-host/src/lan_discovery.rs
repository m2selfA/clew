use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use clew_core::{ControllerId, SiteId};
use clew_transport::{ConnectorCandidate, IrohOuter, MAX_NEARBY_CONNECTOR_ADDRS, SiteDiscoveryTag};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use tokio::{net::UdpSocket, task::JoinHandle, time::Instant};

const LAN_DISCOVERY_VERSION: u32 = 1;
const LAN_DISCOVERY_PROBE_KIND: &str = "clew_lan_connector_probe";
const LAN_DISCOVERY_REPLY_KIND: &str = "clew_lan_connector_reply";
const LAN_DISCOVERY_PORT_BASE: u16 = 42_100;
const LAN_DISCOVERY_PORT_SPAN: u16 = 1_000;
const MAX_LAN_DISCOVERY_DATAGRAM_BYTES: usize = 4 * 1024;
const LAN_DISCOVERY_WINDOW: Duration = Duration::from_millis(900);
const MAX_LAN_DISCOVERY_CANDIDATES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanConnectorProbe {
    version: u32,
    kind: String,
    site_tag: String,
    nonce: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanConnectorReply {
    version: u32,
    kind: String,
    site_tag: String,
    nonce: String,
    candidate: EndpointAddr,
}

pub(crate) struct LanConnectorResponder {
    task: JoinHandle<()>,
}

impl LanConnectorResponder {
    pub(crate) async fn start(
        outer: IrohOuter,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> Option<Self> {
        let site_tag = SiteDiscoveryTag::derive(controller_id, site_id);
        let port = discovery_port(site_tag);
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
            .await
            .ok()?;
        let local_networks = local_ipv4_networks();
        let task = tokio::spawn(async move {
            let mut input = vec![0_u8; MAX_LAN_DISCOVERY_DATAGRAM_BYTES + 1];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut input).await else {
                    return;
                };
                if len == 0
                    || len > MAX_LAN_DISCOVERY_DATAGRAM_BYTES
                    || !source_is_local(peer, &local_networks)
                {
                    continue;
                }
                let Ok(probe) = serde_json::from_slice::<LanConnectorProbe>(&input[..len]) else {
                    continue;
                };
                if !valid_probe(&probe, site_tag) {
                    continue;
                }
                let Some(candidate) = direct_candidate(outer.addr()) else {
                    continue;
                };
                let reply = LanConnectorReply {
                    version: LAN_DISCOVERY_VERSION,
                    kind: LAN_DISCOVERY_REPLY_KIND.into(),
                    site_tag: site_tag.to_string(),
                    nonce: probe.nonce,
                    candidate,
                };
                let Ok(encoded) = serde_json::to_vec(&reply) else {
                    continue;
                };
                if encoded.is_empty() || encoded.len() > MAX_LAN_DISCOVERY_DATAGRAM_BYTES {
                    continue;
                }
                let _ = socket.send_to(&encoded, peer).await;
            }
        });
        Some(Self { task })
    }
}

impl Drop for LanConnectorResponder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn discover_lan_connector_candidates(
    controller_id: ControllerId,
    site_id: SiteId,
) -> Vec<ConnectorCandidate> {
    let site_tag = SiteDiscoveryTag::derive(controller_id, site_id);
    let targets = broadcast_targets(discovery_port(site_tag));
    discover_lan_connector_candidates_to(site_tag, targets, LAN_DISCOVERY_WINDOW)
        .await
        .unwrap_or_default()
}

async fn discover_lan_connector_candidates_to(
    site_tag: SiteDiscoveryTag,
    targets: Vec<SocketAddr>,
    window: Duration,
) -> std::io::Result<Vec<ConnectorCandidate>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let nonce = random_nonce()?;
    let probe = LanConnectorProbe {
        version: LAN_DISCOVERY_VERSION,
        kind: LAN_DISCOVERY_PROBE_KIND.into(),
        site_tag: site_tag.to_string(),
        nonce: nonce.clone(),
    };
    let encoded = serde_json::to_vec(&probe).map_err(std::io::Error::other)?;
    if encoded.is_empty() || encoded.len() > MAX_LAN_DISCOVERY_DATAGRAM_BYTES {
        return Ok(Vec::new());
    }
    for target in targets {
        let _ = socket.send_to(&encoded, target).await;
    }

    let deadline = Instant::now() + window;
    let mut input = vec![0_u8; MAX_LAN_DISCOVERY_DATAGRAM_BYTES + 1];
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    while candidates.len() < MAX_LAN_DISCOVERY_CANDIDATES {
        let received = tokio::time::timeout_at(deadline, socket.recv_from(&mut input)).await;
        let Ok(Ok((len, _peer))) = received else {
            break;
        };
        if len == 0 || len > MAX_LAN_DISCOVERY_DATAGRAM_BYTES {
            continue;
        }
        let Ok(reply) = serde_json::from_slice::<LanConnectorReply>(&input[..len]) else {
            continue;
        };
        if !valid_reply(&reply, site_tag, &nonce) {
            continue;
        }
        let Some(candidate) = direct_candidate(reply.candidate) else {
            continue;
        };
        if seen.insert(candidate.id) {
            candidates.push(ConnectorCandidate { addr: candidate });
        }
    }
    Ok(candidates)
}

fn valid_probe(probe: &LanConnectorProbe, expected: SiteDiscoveryTag) -> bool {
    probe.version == LAN_DISCOVERY_VERSION
        && probe.kind == LAN_DISCOVERY_PROBE_KIND
        && probe.site_tag == expected.to_string()
        && valid_nonce(&probe.nonce)
}

fn valid_reply(reply: &LanConnectorReply, expected: SiteDiscoveryTag, nonce: &str) -> bool {
    reply.version == LAN_DISCOVERY_VERSION
        && reply.kind == LAN_DISCOVERY_REPLY_KIND
        && reply.site_tag == expected.to_string()
        && reply.nonce == nonce
        && valid_nonce(&reply.nonce)
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn random_nonce() -> std::io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        std::io::Error::other(format!("secure random generation failed: {error}"))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn discovery_port(site_tag: SiteDiscoveryTag) -> u16 {
    let tag = site_tag.to_string();
    let prefix = u16::from_str_radix(&tag[..4], 16).unwrap_or(0);
    LAN_DISCOVERY_PORT_BASE + (prefix % LAN_DISCOVERY_PORT_SPAN)
}

fn direct_candidate(addr: EndpointAddr) -> Option<EndpointAddr> {
    let mut direct = EndpointAddr {
        id: addr.id,
        addrs: Default::default(),
    };
    for address in addr.addrs {
        if address.is_ip() {
            direct.addrs.insert(address);
            if direct.addrs.len() >= MAX_NEARBY_CONNECTOR_ADDRS {
                break;
            }
        }
    }
    (!direct.addrs.is_empty()).then_some(direct)
}

fn local_ipv4_networks() -> Vec<netdev::ipnet::Ipv4Net> {
    netdev::get_interfaces()
        .into_iter()
        .flat_map(|interface| interface.ipv4)
        .filter(|network| !network.addr().is_unspecified())
        .collect()
}

fn source_is_local(peer: SocketAddr, networks: &[netdev::ipnet::Ipv4Net]) -> bool {
    match peer.ip() {
        std::net::IpAddr::V4(ip) if ip.is_loopback() => true,
        std::net::IpAddr::V4(ip) => networks.iter().any(|network| network.contains(&ip)),
        std::net::IpAddr::V6(_) => false,
    }
}

fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut targets = BTreeSet::new();
    targets.insert(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port)));
    for network in local_ipv4_networks() {
        let ip = network.addr();
        if ip.is_loopback() || ip.is_unspecified() || network.prefix_len() >= 31 {
            continue;
        }
        targets.insert(SocketAddr::V4(SocketAddrV4::new(network.broadcast(), port)));
    }
    targets.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use clew_transport::IrohOuter;

    use super::*;

    #[test]
    fn discovery_port_is_stable_bounded_and_site_specific() {
        let controller = ControllerId::from_bytes([0x51; 16]).unwrap();
        let first = SiteId::from_bytes([0x52; 16]).unwrap();
        let second = SiteId::from_bytes([0x53; 16]).unwrap();
        let first_port = discovery_port(SiteDiscoveryTag::derive(controller, first));
        let second_port = discovery_port(SiteDiscoveryTag::derive(controller, second));
        assert!(
            (LAN_DISCOVERY_PORT_BASE..LAN_DISCOVERY_PORT_BASE + LAN_DISCOVERY_PORT_SPAN)
                .contains(&first_port)
        );
        assert_ne!(first_port, second_port);
        assert_eq!(
            first_port,
            discovery_port(SiteDiscoveryTag::derive(controller, first))
        );
    }

    #[tokio::test]
    async fn unicast_probe_discovers_only_same_site_candidate() {
        let controller = ControllerId::from_bytes([0x61; 16]).unwrap();
        let site = SiteId::from_bytes([0x62; 16]).unwrap();
        let wrong_site = SiteId::from_bytes([0x63; 16]).unwrap();
        let outer = IrohOuter::bind_direct_only().await.unwrap();
        let responder = LanConnectorResponder::start(outer.clone(), controller, site)
            .await
            .expect("LAN responder did not bind");
        let target = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            discovery_port(SiteDiscoveryTag::derive(controller, site)),
        );
        let found = discover_lan_connector_candidates_to(
            SiteDiscoveryTag::derive(controller, site),
            vec![target],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addr.id, outer.addr().id);

        let wrong = discover_lan_connector_candidates_to(
            SiteDiscoveryTag::derive(controller, wrong_site),
            vec![target],
            Duration::from_millis(150),
        )
        .await
        .unwrap();
        assert!(wrong.is_empty());
        drop(responder);
        outer.close().await;
    }
}
