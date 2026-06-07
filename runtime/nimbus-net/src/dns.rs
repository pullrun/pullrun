use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub struct DnsRecord {
    pub name: String,
    pub ip: String,
}

pub struct DnsProxy {
    listen_addr: SocketAddr,
    upstream: SocketAddr,
    records: Arc<RwLock<HashMap<String, String>>>,
}

impl DnsProxy {
    pub fn new(listen_addr: SocketAddr, upstream: SocketAddr) -> Self {
        Self {
            listen_addr,
            upstream,
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_default_upstream(listen_addr: SocketAddr) -> Self {
        Self::new(listen_addr, "8.8.8.8:53".parse().unwrap())
    }

    pub async fn add_record(&self, name: &str, ip: &str) {
        let mut records = self.records.write().await;
        records.insert(name.to_string(), ip.to_string());
        info!(name, ip, "DNS record added");
    }

    pub async fn remove_record(&self, name: &str) {
        let mut records = self.records.write().await;
        records.remove(name);
        info!(name, "DNS record removed");
    }

    pub async fn resolve_local(&self, name: &str) -> Option<String> {
        let records = self.records.read().await;
        records.get(name).cloned()
    }

    pub async fn run(self: Arc<Self>) -> Result<(), std::io::Error> {
        let socket = Arc::new(UdpSocket::bind(self.listen_addr).await?);
        info!(addr = %self.listen_addr, "DNS proxy listening");

        let mut buf = vec![0u8; 512];
        loop {
            let (len, src) = match socket.recv_from(&mut buf).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("DNS recv error: {e}");
                    continue;
                }
            };

            let query = buf[..len].to_vec();
            let me = self.clone();
            let socket_clone = socket.clone();
            tokio::spawn(async move {
                if let Some(response) = me.handle_query(&query).await {
                    if let Err(e) = socket_clone.send_to(&response, src).await {
                        tracing::warn!("DNS send error: {e}");
                    }
                }
            });
        }
    }

    async fn handle_query(&self, query: &[u8]) -> Option<Vec<u8>> {
        if query.len() < 12 {
            return None;
        }

        let qname = parse_qname(&query[12..])?;
        debug!(%qname, "DNS query");

        let qtype = (query[query.len() - 4] as u16) << 8 | query[query.len() - 3] as u16;

        if let Some(ip) = self.resolve_local(&qname).await {
            return Some(build_a_response(query, &qname, &ip));
        }

        if qtype != 1 {
            return Some(build_refused_response(query));
        }

        match forward_query(query, self.upstream).await {
            Ok(response) => Some(response),
            Err(e) => {
                tracing::warn!(error = %e, "DNS forward failed");
                Some(build_servfail_response(query))
            }
        }
    }
}

fn parse_qname(buf: &[u8]) -> Option<String> {
    let mut name = String::new();
    let mut i = 0;
    while i < buf.len() {
        let len = buf[i] as usize;
        if len == 0 {
            break;
        }
        if i + 1 + len > buf.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(&buf[i + 1..i + 1 + len]).ok()?);
        i += 1 + len;
    }
    Some(name)
}

fn build_a_response(query: &[u8], name: &str, ip: &str) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() < 12 {
        return resp;
    }
    resp[2] = 0x81;
    resp[3] = 0x80;

    let ip_addr: std::net::Ipv4Addr = ip.parse().unwrap_or_else(|_| std::net::Ipv4Addr::new(0, 0, 0, 0));
    let octets = ip_addr.octets();

    let mut answer = Vec::new();
    for label in name.split('.') {
        answer.push(label.len() as u8);
        answer.extend_from_slice(label.as_bytes());
    }
    answer.push(0);
    answer.extend_from_slice(&[0, 1]);
    answer.extend_from_slice(&[0, 1]);
    answer.extend_from_slice(&[0, 0, 0, 60]);
    answer.extend_from_slice(&[0, 4]);
    answer.extend_from_slice(&octets);

    resp.extend_from_slice(&answer);
    resp
}

fn build_servfail_response(query: &[u8]) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() >= 4 {
        resp[2] = 0x81;
        resp[3] = 0x82;
    }
    resp
}

fn build_refused_response(query: &[u8]) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() >= 4 {
        resp[2] = 0x81;
        resp[3] = 0x05;
    }
    resp
}

async fn forward_query(query: &[u8], upstream: SocketAddr) -> std::io::Result<Vec<u8>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.send_to(query, upstream).await?;
    let mut buf = vec![0u8; 4096];
    let (len, _) = socket.recv_from(&mut buf).await?;
    Ok(buf[..len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dns_record_management() {
        let proxy = DnsProxy::with_default_upstream("127.0.0.1:15353".parse().unwrap());
        proxy.add_record("test.nimbus.local", "10.42.0.5").await;
        let result = proxy.resolve_local("test.nimbus.local").await;
        assert_eq!(result, Some("10.42.0.5".to_string()));
        proxy.remove_record("test.nimbus.local").await;
        let result = proxy.resolve_local("test.nimbus.local").await;
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_qname() {
        let query = b"\x03foo\x03bar\x00";
        assert_eq!(parse_qname(query), Some("foo.bar".to_string()));
    }
}