#!/usr/bin/env bash
set -euo pipefail

BRIDGE_NAME="${BRIDGE_NAME:-br0}"
UPLINK_IF="${UPLINK_IF:-enp3s0}"
TAP_IF="${TAP_IF:-tap0}"
ACTION="${1:-up}"
SCRIPT_NAME="${BASH_SOURCE[0]##*/}"

require_root() {
    if [[ "${EUID}" -ne 0 ]]; then
        echo "run as root: sudo ${SCRIPT_NAME} ${ACTION}" >&2
        exit 1
    fi
}

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 1
    fi
}

require_interface() {
    if ! ip link show dev "$1" >/dev/null 2>&1; then
        echo "interface not found: $1" >&2
        exit 1
    fi
}

bridge_exists() {
    ip link show dev "${BRIDGE_NAME}" >/dev/null 2>&1
}

interface_master() {
    ip -o link show dev "$1" | sed -n 's/.* master \([^ ]*\).*/\1/p'
}

print_status() {
    echo "bridge=${BRIDGE_NAME} uplink=${UPLINK_IF} tap=${TAP_IF}"
    echo
    ip -br link show dev "${UPLINK_IF}" dev "${TAP_IF}" 2>/dev/null || true
    if bridge_exists; then
        ip -br link show dev "${BRIDGE_NAME}"
        echo
        ip -4 -br addr show dev "${BRIDGE_NAME}" dev "${UPLINK_IF}" dev "${TAP_IF}" 2>/dev/null || true
        echo
        bridge link 2>/dev/null | grep -E "(^| )(${UPLINK_IF}|${TAP_IF})(:| )" || true
        echo
        ip -4 route show default | grep -E "(^default| dev ${BRIDGE_NAME}\$)" || true
    else
        echo "bridge ${BRIDGE_NAME} does not exist; run '${SCRIPT_NAME} up' to create it"
        echo
        ip -4 -br addr show dev "${UPLINK_IF}" dev "${TAP_IF}" 2>/dev/null || true
        echo
        ip -4 route show default | grep -E "(^default| dev ${UPLINK_IF}\$)" || true
    fi
}

uplink_ipv4_addrs() {
    ip -4 -o addr show dev "${UPLINK_IF}" scope global | awk '{print $4}'
}

bridge_ipv4_addrs() {
    ip -4 -o addr show dev "${BRIDGE_NAME}" scope global | awk '{print $4}'
}

default_via_for_dev() {
    ip -4 route show default dev "$1" | awk 'NR == 1 { print $3 }'
}

default_metric_for_dev() {
    ip -4 route show default dev "$1" | awk '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                if ($i == "metric") {
                    print $(i + 1)
                    exit
                }
            }
        }
    '
}

delete_default_routes_for_dev() {
    while ip -4 route show default dev "$1" | grep -q .; do
        ip -4 route del default dev "$1"
    done
}

add_default_route() {
    local gateway="$1"
    local dev="$2"
    local metric="$3"

    if [[ -z "${gateway}" ]]; then
        return
    fi

    if [[ -n "${metric}" ]]; then
        ip route replace default via "${gateway}" dev "${dev}" metric "${metric}"
    else
        ip route replace default via "${gateway}" dev "${dev}"
    fi
}

bridge_up() {
    require_root
    require_interface "${UPLINK_IF}"
    require_interface "${TAP_IF}"

    local current_master
    current_master="$(interface_master "${UPLINK_IF}")"
    if [[ -n "${current_master}" && "${current_master}" != "${BRIDGE_NAME}" ]]; then
        echo "${UPLINK_IF} is already attached to ${current_master}" >&2
        exit 1
    fi

    mapfile -t addrs < <(uplink_ipv4_addrs)
    local gateway metric bridge_mac
    gateway="$(default_via_for_dev "${UPLINK_IF}")"
    metric="$(default_metric_for_dev "${UPLINK_IF}")"
    bridge_mac="$(<"/sys/class/net/${UPLINK_IF}/address")"

    if ! bridge_exists; then
        ip link add name "${BRIDGE_NAME}" type bridge
    fi

    ip link set dev "${BRIDGE_NAME}" address "${bridge_mac}"
    ip link set dev "${BRIDGE_NAME}" up

    for addr in "${addrs[@]}"; do
        ip addr del "${addr}" dev "${UPLINK_IF}"
    done
    delete_default_routes_for_dev "${UPLINK_IF}"

    ip link set dev "${UPLINK_IF}" master "${BRIDGE_NAME}"
    ip link set dev "${TAP_IF}" master "${BRIDGE_NAME}"
    ip link set dev "${UPLINK_IF}" up
    ip link set dev "${TAP_IF}" up

    for addr in "${addrs[@]}"; do
        ip addr add "${addr}" dev "${BRIDGE_NAME}"
    done
    add_default_route "${gateway}" "${BRIDGE_NAME}" "${metric}"

    echo "bridge ${BRIDGE_NAME} is up"
    print_status
}

bridge_down() {
    require_root

    if ! bridge_exists; then
        echo "bridge ${BRIDGE_NAME} does not exist"
        return
    fi

    mapfile -t addrs < <(bridge_ipv4_addrs)
    local gateway metric
    gateway="$(default_via_for_dev "${BRIDGE_NAME}")"
    metric="$(default_metric_for_dev "${BRIDGE_NAME}")"

    for addr in "${addrs[@]}"; do
        ip addr del "${addr}" dev "${BRIDGE_NAME}"
    done
    delete_default_routes_for_dev "${BRIDGE_NAME}"

    if [[ "$(interface_master "${TAP_IF}")" == "${BRIDGE_NAME}" ]]; then
        ip link set dev "${TAP_IF}" nomaster
    fi
    if [[ "$(interface_master "${UPLINK_IF}")" == "${BRIDGE_NAME}" ]]; then
        ip link set dev "${UPLINK_IF}" nomaster
    fi

    ip link set dev "${UPLINK_IF}" up
    ip link set dev "${TAP_IF}" up

    for addr in "${addrs[@]}"; do
        ip addr add "${addr}" dev "${UPLINK_IF}"
    done
    add_default_route "${gateway}" "${UPLINK_IF}" "${metric}"

    ip link delete "${BRIDGE_NAME}" type bridge

    echo "bridge ${BRIDGE_NAME} is down"
    ip -br link show dev "${UPLINK_IF}" dev "${TAP_IF}" 2>/dev/null || true
    ip -4 -br addr show dev "${UPLINK_IF}" dev "${TAP_IF}" 2>/dev/null || true
}

main() {
    require_command ip

    case "${ACTION}" in
        up)
            require_command bridge
            bridge_up
            ;;
        down)
            require_command bridge
            bridge_down
            ;;
        status)
            print_status
            ;;
        *)
            echo "usage: ${SCRIPT_NAME} [up|down|status]" >&2
            exit 1
            ;;
    esac
}

main "$@"
