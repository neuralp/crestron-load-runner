use std::{
    collections::HashSet,
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    sync::mpsc::Sender,
    thread,
    time::{Duration, Instant},
};

use if_addrs::{IfAddr, get_if_addrs};
use socket2::{Domain, Protocol, Socket, Type};

use crate::model::{ConnectionState, Credentials, Device, DeviceSource, classify_model};

const DISCOVERY_PORT: u16 = 41_794;
const PACKET_SIZE: usize = 266;

#[derive(Debug)]
pub enum DiscoveryEvent {
    Found(Box<Device>),
    Finished(Result<(), String>),
}

pub fn spawn(sender: Sender<DiscoveryEvent>) {
    thread::Builder::new()
        .name("crestron-discovery".into())
        .spawn(move || {
            let result = discover(&sender).map_err(|error| error.to_string());
            let _ = sender.send(DiscoveryEvent::Finished(result));
        })
        .expect("failed to start discovery thread");
}

fn discover(sender: &Sender<DiscoveryEvent>) -> io::Result<()> {
    let interfaces = discovery_interfaces()?;
    if interfaces.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no active IPv4 broadcast interfaces found",
        ));
    }

    let mut sockets = Vec::new();
    let mut bind_errors = Vec::new();
    for (local_ip, broadcast_ip) in interfaces {
        match discovery_socket(local_ip) {
            Ok(socket) => sockets.push((socket, broadcast_ip)),
            Err(error) => bind_errors.push(format!("{local_ip}: {error}")),
        }
    }
    if sockets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "could not open a discovery socket: {}",
                bind_errors.join("; ")
            ),
        ));
    }

    let packet = discovery_packet();

    for _ in 0..3 {
        for (socket, broadcast_ip) in &sockets {
            socket.send_to(&packet, SocketAddrV4::new(*broadcast_ip, DISCOVERY_PORT))?;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buffer = [0_u8; 2048];
    let mut seen = HashSet::new();
    while Instant::now() < deadline {
        for (socket, _) in &sockets {
            loop {
                match socket.recv_from(&mut buffer) {
                    Ok((length, remote)) => {
                        match parse_response(&buffer[..length], remote.ip().to_string()) {
                            Some(device) if seen.insert(device.id.clone()) => {
                                let _ = sender.send(DiscoveryEvent::Found(Box::new(device)));
                            }
                            _ => {}
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn discovery_interfaces() -> io::Result<Vec<(Ipv4Addr, Ipv4Addr)>> {
    let mut interfaces = Vec::new();
    let mut seen = HashSet::new();
    for interface in get_if_addrs()? {
        if interface.is_loopback() || interface.is_p2p() {
            continue;
        }
        let IfAddr::V4(address) = interface.addr else {
            continue;
        };
        let Some(broadcast) = address.broadcast else {
            continue;
        };
        if seen.insert(address.ip) {
            interfaces.push((address.ip, broadcast));
        }
    }
    Ok(interfaces)
}

fn discovery_socket(local_ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(local_ip, DISCOVERY_PORT).into())?;
    Ok(socket.into())
}

fn discovery_packet() -> [u8; PACKET_SIZE] {
    let mut packet = [0_u8; PACKET_SIZE];
    packet[..10].copy_from_slice(&[0x14, 0, 0, 0, 1, 4, 0, 3, 0, 0]);
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "crestron-load-runner".into());
    let bytes = hostname.as_bytes();
    let length = bytes.len().min(PACKET_SIZE - 10);
    packet[10..10 + length].copy_from_slice(&bytes[..length]);
    packet
}

pub(crate) fn parse_response(data: &[u8], host: String) -> Option<Device> {
    if data.len() < 5 || !data.starts_with(&[0x15, 0, 0, 0]) {
        return None;
    }

    let chunks: Vec<String> = data[4..]
        .split(|byte| *byte == 0)
        .filter_map(|chunk| {
            let text: String = chunk
                .iter()
                .copied()
                .filter(|byte| (0x20..=0x7e).contains(byte))
                .map(char::from)
                .collect();
            (!text.trim().is_empty()).then(|| text.trim().to_owned())
        })
        .collect();
    let name = chunks.first()?.clone();
    let description = chunks.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
    let model = description
        .split(" [v")
        .next()
        .unwrap_or_default()
        .trim_start_matches("Cntrl Eng ")
        .trim()
        .to_owned();
    let firmware = description
        .split("[v")
        .nth(1)
        .and_then(|rest| rest.split([']', ' ']).next())
        .unwrap_or_default()
        .to_owned();
    let device_id = description
        .split('@')
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_owned();
    let mac = device_id
        .strip_prefix("E-")
        .filter(|value| value.len() >= 12)
        .map(|value| {
            value.as_bytes()[..12]
                .chunks(2)
                .map(|part| String::from_utf8_lossy(part))
                .collect::<Vec<_>>()
                .join(":")
                .to_ascii_uppercase()
        })
        .unwrap_or_default();
    let id = if device_id.is_empty() {
        format!("{}:22", host.to_ascii_lowercase())
    } else {
        device_id.clone()
    };

    Some(Device {
        id,
        host,
        port: 22,
        name,
        model: model.clone(),
        firmware,
        mac,
        kind: classify_model(&model),
        source: DeviceSource::Discovered,
        credentials: Credentials::default(),
        ssh_host_key_fingerprint: None,
        https_certificate: None,
        vc4_api_token: Default::default(),
        program_slots: Default::default(),
        config_slots: Default::default(),
        touchpanel_project: None,
        selected: false,
        connection: ConnectionState::Disconnected,
        progress: None,
        details: None,
        last_message: "Discovered on the local network".into(),
        last_outcome: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKind;

    #[test]
    fn builds_protocol_packet() {
        let packet = discovery_packet();
        assert_eq!(packet.len(), 266);
        assert_eq!(&packet[..10], &[0x14, 0, 0, 0, 1, 4, 0, 3, 0, 0]);
    }

    #[test]
    fn parses_discovery_response() {
        let mut packet = vec![0x15, 0, 0, 0];
        packet.extend_from_slice(b"ROOM-TSW\0\0TSW-1070 [v3.002.1063] @E-00107f112233\0");
        let device = parse_response(&packet, "192.0.2.20".into()).unwrap();
        assert_eq!(device.name, "ROOM-TSW");
        assert_eq!(device.model, "TSW-1070");
        assert_eq!(device.firmware, "3.002.1063");
        assert_eq!(device.mac, "00:10:7F:11:22:33");
        assert_eq!(device.kind, DeviceKind::Touchpanel);
    }

    #[test]
    fn parses_real_response_layout_with_protocol_metadata() {
        let mut packet = vec![0x15, 0, 0, 0, 0x01, 0x84, 0, 0x01, 0, 0];
        packet.extend_from_slice(b"vc4\0\0VC-4 [v4.0004.00153 (Jan 21 2026)] @E-bc2411745874\0");
        let device = parse_response(&packet, "10.2.1.148".into()).unwrap();
        assert_eq!(device.name, "vc4");
        assert_eq!(device.model, "VC-4");
        assert_eq!(device.firmware, "4.0004.00153");
        assert_eq!(device.kind, DeviceKind::Processor);
    }

    #[test]
    #[ignore = "requires Crestron devices on the local network"]
    fn discovers_local_devices() {
        let (sender, receiver) = std::sync::mpsc::channel();
        discover(&sender).unwrap();
        let devices: Vec<Device> = receiver
            .try_iter()
            .filter_map(|event| match event {
                DiscoveryEvent::Found(device) => Some(*device),
                DiscoveryEvent::Finished(_) => None,
            })
            .collect();
        for device in &devices {
            println!(
                "{}: {} at {} ({})",
                device.name,
                device.model,
                device.host,
                device.kind.label()
            );
        }
        assert!(!devices.is_empty());
    }

    #[test]
    fn rejects_non_response_packets() {
        assert!(parse_response(&[0x14, 0, 0, 0], "192.0.2.20".into()).is_none());
    }
}
