//! Program IP table assignment, sharing the details panel's table parser.

use crate::model::{Device, DeviceKind};

mod window;
pub(crate) use window::{Action, Panel};

pub(crate) fn is_processor(device: &Device) -> bool {
    device.kind == DeviceKind::Processor && !crate::vc4::is_vc4(&device.model)
}
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::Ipv4Addr,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowKey {
    pub ipid: String,
    pub model: String,
}

#[derive(Debug)]
struct MasterEntry {
    ipid: String,
    address: String,
    id_key: String,
    address_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddressMode {
    Ip,
    Hostname,
}

/// Normalize for comparison only. Never convert the table token's radix.
pub(crate) fn ipid_key(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 2 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("Invalid CIP_ID token: {value:?}"));
    }
    let upper = value.to_ascii_uppercase();
    let key = upper.trim_start_matches('0');
    Ok(if key.is_empty() {
        "0".into()
    } else {
        key.into()
    })
}

fn ipv4(value: &str) -> Option<Ipv4Addr> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0; 4];
    for (part, octet) in parts.into_iter().zip(&mut octets) {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    Some(Ipv4Addr::from(octets))
}

fn hostname(value: &str) -> bool {
    let name = value.strip_suffix('.').unwrap_or(value);
    !name.is_empty()
        && name.len() <= 253
        && name.bytes().any(|b| b.is_ascii_alphabetic())
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub(crate) fn address_key(value: &str) -> Result<String, String> {
    if let Some(ip) = ipv4(value) {
        return Ok(ip.to_string());
    }
    if hostname(value) {
        return Ok(value.trim_end_matches('.').to_ascii_lowercase());
    }
    Err(format!("Not a safe IPv4 address or hostname: {value:?}"))
}

pub(crate) fn master_address(device: &Device, mode: AddressMode) -> Result<String, String> {
    let value = match mode {
        AddressMode::Ip => {
            if ipv4(&device.host).is_some() {
                Some(device.host.as_str())
            } else {
                device.discovered.as_ref().map(|d| d.ip.as_str())
            }
        }
        AddressMode::Hostname => device
            .discovered
            .as_ref()
            .map(|d| d.hostname.as_str())
            .filter(|v| hostname(v))
            .or_else(|| hostname(&device.host).then_some(device.host.as_str())),
    }
    .ok_or_else(|| "Processor address unavailable; rediscover the processor".to_owned())?;
    if mode == AddressMode::Ip && ipv4(value).is_none() {
        return Err("Processor IP unavailable; rediscover the processor".into());
    }
    address_key(value)?;
    Ok(value.to_owned())
}

fn column(table: &crate::ip_table::Table<'_>, heading: &str) -> Result<usize, String> {
    let positions: Vec<_> = table
        .headings
        .iter()
        .enumerate()
        .filter(|(_, h)| h.eq_ignore_ascii_case(heading))
        .map(|(i, _)| i)
        .collect();
    match positions.as_slice() {
        [index] => Ok(*index),
        _ => Err(format!("Missing or ambiguous IP table heading: {heading}")),
    }
}

fn one_table(contents: &str) -> Result<crate::ip_table::Table<'_>, String> {
    console_ok(contents)?;
    let mut tables = crate::ip_table::parse(contents)
        .ok_or_else(|| "Unrecognized or malformed IP table; no writes allowed".to_owned())?;
    if tables.len() != 1 {
        return Err("Expected one IP table; no writes allowed".into());
    }
    Ok(tables.remove(0))
}

pub(crate) fn program_rows(contents: &str, program: u8) -> Result<Vec<RowKey>, String> {
    if !(1..=10).contains(&program) {
        return Err("Program must be 1 through 10".into());
    }
    let table = one_table(contents)?;
    if !table.title.is_empty()
        && !table
            .title
            .eq_ignore_ascii_case(&format!("IP Table for program {program}"))
        && !table
            .title
            .eq_ignore_ascii_case(&format!("Program {program}"))
    {
        return Err(format!("Unexpected program table: {}", table.title));
    }
    let ipid = column(&table, "CIP_ID")?;
    let model = column(&table, "Model Name")?;
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for row in table.rows {
        if !seen.insert(ipid_key(row[ipid])?) {
            return Err("Duplicate/ambiguous CIP_ID in processor table".into());
        }
        rows.push(RowKey {
            ipid: row[ipid].into(),
            model: row[model].into(),
        });
    }
    Ok(rows)
}

fn master_entries(contents: &str) -> Result<Vec<MasterEntry>, String> {
    let table = one_table(contents)?;
    let ipid = column(&table, "CIP_ID")?;
    let address = column(&table, "IP Address/SiteName")?;
    table
        .rows
        .into_iter()
        .map(|row| {
            Ok(MasterEntry {
                ipid: row[ipid].into(),
                address: row[address].into(),
                id_key: ipid_key(row[ipid])?,
                address_key: address_key(row[address])?,
            })
        })
        .collect()
}

fn other_entries<'a>(
    entries: &'a [MasterEntry],
    target: &str,
) -> BTreeMap<(&'a str, &'a str), usize> {
    let mut result = BTreeMap::new();
    for entry in entries.iter().filter(|e| e.id_key != target) {
        *result
            .entry((entry.id_key.as_str(), entry.address_key.as_str()))
            .or_default() += 1;
    }
    result
}

fn console_ok(output: &str) -> Result<(), String> {
    if output.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        [
            "error",
            "invalid command",
            "unknown command",
            "command failed",
            "failed:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    }) {
        Err(format!("Console rejected the command: {}", output.trim()))
    } else {
        Ok(())
    }
}

/// No automatic retry: a timed-out mutation may already have reached the device.
pub(crate) async fn replace<F, Fut>(ipid: &str, master: &str, mut run: F) -> Result<String, String>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let target = ipid_key(ipid)?;
    let desired = address_key(master)?;
    let before = master_entries(&run("ipt -t".into()).await?)?;
    let unrelated = other_entries(&before, &target);
    let matching: Vec<_> = before.iter().filter(|e| e.id_key == target).collect();
    if matching.len() == 1 && matching[0].address_key == desired {
        return Ok("Already correct".into());
    }
    let mut removed = BTreeSet::new();
    for entry in matching {
        if !removed.insert(entry.address_key.as_str()) {
            continue;
        }
        console_ok(&run(format!("remmaster {} {}", entry.ipid, entry.address)).await?)?;
        let after = master_entries(&run("ipt -t".into()).await?)?;
        if other_entries(&after, &target) != unrelated {
            return Err("Unrelated IPIDs changed after removal; stopped without adding".into());
        }
        if after
            .iter()
            .any(|e| e.id_key == target && removed.contains(e.address_key.as_str()))
        {
            return Err("Removal could not be verified; stopped without adding".into());
        }
        if after.iter().any(|e| {
            e.id_key == target
                && !before
                    .iter()
                    .any(|old| old.id_key == target && old.address_key == e.address_key)
        }) {
            return Err("IP table changed concurrently; stopped without adding".into());
        }
    }
    console_ok(&run(format!("addmaster {ipid} {master}")).await?)?;
    let after = master_entries(&run("ipt -t".into()).await?)?;
    let matching: Vec<_> = after.iter().filter(|e| e.id_key == target).collect();
    if matching.len() != 1
        || matching[0].address_key != desired
        || other_entries(&after, &target) != unrelated
    {
        return Err("Final IP table does not match the assignment; inspect the device log".into());
    }
    Ok("Verified".into())
}

#[cfg(test)]
#[path = "ipid_assignment_tests.rs"]
mod tests;
