use anyhow::{Context, Result};
use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

#[cfg(target_os = "linux")]
use ipnetwork::{IpNetwork, Ipv4Network};
#[cfg(target_os = "linux")]
use rustables::{
    Batch, Chain, ChainPolicy, ChainType, Hook, HookClass, MsgType, ProtocolFamily, Rule, Table,
    expr::{Bitwise, Cmp, CmpOp, ConnTrackState, Conntrack, ConntrackKey},
    list_tables,
};
#[cfg(target_os = "linux")]
use std::{fs, net::IpAddr};
#[cfg(target_os = "linux")]
use tracing::info;

#[cfg(target_os = "linux")]
use super::{link_by_name, new_netlink_handle, set_link_up};

#[cfg(target_os = "linux")]
const NAT_FORWARD_CHAIN_NAME: &str = "forward";
#[cfg(target_os = "linux")]
const NAT_POSTROUTING_CHAIN_NAME: &str = "postrouting";
#[cfg(target_os = "linux")]
const NAT_FORWARD_CHAIN_PRIORITY: i32 = 0;
#[cfg(target_os = "linux")]
const NAT_POSTROUTING_CHAIN_PRIORITY: i32 = 100;
#[cfg(target_os = "linux")]
const NAT_TABLE_NAME_PREFIX: &str = "websockproxy_nat_";

#[cfg(target_os = "linux")]
pub(super) async fn configure_nat_mode(cli: &crate::Cli, tap_name: &str) -> Result<()> {
    let uplink_if = cli
        .uplink_if
        .as_deref()
        .context("nat mode requires --uplink-if")?;
    let nat_subnet = cli.nat_subnet()?;
    let tap_gateway = nat_gateway(nat_subnet)?;
    let handle = new_netlink_handle()?;
    let uplink_link = link_by_name(&handle, uplink_if).await?;
    let tap_link = link_by_name(&handle, tap_name).await?;

    handle
        .address()
        .add(
            tap_link.header.index,
            IpAddr::V4(tap_gateway),
            nat_subnet.prefix_len(),
        )
        .replace()
        .execute()
        .await
        .with_context(|| format!("failed to configure NAT gateway on {tap_name}"))?;
    set_link_up(&handle, tap_link.header.index)
        .await
        .with_context(|| format!("failed to bring {tap_name} up"))?;

    fs::write("/proc/sys/net/ipv4/ip_forward", "1\n")
        .context("failed to enable net.ipv4.ip_forward")?;
    configure_nat_ruleset(tap_name, uplink_if, nat_subnet)?;

    let table_name = nat_table_name(tap_name);

    info!(
        tap = %tap_name,
        uplink = %uplink_if,
        uplink_index = uplink_link.header.index,
        nat_network = %nat_subnet,
        tap_gateway = %tap_gateway,
        nft_table = %table_name,
        "configured NAT networking"
    );

    Ok(())
}

pub(super) fn nat_gateway(subnet: Ipv4Net) -> Result<Ipv4Addr> {
    Ok(Ipv4Addr::from(
        u32::from(subnet.network()).saturating_add(1),
    ))
}

#[cfg(target_os = "linux")]
fn configure_nat_ruleset(tap_name: &str, uplink_if: &str, nat_subnet: Ipv4Net) -> Result<()> {
    let table_name = nat_table_name(tap_name);
    remove_nat_ruleset(&table_name)?;

    let nat_network = nat_ip_network(nat_subnet)?;
    let table = Table::new(ProtocolFamily::Ipv4).with_name(&table_name);
    let forward_chain = Chain::new(&table)
        .with_name(NAT_FORWARD_CHAIN_NAME)
        .with_hook(Hook::new(HookClass::Forward, NAT_FORWARD_CHAIN_PRIORITY))
        .with_type(ChainType::Filter)
        .with_policy(ChainPolicy::Accept);
    let postrouting_chain = Chain::new(&table)
        .with_name(NAT_POSTROUTING_CHAIN_NAME)
        .with_hook(Hook::new(
            HookClass::PostRouting,
            NAT_POSTROUTING_CHAIN_PRIORITY,
        ))
        .with_type(ChainType::Nat)
        .with_policy(ChainPolicy::Accept);

    let tap_to_uplink_rule = Rule::new(&forward_chain)?
        .iiface(tap_name)?
        .oiface(uplink_if)?
        .accept();
    let uplink_to_tap_rule = related_or_established_rule(&forward_chain, uplink_if, tap_name)?;
    let masquerade_rule = Rule::new(&postrouting_chain)?
        .snetwork(nat_network)?
        .oiface(uplink_if)?
        .masquerade();

    let mut batch = Batch::new();
    batch.add(&table, MsgType::Add);
    batch.add(&forward_chain, MsgType::Add);
    batch.add(&postrouting_chain, MsgType::Add);
    batch.add(&tap_to_uplink_rule, MsgType::Add);
    batch.add(&uplink_to_tap_rule, MsgType::Add);
    batch.add(&masquerade_rule, MsgType::Add);
    batch
        .send()
        .context("failed to apply nftables NAT ruleset")?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_nat_ruleset(table_name: &str) -> Result<()> {
    let existing_tables = list_tables().context("failed to query nftables tables")?;
    let table_exists = existing_tables
        .into_iter()
        .any(|table| matches!(table.get_name(), Some(name) if name == table_name));

    if !table_exists {
        return Ok(());
    }

    let table = Table::new(ProtocolFamily::Ipv4).with_name(table_name);
    let mut batch = Batch::new();
    batch.add(&table, MsgType::Del);
    batch
        .send()
        .with_context(|| format!("failed to delete existing nftables table `{table_name}`"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn related_or_established_rule(chain: &Chain, uplink_if: &str, tap_name: &str) -> Result<Rule> {
    let allowed_states = (ConnTrackState::ESTABLISHED | ConnTrackState::RELATED).bits();
    let rule = Rule::new(chain)?
        .iiface(uplink_if)?
        .oiface(tap_name)?
        .with_expr(Conntrack::new(ConntrackKey::State))
        .with_expr(Bitwise::new(
            allowed_states.to_le_bytes(),
            0_u32.to_be_bytes(),
        )?)
        .with_expr(Cmp::new(CmpOp::Neq, 0_u32.to_be_bytes()))
        .accept();
    Ok(rule)
}

#[cfg(target_os = "linux")]
fn nat_ip_network(subnet: Ipv4Net) -> Result<IpNetwork> {
    let network = Ipv4Network::new(subnet.network(), subnet.prefix_len())
        .with_context(|| format!("failed to convert `{subnet}` into an nftables IPv4 network"))?;
    Ok(IpNetwork::V4(network))
}

#[cfg(target_os = "linux")]
fn nat_table_name(tap_name: &str) -> String {
    format!(
        "{NAT_TABLE_NAME_PREFIX}{}",
        sanitize_nftables_name_component(tap_name)
    )
}

#[cfg(target_os = "linux")]
fn sanitize_nftables_name_component(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());

    for ch in name.chars() {
        match ch {
            'a'..='z' | '0'..='9' => sanitized.push(ch),
            'A'..='Z' => sanitized.push(ch.to_ascii_lowercase()),
            _ => sanitized.push('_'),
        }
    }

    if sanitized.is_empty() {
        sanitized.push_str("tap");
    }

    sanitized
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::{nat_table_name, sanitize_nftables_name_component};

    #[cfg(target_os = "linux")]
    #[test]
    fn nftables_name_component_is_sanitized() {
        assert_eq!(sanitize_nftables_name_component("Tap-01.eth"), "tap_01_eth");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nat_table_name_uses_tap_name() {
        assert_eq!(nat_table_name("tap0"), "websockproxy_nat_tap0");
    }
}
