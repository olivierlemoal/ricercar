//! Finding renderers: an SSDP M-SEARCH on every interface, then the device
//! description behind each answer.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::xml;

pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RCS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
/// Descriptions larger than this are not renderers we want to talk to.
const MAX_DESCRIPTION: u64 = 512 * 1024;
const DESCRIPTION_TIMEOUT: Duration = Duration::from_secs(3);

/// A renderer we can drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// UDN without its `uuid:` prefix.
    pub uuid: String,
    pub name: String,
    pub model: String,
    /// Absolute control URLs.
    pub avt: String,
    pub rcs: Option<String>,
}

impl Device {
    /// Address the renderer is reached at (its control URL's host).
    pub fn addr(&self) -> Option<SocketAddr> {
        let rest = self.avt.strip_prefix("http://")?;
        let host = rest.split('/').next()?;
        host.parse().ok().or_else(|| {
            host.parse::<IpAddr>()
                .ok()
                .map(|ip| SocketAddr::new(ip, 80))
        })
    }
}

/// Interfaces that reach a LAN: point-to-point tunnels (`/32`, VPNs such as
/// WARP or Tailscale) carry no renderers, and their addresses are not
/// reachable from one.
fn lans() -> Vec<crate::ssdp::Iface> {
    crate::ssdp::ifaces()
        .into_iter()
        .filter(|i| i.mask != Ipv4Addr::BROADCAST)
        .collect()
}

/// Renderers answering a search for `st` within `wait`. One answering at
/// several addresses is kept at one on a LAN of ours.
pub fn search(st: &str, wait: Duration) -> Vec<Device> {
    let lans = lans();
    let locations = msearch(st, wait, &lans);
    let threads: Vec<_> = locations
        .into_iter()
        .map(|loc| std::thread::spawn(move || describe(&loc)))
        .collect();
    let mut found: Vec<Device> = threads
        .into_iter()
        .filter_map(|t| t.join().ok().flatten())
        .collect();
    found.sort_by_key(|d| rank(d, &lans));
    let mut seen = HashSet::new();
    found.retain(|d| seen.insert(d.uuid.clone()));
    found
}

/// 0: on one of our LANs, 1: this host, 2: anywhere else.
fn rank(d: &Device, lans: &[crate::ssdp::Iface]) -> u8 {
    match d.addr().map(|a| a.ip()) {
        Some(IpAddr::V4(ip))
            if lans.iter().any(|i| {
                let m = u32::from(i.mask);
                u32::from(i.ip) & m == u32::from(ip) & m
            }) =>
        {
            0
        }
        Some(ip) if ip.is_loopback() => 1,
        _ => 2,
    }
}

fn msearch_packet(st: &str, mx: u64) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {GROUP}:{PORT}\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {st}\r\nUSER-AGENT: Linux/6 UPnP/1.1 ricercar/0.5\r\n\r\n"
    )
}

/// LOCATION of every answer to an M-SEARCH sent on each LAN interface (and
/// on loopback, for a renderer on this host).
fn msearch(st: &str, wait: Duration, lans: &[crate::ssdp::Iface]) -> Vec<String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let Ok(sock) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else {
        return Vec::new();
    };
    if sock
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())
        .is_err()
    {
        return Vec::new();
    }
    let _ = sock.set_multicast_ttl_v4(2);
    let packet = msearch_packet(st, wait.as_secs().clamp(1, 5));
    let dst = socket2::SockAddr::from(SocketAddr::from((GROUP, PORT)));
    let mut ifs: Vec<Ipv4Addr> = lans.iter().map(|i| i.ip).collect();
    ifs.push(Ipv4Addr::LOCALHOST);
    // Twice: UDP gets lost.
    for _ in 0..2 {
        for ip in &ifs {
            if sock.set_multicast_if_v4(ip).is_ok() {
                let _ = sock.send_to(packet.as_bytes(), &dst);
            }
        }
    }
    let sock: std::net::UdpSocket = sock.into();
    let deadline = Instant::now() + wait;
    let mut out: Vec<String> = Vec::new();
    let mut buf = [0u8; 4096];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() || sock.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let reply = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = location(&reply)
                    && !out.contains(&loc)
                {
                    out.push(loc);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(_) => {}
        }
    }
    out
}

/// LOCATION of a search reply (`HTTP/1.1 200 OK`), http URLs only.
fn location(reply: &str) -> Option<String> {
    let mut lines = reply.lines();
    if !lines.next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| v.starts_with("http://"))
}

/// The renderer described at `location`, when it has AVTransport.
pub(super) fn describe(location: &str) -> Option<Device> {
    let resp = ureq::get(location)
        .timeout(DESCRIPTION_TIMEOUT)
        .call()
        .map_err(|e| tracing::debug!("upnp output: {location}: {e}"))
        .ok()?;
    let mut body = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(resp.into_reader(), MAX_DESCRIPTION),
        &mut body,
    )
    .ok()?;
    parse_description(location, &body)
}

#[derive(Default)]
struct Desc {
    kind: String,
    name: String,
    manufacturer: String,
    model: String,
    udn: String,
    /// (serviceType, controlURL)
    services: Vec<(String, String)>,
}

/// The first MediaRenderer of a description (root or embedded) with an
/// AVTransport service; control URLs made absolute.
pub fn parse_description(location: &str, body: &str) -> Option<Device> {
    let mut reader = Reader::from_str(body);
    let mut path: Vec<String> = Vec::new();
    let mut devices: Vec<Desc> = Vec::new();
    let mut open: Vec<Desc> = Vec::new();
    let mut service: Option<(String, String)> = None;
    let mut base: Option<String> = None;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                match name.as_str() {
                    "device" => open.push(Desc::default()),
                    "service" => service = Some(Default::default()),
                    _ => {}
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Text(t)) => text.push_str(&t.into_inner()),
            Ok(Event::CData(t)) => text.push_str(&t.into_inner()),
            Ok(Event::GeneralRef(r)) => text.push_str(&xml::resolve_ref(&r)),
            Ok(Event::End(_)) => {
                let Some(name) = path.pop() else { break };
                let value = std::mem::take(&mut text).trim().to_string();
                let parent = path.last().map(String::as_str);
                match (name.as_str(), parent) {
                    ("URLBase", _) => base = Some(value),
                    ("device", _) => devices.extend(open.pop()),
                    ("service", _) => {
                        if let (Some(s), Some(d)) = (service.take(), open.last_mut()) {
                            d.services.push(s);
                        }
                    }
                    ("serviceType", Some("service")) => {
                        if let Some(s) = service.as_mut() {
                            s.0 = value;
                        }
                    }
                    ("controlURL", Some("service")) => {
                        if let Some(s) = service.as_mut() {
                            s.1 = value;
                        }
                    }
                    (field, Some("device")) => {
                        if let Some(d) = open.last_mut() {
                            match field {
                                "deviceType" => d.kind = value,
                                "friendlyName" => d.name = value,
                                "manufacturer" => d.manufacturer = value,
                                "modelName" => d.model = value,
                                "UDN" => d.udn = value,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let base = base
        .filter(|b| b.starts_with("http://"))
        .unwrap_or_else(|| location.to_string());
    devices
        .into_iter()
        .filter(|d| {
            d.kind
                .starts_with("urn:schemas-upnp-org:device:MediaRenderer:")
        })
        .find_map(|d| {
            let control = |urn: &str| {
                let prefix = urn.trim_end_matches(":1");
                d.services
                    .iter()
                    .find(|(t, u)| t.starts_with(prefix) && !u.is_empty())
                    .map(|(_, u)| join(&base, u))
            };
            let avt = control(AVT)?;
            let uuid = d.udn.strip_prefix("uuid:").unwrap_or(&d.udn).to_string();
            if uuid.is_empty() {
                return None;
            }
            let model = match (d.manufacturer.is_empty(), d.model.is_empty()) {
                (false, false) if d.model.starts_with(&d.manufacturer) => d.model.clone(),
                (false, false) => format!("{} {}", d.manufacturer, d.model),
                (true, _) => d.model.clone(),
                (false, true) => d.manufacturer.clone(),
            };
            Some(Device {
                name: if d.name.is_empty() {
                    uuid.clone()
                } else {
                    d.name
                },
                uuid,
                model,
                avt,
                rcs: control(RCS),
            })
        })
}

/// `rel` against the description's base URL.
fn join(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let rest = base.strip_prefix("http://").unwrap_or(base);
    let host = rest.split('/').next().unwrap_or(rest);
    if rel.starts_with('/') {
        return format!("http://{host}{rel}");
    }
    let path = rest[host.len()..]
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir);
    format!("http://{host}{path}/{rel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEOS_LIKE: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Salon &amp; Co</friendlyName>
    <manufacturer>Marantz</manufacturer>
    <modelName>MODEL 40n</modelName>
    <UDN>uuid:5f9ec1b3-ed59-1900-4530-0005cdabcdef</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/upnp/control/ConnectionManager1</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/upnp/control/AVTransport1</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>rc</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn description_of_a_renderer() {
        let d = parse_description(
            "http://192.168.1.20:60006/upnp/desc/aios_device/aios_device.xml",
            HEOS_LIKE,
        )
        .unwrap();
        assert_eq!(d.uuid, "5f9ec1b3-ed59-1900-4530-0005cdabcdef");
        assert_eq!(d.name, "Salon & Co");
        assert_eq!(d.model, "Marantz MODEL 40n");
        assert_eq!(d.avt, "http://192.168.1.20:60006/upnp/control/AVTransport1");
        assert_eq!(
            d.rcs.as_deref(),
            Some("http://192.168.1.20:60006/upnp/desc/aios_device/rc")
        );
        assert_eq!(d.addr(), Some("192.168.1.20:60006".parse().unwrap()));
    }

    #[test]
    fn embedded_renderer_and_url_base() {
        let body = r#"<root><URLBase>http://10.0.0.5:8080/</URLBase><device>
            <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType><UDN>uuid:srv</UDN>
            <deviceList><device>
              <deviceType>urn:schemas-upnp-org:device:MediaRenderer:2</deviceType>
              <friendlyName>Kitchen</friendlyName><UDN>uuid:r1</UDN>
              <serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:2</serviceType>
              <controlURL>avt/ctl</controlURL></service></serviceList>
            </device></deviceList></device></root>"#;
        let d = parse_description("http://10.0.0.5:8080/desc.xml", body).unwrap();
        assert_eq!((d.uuid.as_str(), d.name.as_str()), ("r1", "Kitchen"));
        assert_eq!(d.avt, "http://10.0.0.5:8080/avt/ctl");
        assert_eq!(d.rcs, None);
    }

    #[test]
    fn not_a_renderer() {
        let body = r#"<root><device><deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
            <UDN>uuid:x</UDN></device></root>"#;
        assert_eq!(parse_description("http://h/d.xml", body), None);
        let no_avt = HEOS_LIKE.replace("AVTransport", "Nothing");
        assert_eq!(parse_description("http://h/d.xml", &no_avt), None);
    }

    #[test]
    fn search_replies() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.30:49152/\r\nST: x\r\n\r\n";
        assert_eq!(
            location(reply).as_deref(),
            Some("http://192.168.1.30:49152/")
        );
        assert_eq!(
            location("NOTIFY * HTTP/1.1\r\nLOCATION: http://h/\r\n\r\n"),
            None
        );
    }
}
