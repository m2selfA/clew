use std::{collections::BTreeSet, fmt, pin::Pin};

use clew_core::{ControllerId, SiteId};
use iroh::{
    EndpointAddr, EndpointId,
    address_lookup::{AddrFilter, AddressLookup, EndpointData, UserData},
};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_stream::{Stream, StreamExt};

use crate::{IrohOuter, IrohOuterError};

const DISCOVERY_TAG_DOMAIN: &[u8] = b"clew/site-discovery-tag/v1\0";
const MDNS_SERVICE_NAME: &str = "clewv1";
const ADVERTISEMENT_PREFIX: &str = "clew1;c;";
const SITE_TAG_BYTES: usize = 16;
const SITE_TAG_HEX_LEN: usize = SITE_TAG_BYTES * 2;

/// Non-secret, stable equality tag used only to filter same-Site LAN candidates.
///
/// Possessing or advertising this value grants no Clew authority. A peer found by
/// this tag still needs a Controller-signed Connector lease before any tunnel can
/// be used.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SiteDiscoveryTag([u8; SITE_TAG_BYTES]);

impl SiteDiscoveryTag {
    #[must_use]
    pub fn derive(controller_id: ControllerId, site_id: SiteId) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DISCOVERY_TAG_DOMAIN);
        hasher.update(controller_id.as_bytes());
        hasher.update(site_id.as_bytes());
        let digest = hasher.finalize();
        let mut tag = [0_u8; SITE_TAG_BYTES];
        tag.copy_from_slice(&digest[..SITE_TAG_BYTES]);
        Self(tag)
    }

    pub fn parse(value: &str) -> Result<Self, ConnectorDiscoveryError> {
        if value.len() != SITE_TAG_HEX_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ConnectorDiscoveryError::InvalidAdvertisement);
        }
        let mut bytes = [0_u8; SITE_TAG_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for SiteDiscoveryTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SiteDiscoveryTag({self})")
    }
}

impl fmt::Display for SiteDiscoveryTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Bounded mDNS user-data marker. It is deliberately only a candidate hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorDiscoveryAdvertisement {
    pub site_tag: SiteDiscoveryTag,
}

impl ConnectorDiscoveryAdvertisement {
    #[must_use]
    pub fn for_site(controller_id: ControllerId, site_id: SiteId) -> Self {
        Self {
            site_tag: SiteDiscoveryTag::derive(controller_id, site_id),
        }
    }

    pub fn to_user_data(self) -> Result<UserData, ConnectorDiscoveryError> {
        let encoded = format!("{ADVERTISEMENT_PREFIX}{}", self.site_tag);
        debug_assert!(encoded.len() < UserData::MAX_LENGTH);
        UserData::try_from(encoded).map_err(|_| ConnectorDiscoveryError::AdvertisementTooLarge)
    }

    pub fn parse(user_data: &UserData) -> Result<Self, ConnectorDiscoveryError> {
        let raw = user_data.as_ref();
        let tag = raw
            .strip_prefix(ADVERTISEMENT_PREFIX)
            .ok_or(ConnectorDiscoveryError::InvalidAdvertisement)?;
        if tag.contains(';') {
            return Err(ConnectorDiscoveryError::InvalidAdvertisement);
        }
        Ok(Self {
            site_tag: SiteDiscoveryTag::parse(tag)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorCandidate {
    pub addr: EndpointAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectorDiscoveryEvent {
    Candidate(ConnectorCandidate),
    Expired { endpoint_id: EndpointId },
}

/// One Clew-only mDNS lookup service keyed to an existing iroh endpoint id.
///
/// The service is deliberately *not* registered in the endpoint-global iroh
/// AddressLookupServices. Clew Site metadata therefore stays in LAN mDNS TXT
/// records and is never copied into the N0 DNS/Pkarr publishers from the normal
/// endpoint preset.
///
/// Discovery is not authentication. Consumers must verify a Controller-signed
/// Connector lease for every candidate before sending any Clew tunnel bytes.
#[derive(Clone, Debug)]
pub struct MdnsConnectorDiscovery {
    mdns: MdnsAddressLookup,
    own_endpoint_id: EndpointId,
    expected_site_tag: SiteDiscoveryTag,
}

impl MdnsConnectorDiscovery {
    pub fn attach(
        outer: &IrohOuter,
        controller_id: ControllerId,
        site_id: SiteId,
        advertise_connector: bool,
    ) -> Result<Self, ConnectorDiscoveryError> {
        let advertisement = ConnectorDiscoveryAdvertisement::for_site(controller_id, site_id);
        let endpoint_addr = outer.addr();
        let mdns = MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE_NAME)
            .advertise(advertise_connector)
            .addr_filter(AddrFilter::ip_only())
            .build(endpoint_addr.id)
            .map_err(|error| ConnectorDiscoveryError::Mdns(error.to_string()))?;
        if advertise_connector {
            let direct = EndpointData::from_iter(
                endpoint_addr
                    .addrs
                    .iter()
                    .filter(|addr| addr.is_ip())
                    .cloned(),
            )
            .with_user_data(advertisement.to_user_data()?);
            mdns.publish(&direct);
        }
        Ok(Self {
            mdns,
            own_endpoint_id: endpoint_addr.id,
            expected_site_tag: advertisement.site_tag,
        })
    }

    pub async fn subscribe(&self) -> ConnectorDiscoveryEvents {
        ConnectorDiscoveryEvents {
            inner: Box::pin(self.mdns.subscribe().await),
            own_endpoint_id: self.own_endpoint_id,
            expected_site_tag: self.expected_site_tag,
            matched: BTreeSet::new(),
        }
    }
}

pub struct ConnectorDiscoveryEvents {
    inner: Pin<Box<dyn Stream<Item = DiscoveryEvent> + Send>>,
    own_endpoint_id: EndpointId,
    expected_site_tag: SiteDiscoveryTag,
    matched: BTreeSet<EndpointId>,
}

impl ConnectorDiscoveryEvents {
    pub async fn next(&mut self) -> Option<ConnectorDiscoveryEvent> {
        while let Some(event) = self.inner.next().await {
            match event {
                DiscoveryEvent::Discovered { endpoint_info, .. } => {
                    if endpoint_info.endpoint_id == self.own_endpoint_id {
                        continue;
                    }
                    let Some(user_data) = endpoint_info.user_data() else {
                        continue;
                    };
                    let Ok(advertisement) = ConnectorDiscoveryAdvertisement::parse(user_data)
                    else {
                        continue;
                    };
                    if advertisement.site_tag != self.expected_site_tag {
                        continue;
                    }
                    self.matched.insert(endpoint_info.endpoint_id);
                    return Some(ConnectorDiscoveryEvent::Candidate(ConnectorCandidate {
                        addr: endpoint_info.into_endpoint_addr(),
                    }));
                }
                DiscoveryEvent::Expired { endpoint_id } => {
                    if self.matched.remove(&endpoint_id) {
                        return Some(ConnectorDiscoveryEvent::Expired { endpoint_id });
                    }
                }
                _ => {}
            }
        }
        None
    }
}

impl From<IrohOuterError> for ConnectorDiscoveryError {
    fn from(error: IrohOuterError) -> Self {
        Self::Iroh(error.to_string())
    }
}

#[derive(Debug, Error)]
pub enum ConnectorDiscoveryError {
    #[error("connector discovery advertisement is invalid")]
    InvalidAdvertisement,
    #[error("connector discovery advertisement exceeds the iroh user-data bound")]
    AdvertisementTooLarge,
    #[error("iroh mDNS discovery failed: {0}")]
    Mdns(String),
    #[error("iroh connector discovery failed: {0}")]
    Iroh(String),
}

fn hex_nibble(byte: u8) -> Result<u8, ConnectorDiscoveryError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ConnectorDiscoveryError::InvalidAdvertisement),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn advertisement_is_stable_bounded_and_strict() {
        let controller_id = ControllerId::from_bytes([41_u8; 16]).unwrap();
        let site_id = SiteId::from_bytes([42_u8; 16]).unwrap();
        let first = ConnectorDiscoveryAdvertisement::for_site(controller_id, site_id);
        let second = ConnectorDiscoveryAdvertisement::for_site(controller_id, site_id);
        assert_eq!(first, second);
        let encoded = first.to_user_data().unwrap();
        assert!(encoded.as_ref().len() < UserData::MAX_LENGTH);
        assert_eq!(
            ConnectorDiscoveryAdvertisement::parse(&encoded).unwrap(),
            first
        );
        assert_eq!(first.site_tag.to_string().len(), SITE_TAG_HEX_LEN);

        for invalid in [
            "clew1;c;ABCDEF0123456789ABCDEF0123456789",
            "clew1;c;1234",
            "clew2;c;0123456789abcdef0123456789abcdef",
            "clew1;c;0123456789abcdef0123456789abcdef;extra",
        ] {
            let data = UserData::try_from(invalid.to_owned()).unwrap();
            assert!(ConnectorDiscoveryAdvertisement::parse(&data).is_err());
        }
    }

    #[tokio::test]
    async fn real_mdns_discovers_only_same_site_and_connects() {
        let controller_id = ControllerId::from_bytes([51_u8; 16]).unwrap();
        let wanted_site = SiteId::from_bytes([52_u8; 16]).unwrap();
        let wrong_site = SiteId::from_bytes([53_u8; 16]).unwrap();

        let listener_outer = IrohOuter::bind_direct_only().await.unwrap();
        let listener =
            MdnsConnectorDiscovery::attach(&listener_outer, controller_id, wanted_site, false)
                .unwrap();
        let mut events = listener.subscribe().await;

        let wrong_outer = IrohOuter::bind_direct_only().await.unwrap();
        let _wrong =
            MdnsConnectorDiscovery::attach(&wrong_outer, controller_id, wrong_site, true).unwrap();

        let helper_outer = IrohOuter::bind_direct_only().await.unwrap();
        let _helper =
            MdnsConnectorDiscovery::attach(&helper_outer, controller_id, wanted_site, true)
                .unwrap();

        let candidate = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ConnectorDiscoveryEvent::Candidate(candidate)) = events.next().await {
                    break candidate;
                }
            }
        })
        .await
        .expect("same-site Connector was not discovered over mDNS");
        assert_eq!(candidate.addr.id, helper_outer.addr().id);
        assert_ne!(candidate.addr.id, wrong_outer.addr().id);

        let helper_task = tokio::spawn({
            let helper_outer = helper_outer.clone();
            async move {
                let (protocol, mut stream) = helper_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, crate::IrohProtocol::Connector);
                let mut ping = [0_u8; 4];
                stream.read_exact(&mut ping).await.unwrap();
                assert_eq!(&ping, b"ping");
                stream.write_all(b"pong").await.unwrap();
                let mut ack = [0_u8; 1];
                stream.read_exact(&mut ack).await.unwrap();
                assert_eq!(&ack, b"!");
                stream
            }
        });
        let mut stream = listener_outer
            .connect_connector(candidate.addr)
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut pong = [0_u8; 4];
        stream.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        stream.write_all(b"!").await.unwrap();
        let helper_stream = helper_task.await.unwrap();
        drop(helper_stream);
        drop(stream);

        listener_outer.close().await;
        wrong_outer.close().await;
        helper_outer.close().await;
    }
}
