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
        route::{RouteAddress, RouteAttribute, RouteFlags, RouteMessage},
    },
};
#[cfg(target_os = "linux")]
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_os = "linux")]
use tracing::info;

#[cfg(target_os = "linux")]
use super::{
    link_by_name, link_by_name_optional, new_netlink_handle, resolve_uplink_if, set_link_up,
};

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
    message: RouteMessage,
    gateway: Option<Ipv4Addr>,
    metric: Option<u32>,
}

#[cfg(target_os = "linux")]
pub(super) async fn configure_bridge_mode(cli: &crate::Cli, tap_name: &str) -> Result<()> {
    let uplink_if = resolve_uplink_if(cli).await?;
    let handle = new_netlink_handle()?;

    let uplink_link = link_by_name(&handle, &uplink_if).await?;
    let tap_link = link_by_name(&handle, tap_name).await?;
    let bridge_link = ensure_bridge(&handle, &cli.bridge_name, link_mac(&uplink_link)).await?;
    let uplink_addresses = ipv4_addresses_for_link(&handle, uplink_link.header.index).await?;
    let default_routes = default_ipv4_routes_for_link(&handle, uplink_link.header.index).await?;

    for route in &default_routes {
        handle
            .route()
            .del(route.message.clone())
            .execute()
            .await
            .with_context(|| format!("failed to delete default route from {uplink_if}"))?;
    }

    for address in &uplink_addresses {
        handle
            .address()
            .del(address.message.clone())
            .execute()
            .await
            .with_context(|| format!("failed to delete IPv4 address from {uplink_if}"))?;
    }

    attach_link_to_bridge(
        &handle,
        &uplink_link,
        bridge_link.header.index,
        &uplink_if,
        &cli.bridge_name,
    )
    .await?;
    attach_link_to_bridge(
        &handle,
        &tap_link,
        bridge_link.header.index,
        tap_name,
        &cli.bridge_name,
    )
    .await?;

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
    if let Some(mac) = mac {
        if link_mac(&bridge_link).as_deref() != Some(mac.as_slice()) {
            set_link_address(handle, bridge_link.header.index, mac)
                .await
                .with_context(|| format!("failed to set bridge MAC address for {bridge_name}"))?;
        }
    }
    set_link_up(handle, bridge_link.header.index)
        .await
        .with_context(|| format!("failed to bring bridge {bridge_name} up"))?;

    link_by_name(handle, bridge_name).await
}

#[cfg(target_os = "linux")]
async fn set_link_address(handle: &Handle, index: u32, mac: Vec<u8>) -> Result<()> {
    handle
        .link()
        .set(LinkUnspec::new_with_index(index).address(mac).build())
        .execute()
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn attach_link_to_bridge(
    handle: &Handle,
    link: &LinkMessage,
    bridge_index: u32,
    link_name: &str,
    bridge_name: &str,
) -> Result<()> {
    match current_controller_index(link) {
        Some(current_bridge_index) if current_bridge_index == bridge_index => {
            set_link_up(handle, link.header.index)
                .await
                .with_context(|| format!("failed to bring {link_name} up"))?;
            return Ok(());
        }
        Some(current_bridge_index) => {
            anyhow::bail!(
                "{link_name} is already attached to controller index {current_bridge_index}, expected bridge {bridge_name}"
            );
        }
        None => {}
    }

    handle
        .link()
        .set(
            LinkUnspec::new_with_index(link.header.index)
                .controller(bridge_index)
                .build(),
        )
        .execute()
        .await
        .with_context(|| format!("failed to attach {link_name} to bridge {bridge_name}"))?;

    set_link_up(handle, link.header.index)
        .await
        .with_context(|| {
            format!("failed to bring {link_name} up after attaching it to bridge {bridge_name}")
        })?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn current_controller_index(link: &LinkMessage) -> Option<u32> {
    link.attributes
        .iter()
        .find_map(|attribute| match attribute {
            LinkAttribute::Controller(index) if *index != 0 => Some(*index),
            _ => None,
        })
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
                    gateway = Some(*route_gateway);
                }
                RouteAttribute::Priority(route_metric) => metric = Some(*route_metric),
                _ => {}
            }
        }

        if output_interface == Some(index) {
            result.push(DefaultRoute {
                message: route,
                gateway,
                metric,
            });
        }
    }

    Ok(result)
}

#[cfg(target_os = "linux")]
fn default_route_message(route: &DefaultRoute, output_interface: u32) -> RouteMessage {
    let mut builder = RouteMessageBuilder::<Ipv4Addr>::new()
        .output_interface(output_interface)
        .table_id(route_table_id(&route.message))
        .protocol(route.message.header.protocol)
        .scope(route.message.header.scope)
        .kind(route.message.header.kind);

    if route.message.header.flags.contains(RouteFlags::Onlink) {
        builder = builder.onlink();
    }

    if let Some(gateway) = route.gateway {
        builder = builder.gateway(gateway);
    }

    if let Some(metric) = route.metric {
        builder = builder.priority(metric);
    }

    builder.build()
}

#[cfg(target_os = "linux")]
fn route_table_id(route: &RouteMessage) -> u32 {
    route
        .attributes
        .iter()
        .find_map(|attribute| match attribute {
            RouteAttribute::Table(table) => Some(*table),
            _ => None,
        })
        .unwrap_or_else(|| u32::from(route.header.table))
}
