//! Control actions sent to a renderer (SOAP over HTTP).

use std::time::Duration;

use super::search::{AVT, Device, RCS};
use crate::{soap, xml};

const TIMEOUT: Duration = Duration::from_secs(5);
/// Largest response read (DIDL metadata comes back in GetPositionInfo).
const MAX_RESPONSE: u64 = 256 * 1024;

/// A failed action: unreachable renderer, or a SOAP fault.
#[derive(Debug)]
pub enum Failure {
    Unreachable(String),
    Refused(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreachable(e) => write!(f, "renderer unreachable ({e})"),
            Failure::Refused(e) => write!(f, "renderer refused: {e}"),
        }
    }
}

pub type Out = Result<Vec<(String, String)>, Failure>;

fn envelope(service: &str, action: &str, args: &[(&str, &str)]) -> String {
    let body: String = args
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", xml::escape(v)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">{body}</u:{action}></s:Body></s:Envelope>"
    )
}

fn read_body(r: ureq::Response) -> String {
    let mut s = String::new();
    let _ = std::io::Read::read_to_string(
        &mut std::io::Read::take(r.into_reader(), MAX_RESPONSE),
        &mut s,
    );
    s
}

/// What a fault says: `UPnPError` code and description when given.
fn fault_text(body: &str) -> String {
    let pick = |tag: &str| {
        let open = format!("<{tag}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find('<')? + start;
        Some(body[start..end].trim().to_string())
    };
    match (pick("errorCode"), pick("errorDescription")) {
        (Some(c), Some(d)) => format!("{d} ({c})"),
        (Some(c), None) => format!("error {c}"),
        _ => "SOAP fault".into(),
    }
}

fn call(url: &str, service: &str, action: &str, args: &[(&str, &str)]) -> Out {
    let resp = ureq::post(url)
        .timeout(TIMEOUT)
        .set("Content-Type", "text/xml; charset=\"utf-8\"")
        .set("SOAPACTION", &format!("\"{service}#{action}\""))
        .send_string(&envelope(service, action, args));
    match resp {
        Ok(r) => {
            let body = read_body(r);
            soap::parse(body.as_bytes())
                .map(|(_, out)| out)
                .map_err(|e| Failure::Refused(format!("{action}: {e}")))
        }
        Err(ureq::Error::Status(_, r)) => Err(Failure::Refused(format!(
            "{action}: {}",
            fault_text(&read_body(r))
        ))),
        Err(e) => Err(Failure::Unreachable(e.to_string())),
    }
}

/// Value of an output argument.
pub fn arg<'a>(out: &'a [(String, String)], name: &str) -> &'a str {
    out.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

impl Device {
    pub fn avt(&self, action: &str, args: &[(&str, &str)]) -> Out {
        let mut all = vec![("InstanceID", "0")];
        all.extend_from_slice(args);
        call(&self.avt, AVT, action, &all)
    }

    /// RenderingControl, when the renderer has it.
    pub fn rcs(&self, action: &str, args: &[(&str, &str)]) -> Option<Out> {
        let url = self.rcs.as_deref()?;
        let mut all = vec![("InstanceID", "0"), ("Channel", "Master")];
        all.extend_from_slice(args);
        Some(call(url, RCS, action, &all))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_escapes_arguments() {
        let e = envelope(
            AVT,
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/a?b=1&c=2")],
        );
        let (action, args) = soap::parse(e.as_bytes()).unwrap();
        assert_eq!(action, "SetAVTransportURI");
        assert_eq!(
            args,
            vec![("CurrentURI".to_string(), "http://h/a?b=1&c=2".to_string())]
        );
    }

    #[test]
    fn faults() {
        let body = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>714</errorCode><errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
        assert_eq!(fault_text(body), "Illegal MIME-type (714)");
        assert_eq!(fault_text("<x/>"), "SOAP fault");
    }
}
