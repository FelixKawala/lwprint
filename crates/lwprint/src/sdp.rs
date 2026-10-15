//! Minimal SDP client: finds the RFCOMM channel of a device's Serial Port service,
//! like `sdptool browse` does. Needs no bluetoothd compatibility mode.

use anyhow::{anyhow, bail, Context, Result};
use bluer::l2cap::{SeqPacket, SocketAddr};
use bluer::{Address, AddressType};
use std::time::Duration;
use tokio::time::timeout;

const SDP_PSM: u16 = 1;
const UUID_RFCOMM: u32 = 0x0003;
const UUID_L2CAP: u32 = 0x0100;
const UUID_SERIAL_PORT: u32 = 0x1101;

#[derive(Debug, Clone, PartialEq)]
pub enum De {
    Nil,
    UInt(u64),
    Int(i64),
    Uuid(u32),
    Uuid128([u8; 16]),
    Text(String),
    Bool(bool),
    Seq(Vec<De>),
    Other,
}

/// Parse one SDP data element starting at `*i`.
pub fn parse(buf: &[u8], i: &mut usize) -> Result<De> {
    let hdr = *buf.get(*i).ok_or_else(|| anyhow!("truncated SDP data"))?;
    *i += 1;
    let (typ, size) = (hdr >> 3, hdr & 7);
    if typ == 0 {
        return Ok(De::Nil);
    }
    let len = match size {
        0..=4 => [1usize, 2, 4, 8, 16][size as usize],
        _ => {
            let lb = [1usize, 2, 4][(size - 5) as usize];
            let raw = buf.get(*i..*i + lb).ok_or_else(|| anyhow!("truncated SDP length"))?;
            *i += lb;
            raw.iter().fold(0usize, |a, &b| (a << 8) | b as usize)
        }
    };
    let raw = buf.get(*i..*i + len).ok_or_else(|| anyhow!("truncated SDP element"))?;
    let end = *i + len;
    let be = |r: &[u8]| r.iter().fold(0u64, |a, &b| (a << 8) | b as u64);
    let value = match typ {
        1 => De::UInt(be(raw)),
        2 => De::Int(be(raw) as i64),
        3 if len <= 4 => De::Uuid(be(raw) as u32),
        3 if len == 16 => De::Uuid128(raw.try_into().unwrap()),
        4 | 8 => De::Text(String::from_utf8_lossy(raw).trim_end_matches('\0').to_string()),
        5 => De::Bool(raw.first().copied().unwrap_or(0) != 0),
        6 | 7 => {
            let mut items = Vec::new();
            let mut j = *i;
            while j < end {
                items.push(parse(buf, &mut j)?);
            }
            De::Seq(items)
        }
        _ => De::Other,
    };
    *i = end;
    Ok(value)
}

/// From parsed service records, return the RFCOMM channel of the Serial Port service.
pub fn serial_port_channel(records: &De) -> Option<u8> {
    let De::Seq(records) = records else { return None };
    for rec in records {
        let De::Seq(attrs) = rec else { continue };
        let mut is_spp = false;
        let mut channel = None;
        for pair in attrs.chunks(2) {
            match pair {
                [De::UInt(0x0001), De::Seq(classes)] => {
                    is_spp = classes.contains(&De::Uuid(UUID_SERIAL_PORT));
                }
                [De::UInt(0x0004), De::Seq(protocols)] => {
                    for p in protocols {
                        if let De::Seq(p) = p {
                            if let [De::Uuid(UUID_RFCOMM), De::UInt(ch), ..] = p.as_slice() {
                                channel = Some(*ch as u8);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if is_spp && channel.is_some() {
            return channel;
        }
    }
    None
}

/// Result of a lookup. Keep it alive until the serial connection is open: dropping the
/// SDP link makes the kernel start tearing down the Bluetooth connection, and a serial
/// connection opened right then can land on the dying link (it connects, then the first
/// write fails with "Transport endpoint is not connected").
pub struct Lookup {
    pub channel: u8,
    _link: SeqPacket,
}

/// Ask the device for its service records and return the Serial Port channel.
pub async fn find_serial_channel(addr: Address) -> Result<Lookup> {
    let sa = SocketAddr::new(addr, AddressType::BrEdr, SDP_PSM);
    let sock = timeout(Duration::from_secs(8), SeqPacket::connect(sa))
        .await
        .context("timed out connecting for service lookup")?
        .context("could not connect for service lookup")?;

    // ServiceSearchAttributeRequest: every service using L2CAP, all attributes.
    let pattern = [0x35, 0x03, 0x19, (UUID_L2CAP >> 8) as u8, UUID_L2CAP as u8];
    let attrs = [0x35, 0x05, 0x0A, 0x00, 0x00, 0xFF, 0xFF];
    let mut continuation = vec![0u8];
    let mut data = Vec::new();
    for tid in 1u16..=32 {
        let mut params = pattern.to_vec();
        params.extend_from_slice(&0xFFFFu16.to_be_bytes());
        params.extend_from_slice(&attrs);
        params.extend_from_slice(&continuation);
        let mut pdu = vec![0x06];
        pdu.extend_from_slice(&tid.to_be_bytes());
        pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
        pdu.extend_from_slice(&params);
        sock.send(&pdu).await?;

        let mut buf = vec![0u8; 4096];
        let n = timeout(Duration::from_secs(10), sock.recv(&mut buf))
            .await
            .context("timed out waiting for service list")??;
        let resp = &buf[..n];
        if resp.len() < 8 || resp[0] != 0x07 {
            bail!("unexpected service lookup reply");
        }
        let count = u16::from_be_bytes([resp[5], resp[6]]) as usize;
        let chunk = resp.get(7..7 + count).ok_or_else(|| anyhow!("short service lookup reply"))?;
        data.extend_from_slice(chunk);
        continuation = resp[7 + count..].to_vec();
        if continuation.first().copied().unwrap_or(0) == 0 {
            let mut i = 0;
            let records = parse(&data, &mut i)?;
            let channel =
                serial_port_channel(&records).ok_or_else(|| anyhow!("the device has no Serial Port service"))?;
            return Ok(Lookup { channel, _link: sock });
        }
    }
    bail!("service list too long")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_channel_in_lw_c410_like_records() {
        // Two records: Serial Port on channel 6, Apple iAP on channel 3.
        let spp = [
            0x09, 0x00, 0x01, 0x35, 0x03, 0x19, 0x11, 0x01, //
            0x09, 0x00, 0x04, 0x35, 0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00,
            0x03, 0x08, 0x06,
        ];
        let mut iap = vec![0x09, 0x00, 0x01, 0x35, 0x11, 0x1C];
        iap.extend_from_slice(&[0, 0, 0, 0, 0xDE, 0xCA, 0xFA, 0xDE, 0xDE, 0xCA, 0xDE, 0xAF, 0xDE, 0xCA, 0xCA, 0xFF]);
        iap.extend_from_slice(&[0x09, 0x00, 0x04, 0x35, 0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00, 0x03, 0x08, 0x03]);
        let mut all = vec![0x35, (iap.len() + 2 + spp.len() + 2) as u8, 0x35, iap.len() as u8];
        all.extend_from_slice(&iap);
        all.extend_from_slice(&[0x35, spp.len() as u8]);
        all.extend_from_slice(&spp);
        let mut i = 0;
        let records = parse(&all, &mut i).unwrap();
        assert_eq!(serial_port_channel(&records), Some(6));
    }
}
