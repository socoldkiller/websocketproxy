#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use futures_util::TryStreamExt;
#[cfg(target_os = "linux")]
use rtnetlink::{
    Handle, LinkBridge, LinkUnspec, RouteMessageBuilder,
    packet_route::{
        AddressFamily,
        address::{AddressAttribute, AddressMessage},
        link::{LinkAttribute, LinkMessage},
        route::{RouteAddress, RouteAttribute, RouteMessage},
    },
};
#[cfg(target_os = "linux")]
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_os = "linux")]
use tracing::info;

#[cfg(target_os = "linux")]
use super::{link_by_name, new_netlink_handle};

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct SavedIpv4Address {
    message: AddressMessage,
    address: Ipv4Addr,
    prefix_len: u8,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct DefaultRoute {
    gateway: Option<Ipv4Addr>,
    metric: Option<u32>,
}

#[cfg(target_os = "linux")]
pub(super) async fn configure_bridge_mode(cli: &crate::Cli, tap_name: &str) -> Result<()> {
    let uplink_if = cli
        .uplink_if
        .as_deref()
        .context("bridge mode requires --uplink-if")?;
    let handle = new_netlink_handle()?;

    let uplink_link = link_by_name(&handle, uplink_if).await?;
    let tap_link = link_by_name(&handle, tap_name).await?;
    let bridge_link = ensure_bridge(&handle, &cli.bridge_name, link_mac(&uplink_link)).await?;
    let uplink_addresses = ipv4_addresses_for_link(&handle, uplink_link.header.index).await?;
    let default_routes = default_ipv4_routes_for_link(&handle, uplink_link.header.index).await?;

    for address in &uplink_addresses {
        handle
            .address()
            .del(address.message.clone())
            .execute()
            .await
            .with_context(|| format!("failed to delete IPv4 address from {uplink_if}"))?;
    }

    for route in &default_routes {
        handle
            .route()
            .del(default_route_message(route, uplink_link.header.index))
            .execute()
            .await
            .with_context(|| format!("failed to delete default route from {uplink_if}"))?;
    }

    set_link_controller_up(&handle, uplink_link.header.index, bridge_link.header.index)
        .await
        .with_context(|| format!("failed to attach {uplink_if} to bridge {}", cli.bridge_name))?;
    set_link_controller_up(&handle, tap_link.header.index, bridge_link.header.index)
        .await
        .with_context(|| format!("failed to attach {tap_name} to bridge {}", cli.bridge_name))?;

    for address in &uplink_addresses {
        handle
            .address()
            .add(
                bridge_link.header.index,
                IpAddr::V4(address.address),
                address.prefix_len,
            )
            .replace()
            .execute()
            .await
            .with_context(|| format!("failed to assign IPv4 address to {}", cli.bridge_name))?;
    }

    for route in &default_routes {
        handle
            .route()
            .add(default_route_message(route, bridge_link.header.index))
            .replace()
            .execute()
            .await
            .with_context(|| format!("failed to add default route to {}", cli.bridge_name))?;
    }

    info!(
        tap = %tap_name,
        uplink = %uplink_if,
        bridge = %cli.bridge_name,
        "configured bridge networking"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
async fn ensure_bridge(
    handle: &Handle,
    bridge_name: &str,
    mac: Option<Vec<u8>>,
) -> Result<LinkMessage> {
    if link_by_name_optional(handle, bridge_name).await?.is_none() {
        handle
            .link()
            .add(LinkBridge::new(bridge_name).build())
            .execute()
            .await
            .with_context(|| format!("failed to create bridge {bridge_name}"))?;
    }

    let bridge_link = link_by_name(handle, bridge_name).await?;
    let mut bridge_set = LinkUnspec::new_with_index(bridge_link.header.index).up();
    if let Some(mac) = mac {
        bridge_set = bridge_set.address(mac);
    }
    handle
        .link()
        .set(bridge_set.build())
        .execute()
        .await
        .with_context(|| format!("failed to configure bridge {bridge_name}"))?;

    link_by_name(handle, bridge_name).await
}

#[cfg(target_os = "linux")]
async fn set_link_controller_up(handle: &Handle, index: u32, controller_index: u32) -> Result<()> {
    handle
        .link()
        .set(
            LinkUnspec::new_with_index(index)
                .controller(controller_index)
                .up()
                .build(),
        )
        .execute()
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn link_by_name_optional(handle: &Handle, name: &str) -> Result<Option<LinkMessage>> {
    let mut links = handle.link().get().match_name(name.to_owned()).execute();
    let link = links
        .try_next()
        .await
        .with_context(|| format!("failed to query interface {name}"))?;

    if link.is_some() {
        anyhow::ensure!(
            links.try_next().await?.is_none(),
            "multiple interfaces named {name}"
        );
    }

    Ok(link)
}

#[cfg(target_os = "linux")]
fn link_mac(link: &LinkMessage) -> Option<Vec<u8>> {
    link.attributes
        .iter()
        .find_map(|attribute| match attribute {
            LinkAttribute::Address(address) if !address.is_empty() => Some(address.clone()),
            _ => None,
        })
}

#[cfg(target_os = "linux")]
async fn ipv4_addresses_for_link(handle: &Handle, index: u32) -> Result<Vec<SavedIpv4Address>> {
    let mut addresses = handle
        .address()
        .get()
        .set_link_index_filter(index)
        .execute();
    let mut result = Vec::new();

    while let Some(message) = addresses.try_next().await? {
        if message.header.family != AddressFamily::Inet {
            continue;
        }

        let address = message
            .attributes
            .iter()
            .find_map(|attribute| match attribute {
                AddressAttribute::Local(IpAddr::V4(address))
                | AddressAttribute::Address(IpAddr::V4(address)) => Some(*address),
                _ => None,
            });

        if let Some(address) = address {
            result.push(SavedIpv4Address {
                prefix_len: message.header.prefix_len,
                message,
                address,
            });
        }
    }

    Ok(result)
}

#[cfg(target_os = "linux")]
async fn default_ipv4_routes_for_link(handle: &Handle, index: u32) -> Result<Vec<DefaultRoute>> {
    let mut routes = handle.route().get(RouteMessage::default()).execute();
    let mut result = Vec::new();

    while let Some(route) = routes.try_next().await? {
        if route.header.address_family != AddressFamily::Inet
            || route.header.destination_prefix_length != 0
        {
            continue;
        }

        let mut output_interface = None;
        let mut gateway = None;
        let mut metric = None;

        for attribute in &route.attributes {
            match attribute {
                RouteAttribute::Oif(route_index) => output_interface = Some(*route_index),
                RouteAttribute::Gateway(RouteAddress::Inet(route_gateway)) => {
                    gateway = Some(*route_gateway)
                }
                RouteAttribute::Priority(route_metric) => metric = Some(*route_metric),
                _ => {}
            }
        }

        if output_interface == Some(index) {
            result.push(DefaultRoute { gateway, metric });
        }
    }

    Ok(result)
}

#[cfg(target_os = "linux")]
fn default_route_message(route: &DefaultRoute, output_interface: u32) -> RouteMessage {
    let mut builder = RouteMessageBuilder::<Ipv4Addr>::new().output_interface(output_interface);

    if let Some(gateway) = route.gateway {
        builder = builder.gateway(gateway);
    }

    if let Some(metric) = route.metric {
        builder = builder.priority(metric);
    }

    builder.build()
}
