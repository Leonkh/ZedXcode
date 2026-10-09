//! Simulator destinations: one inventory of the iOS simulators
//! (`xcrun simctl list devices --json`), the resolution of a chosen
//! destination against it, and the one automatic rule. The pipeline, the
//! destination picker, `doctor` and `setup` all go through here, so they
//! agree on which simulators exist and which one runs.
//!
//! - The inventory holds the available iPhones and iPads on iOS runtimes. The
//!   family comes from `deviceTypeIdentifier`, so a simulator renamed to
//!   "Test Phone A" is still an iPhone; the name is the fallback for a simctl
//!   that omits the identifier.
//! - A destination resolves by UDID first; when that simulator is gone, by
//!   name and OS, with a warning.
//! - The OS is lenient: "26", "26.3" and "26.3.1" all match the installed
//!   iOS 26.3 runtime (runtime identifiers carry no patch level). When the
//!   OS matches several runtimes, the newest wins, with a warning.
//! - Automatic: the booted iPhone, else the newest iPhone on the newest iOS
//!   runtime. "Newest" is read from the model's generation number
//!   (`iPhone-17-Pro` is generation 17); models without one (SE, Air, X)
//!   rank below every numbered one. Within a generation the device type's
//!   natural order decides (`iPhone-17-Pro-Max` before `iPhone-17-Pro`).

use std::cmp::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::engine::simctl;

/// How long one `simctl list` answer is reused within a process.
const INVENTORY_TTL: Duration = Duration::from_secs(30);

/// The last inventory read in this process, and when.
static CACHE: Mutex<Option<(Instant, Inventory)>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    IPhone,
    IPad,
}

/// One available simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub family: Family,
    /// `com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro`; empty when
    /// simctl did not report it.
    pub device_type: String,
    /// The runtime's iOS version, e.g. `[26, 3]`.
    pub os: Vec<u32>,
    pub booted: bool,
}

impl Device {
    /// `"26.3"`.
    pub fn os_version(&self) -> String {
        version_string(&self.os)
    }

    /// `iPhone 16 Pro (iOS 26.3)`.
    pub fn label(&self) -> String {
        format!("{} (iOS {})", self.name, self.os_version())
    }
}

/// The available iPhone and iPad simulators, sorted booted first, then
/// newest iOS, then iPhones before iPads, then name and UDID (the picker's
/// order), and the installed iOS runtimes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    devices: Vec<Device>,
    /// Installed iOS runtime versions, newest first.
    runtimes: Vec<Vec<u32>>,
}

impl Inventory {
    /// Parse `simctl list devices --json`.
    pub fn parse(json: &Value) -> Result<Inventory> {
        let runtimes = json
            .get("devices")
            .and_then(Value::as_object)
            .context("malformed simctl JSON: missing `devices` object")?;
        let mut inventory = Inventory::default();
        for (runtime, list) in runtimes {
            let Some(os) = runtime_version(runtime) else {
                continue; // watchOS, tvOS, visionOS
            };
            let list = list.as_array().map(Vec::as_slice).unwrap_or_default();
            // A runtime that is no longer installed still lists the devices
            // made with it, all unavailable.
            let installed = list.is_empty()
                || list
                    .iter()
                    .any(|d| d.get("isAvailable").and_then(Value::as_bool) == Some(true));
            if installed && !inventory.runtimes.contains(&os) {
                inventory.runtimes.push(os.clone());
            }
            for d in list {
                if d.get("isAvailable").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let (Some(udid), Some(name), Some(state)) = (
                    d.get("udid").and_then(Value::as_str),
                    d.get("name").and_then(Value::as_str),
                    d.get("state").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let device_type = d
                    .get("deviceTypeIdentifier")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(family) = family(device_type, name) else {
                    continue;
                };
                inventory.devices.push(Device {
                    udid: udid.to_owned(),
                    name: name.to_owned(),
                    family,
                    device_type: device_type.to_owned(),
                    os: os.clone(),
                    booted: state == "Booted",
                });
            }
        }
        inventory.runtimes.sort_by(|a, b| b.cmp(a));
        inventory.devices.sort_by(|a, b| {
            b.booted
                .cmp(&a.booted)
                .then_with(|| b.os.cmp(&a.os))
                .then_with(|| a.family.cmp(&b.family))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.udid.cmp(&b.udid))
        });
        Ok(inventory)
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// `(available, booted)` simulators.
    pub fn counts(&self) -> (usize, usize) {
        let booted = self.devices.iter().filter(|d| d.booted).count();
        (self.devices.len(), booted)
    }

    fn booted_label(&self) -> Option<String> {
        // The booted iPhone the automatic rule would take, else any booted one.
        self.devices
            .iter()
            .filter(|d| d.booted)
            .min_by(|a, b| automatic_order(a, b))
            .map(Device::label)
    }
}

/// The simulator inventory, from simctl (bounded like every simctl list) or
/// from this process's last answer while it is younger than 30 s.
pub async fn inventory() -> Result<Inventory> {
    if let Some(inventory) = cached(Instant::now()) {
        return Ok(inventory);
    }
    let json = simctl::list_devices_json().await?;
    let inventory = Inventory::parse(&json)?;
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), inventory.clone()));
    }
    Ok(inventory)
}

fn cached(now: Instant) -> Option<Inventory> {
    let cache = CACHE.lock().ok()?;
    let (at, inventory) = cache.as_ref()?;
    (now.saturating_duration_since(*at) < INVENTORY_TTL).then(|| inventory.clone())
}

/// A chosen destination, before it meets the inventory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Tried first.
    pub udid: Option<String>,
    /// A device name; tried when there is no UDID or its simulator is gone.
    pub name: Option<String>,
    /// iOS version, matched leniently; alone it narrows the automatic rule.
    pub os: Option<String>,
    /// `name` holds a 0.1 `device` value (scenario key, `--device`), which
    /// may also be a UDID or `booted` (any booted simulator).
    pub legacy: bool,
}

impl Query {
    /// The 0.1 `device` + `os` pair; `None` when both are absent.
    pub fn legacy(device: Option<&str>, os: Option<&str>) -> Option<Query> {
        if device.is_none() && os.is_none() {
            return None;
        }
        Some(Query {
            udid: None,
            name: device.map(str::to_owned),
            os: os.map(str::to_owned),
            legacy: true,
        })
    }

    /// `iPhone 14 · iOS 17.5`, for messages.
    pub fn label(&self) -> String {
        let device = self.name.as_deref().or(self.udid.as_deref());
        match (device, self.os.as_deref()) {
            (Some(d), Some(os)) => format!("{d} · iOS {os}"),
            (Some(d), None) => d.to_owned(),
            (None, Some(os)) => format!("iOS {os}"),
            (None, None) => "automatic".to_owned(),
        }
    }
}

/// A resolved destination and what the user should read about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub device: Device,
    pub warnings: Vec<String>,
}

/// Resolve `query` (`None` = automatic) against the inventory. `from` names
/// where the query came from, for the messages (".zed/.zedx/selection.json").
pub fn resolve(query: Option<&Query>, inventory: &Inventory, from: &str) -> Result<Resolved> {
    let Some(query) = query else {
        return automatic(inventory, None, None);
    };
    let by_udid = |udid: &str| {
        inventory
            .devices
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(udid))
    };
    if let Some(device) = query.udid.as_deref().and_then(by_udid) {
        return Ok(resolved(device, Vec::new()));
    }
    // A 0.1 value may be a UDID.
    if query.legacy {
        if let Some(device) = query.name.as_deref().and_then(by_udid) {
            return Ok(resolved(device, Vec::new()));
        }
    }
    let os = query.os.as_deref().map(parse_version);
    let Some(name) = query.name.as_deref() else {
        if query.udid.is_some() {
            return Err(not_available(query, inventory, from));
        }
        // An OS alone narrows the automatic rule.
        return automatic(inventory, os.as_ref(), query.os.as_deref())
            .map_err(|_| not_available(query, inventory, from));
    };
    let booted_query = query.legacy && name.eq_ignore_ascii_case("booted");
    let candidates: Vec<&Device> = inventory
        .devices
        .iter()
        .filter(|d| {
            if booted_query {
                d.booted
            } else {
                d.name.eq_ignore_ascii_case(name)
            }
        })
        .filter(|d| os.as_ref().is_none_or(|os| os_matches(&d.os, os)))
        .collect();
    let mut warnings = Vec::new();
    let candidates = narrow_os(candidates, query.os.as_deref(), &mut warnings);
    let Some(device) = candidates.into_iter().min_by(|a, b| {
        b.booted
            .cmp(&a.booted)
            .then_with(|| b.os.cmp(&a.os))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.udid.cmp(&b.udid))
    }) else {
        return Err(not_available(query, inventory, from));
    };
    if let Some(gone) = query.udid.as_deref() {
        warnings.push(format!(
            "The simulator {gone} chosen in {from} is gone; using {} ({}), the same model and \
             iOS. Choose it again with Xcode: Choose Destination to remember it.",
            device.label(),
            device.udid
        ));
    }
    Ok(resolved(device, warnings))
}

fn resolved(device: &Device, warnings: Vec<String>) -> Resolved {
    Resolved {
        device: device.clone(),
        warnings,
    }
}

/// The automatic rule, optionally inside the runtimes matching `os`: the
/// booted iPhone, else the newest iPhone on the newest iOS runtime.
fn automatic(
    inventory: &Inventory,
    os: Option<&Vec<u32>>,
    os_text: Option<&str>,
) -> Result<Resolved> {
    let iphones: Vec<&Device> = inventory
        .devices
        .iter()
        .filter(|d| d.family == Family::IPhone)
        .filter(|d| os.is_none_or(|os| os_matches(&d.os, os)))
        .collect();
    let mut warnings = Vec::new();
    let iphones = narrow_os(iphones, os_text, &mut warnings);
    match iphones.into_iter().min_by(|a, b| automatic_order(a, b)) {
        Some(device) => Ok(resolved(device, warnings)),
        None => bail!(
            "No iPhone simulator is available{}: install an iOS simulator runtime in Xcode ▸ \
             Settings ▸ Components, or add an iPhone simulator in Xcode ▸ Window ▸ Devices and \
             Simulators.",
            os_text.map(|o| format!(" on iOS {o}")).unwrap_or_default()
        ),
    }
}

/// Booted first, then the newest runtime, then the newest model (the higher
/// generation number, see [`generation`]; then the device type in natural
/// order, so "iPhone-17-Pro" comes before "iPhone-17"), then name and UDID,
/// so the choice is stable.
fn automatic_order(a: &Device, b: &Device) -> Ordering {
    b.booted
        .cmp(&a.booted)
        .then_with(|| b.os.cmp(&a.os))
        .then_with(|| generation(b).cmp(&generation(a)))
        .then_with(|| natural_cmp(model(b), model(a)))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.udid.cmp(&b.udid))
}

/// The model: the last part of the device type
/// (`com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro` -> `iPhone-17-Pro`),
/// else the name for a simctl that does not report it.
fn model(d: &Device) -> &str {
    d.device_type
        .rsplit('.')
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or(&d.name)
}

/// The generation number a model starts with after its family
/// (`iPhone-17-Pro` and "iPhone 16e" -> 17 and 16); 0 for a model without
/// one (`iPhone-SE-3rd-generation`, `iPhone-Air`, `iPhone-Xs`), which ranks
/// it below every numbered model.
fn generation(d: &Device) -> u32 {
    let model = model(d);
    let rest = model
        .strip_prefix("iPhone")
        .or_else(|| model.strip_prefix("iPad"))
        .unwrap_or(model)
        .trim_start_matches(['-', ' ']);
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .map_or(rest, |end| &rest[..end]);
    digits.parse().unwrap_or(0)
}

/// When an OS given as text matched devices on several runtimes, keep the
/// newest runtime's and say so.
fn narrow_os<'a>(
    candidates: Vec<&'a Device>,
    os_text: Option<&str>,
    warnings: &mut Vec<String>,
) -> Vec<&'a Device> {
    let Some(os_text) = os_text else {
        return candidates;
    };
    let mut versions: Vec<&Vec<u32>> = candidates.iter().map(|d| &d.os).collect();
    versions.sort();
    versions.dedup();
    let Some(newest) = versions.last().copied().cloned() else {
        return candidates;
    };
    if versions.len() > 1 {
        let names: Vec<String> = versions
            .iter()
            .map(|v| format!("iOS {}", version_string(v)))
            .collect();
        let (last, rest) = names.split_last().expect("at least two versions");
        warnings.push(format!(
            "OS \"{os_text}\" matches {} and {last}; using iOS {}. Pick an exact destination \
             with Xcode: Choose Destination.",
            rest.join(", "),
            version_string(&newest)
        ));
    }
    candidates.into_iter().filter(|d| d.os == newest).collect()
}

/// The error for a destination that matches no available simulator.
fn not_available(query: &Query, inventory: &Inventory, from: &str) -> anyhow::Error {
    let booted = inventory
        .booted_label()
        .map(|b| format!("; {b} is booted"))
        .unwrap_or_default();
    let install = query
        .os
        .as_deref()
        .filter(|os| {
            let wanted = parse_version(os);
            !inventory.runtimes.iter().any(|r| os_matches(r, &wanted))
        })
        .map(|os| format!(", or install iOS {os} in Xcode ▸ Settings ▸ Components"))
        .unwrap_or_default();
    anyhow::anyhow!(
        "Destination \"{}\" (from {from}) is not available{booted}. Use Xcode: Choose \
         Destination{install}.",
        query.label()
    )
}

/// The family a simulator belongs to: from its device type, else its name;
/// `None` for anything but an iPhone or iPad.
fn family(device_type: &str, name: &str) -> Option<Family> {
    let kind = device_type
        .rsplit('.')
        .next()
        .filter(|k| !k.is_empty())
        .unwrap_or(name);
    if kind.starts_with("iPhone") {
        Some(Family::IPhone)
    } else if kind.starts_with("iPad") {
        Some(Family::IPad)
    } else {
        None
    }
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-3` -> `[26, 3]`; `None` for a
/// runtime other than iOS.
fn runtime_version(runtime: &str) -> Option<Vec<u32>> {
    let tail = runtime.rsplit('.').next()?.strip_prefix("iOS-")?;
    let version: Vec<u32> = tail.split('-').map_while(|p| p.parse().ok()).collect();
    (!version.is_empty()).then_some(version)
}

/// `"26.3"` (also `"iOS 26.3"`) -> `[26, 3]`; text that is no version gives
/// an empty list, which matches no runtime.
fn parse_version(text: &str) -> Vec<u32> {
    let text = text.trim();
    let text = text
        .strip_prefix("iOS")
        .map(str::trim_start)
        .unwrap_or(text);
    let parts: Option<Vec<u32>> = text.split('.').map(|p| p.parse().ok()).collect();
    parts.unwrap_or_default()
}

fn version_string(version: &[u32]) -> String {
    version
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// Whether a runtime version matches a requested one: either is a prefix of
/// the other ("26" matches 26.3; "26.3.1" matches the 26.3 runtime, whose
/// identifier has no patch level). An empty request matches nothing.
fn os_matches(runtime: &[u32], wanted: &[u32]) -> bool {
    !wanted.is_empty() && (runtime.starts_with(wanted) || wanted.starts_with(runtime))
}

/// Compare with digit runs as numbers: "iPhone-9" < "iPhone-17".
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let split = |s: &str| {
                    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
                    (s[..end].trim_start_matches('0').to_owned(), end)
                };
                let ((na, ea), (nb, eb)) = (split(a), split(b));
                let order = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[ea..];
                b = &b[eb..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a = &a[x.len_utf8()..];
                b = &b[y.len_utf8()..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const IPHONE_17: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-17";
    const IPHONE_17_PRO: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro";
    const IPHONE_9: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-9";
    const IPHONE_SE: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-SE-3rd-generation";
    const IPAD_AIR: &str = "com.apple.CoreSimulator.SimDeviceType.iPad-Air-11-inch-M3";
    const WATCH: &str = "com.apple.CoreSimulator.SimDeviceType.Apple-Watch-Series-10-46mm";

    fn device(udid: &str, name: &str, device_type: &str, state: &str) -> Value {
        json!({ "udid": udid, "name": name, "state": state, "isAvailable": true,
                "deviceTypeIdentifier": device_type })
    }

    /// Synthetic UDIDs; three iOS 26 runtimes, one older one, a watchOS one.
    fn fixture() -> Value {
        json!({ "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-18-6": [
                device("11111111-AAAA-BBBB-CCCC-000000000001", "iPhone SE (3rd generation)",
                       IPHONE_SE, "Shutdown"),
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-0": [
                device("11111111-AAAA-BBBB-CCCC-000000000002", "iPhone 17", IPHONE_17, "Shutdown"),
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
                device("11111111-AAAA-BBBB-CCCC-000000000003", "iPhone 17", IPHONE_17, "Shutdown"),
                // A renamed simulator: listed and used as an iPhone.
                device("11111111-AAAA-BBBB-CCCC-000000000004", "Test Phone A", IPHONE_17_PRO,
                       "Shutdown"),
                device("11111111-AAAA-BBBB-CCCC-000000000005", "iPad Air 11-inch (M3)", IPAD_AIR,
                       "Shutdown"),
                { "udid": "11111111-AAAA-BBBB-CCCC-000000000006", "name": "iPhone 17 Pro",
                  "state": "Shutdown", "isAvailable": false,
                  "deviceTypeIdentifier": IPHONE_17_PRO },
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
                device("11111111-AAAA-BBBB-CCCC-000000000007", "iPhone 17", IPHONE_17, "Shutdown"),
                device("11111111-AAAA-BBBB-CCCC-000000000008", "iPhone 9", IPHONE_9, "Shutdown"),
            ],
            "com.apple.CoreSimulator.SimRuntime.watchOS-26-0": [
                device("11111111-AAAA-BBBB-CCCC-000000000009", "Apple Watch Series 10 (46mm)",
                       WATCH, "Booted"),
            ],
        }})
    }

    fn inventory_of(v: &Value) -> Inventory {
        Inventory::parse(v).unwrap()
    }

    fn query(name: Option<&str>, os: Option<&str>) -> Query {
        Query {
            name: name.map(str::to_owned),
            os: os.map(str::to_owned),
            ..Default::default()
        }
    }

    fn udid_of(r: &Resolved) -> &str {
        &r.device.udid[r.device.udid.len() - 2..]
    }

    const FROM: &str = ".zed/.zedx/selection.json";

    #[test]
    fn listing_uses_the_device_type_and_skips_other_platforms() {
        let inv = inventory_of(&fixture());
        let listed: Vec<(&str, String, Family)> = inv
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.os_version(), d.family))
            .collect();
        // Newest iOS first, iPhones before iPads, then name.
        assert_eq!(
            listed,
            [
                ("iPhone 17", "26.5".to_owned(), Family::IPhone),
                ("iPhone 9", "26.5".to_owned(), Family::IPhone),
                ("Test Phone A", "26.3".to_owned(), Family::IPhone),
                ("iPhone 17", "26.3".to_owned(), Family::IPhone),
                ("iPad Air 11-inch (M3)", "26.3".to_owned(), Family::IPad),
                ("iPhone 17", "26.0".to_owned(), Family::IPhone),
                (
                    "iPhone SE (3rd generation)",
                    "18.6".to_owned(),
                    Family::IPhone
                ),
            ]
        );
        // The unavailable simulator and the watch are not listed.
        assert!(!inv.devices().iter().any(|d| d.udid.ends_with("06")));
        assert!(!inv.devices().iter().any(|d| d.udid.ends_with("09")));
        assert_eq!(inv.counts(), (7, 0));
    }

    #[test]
    fn the_name_is_the_fallback_without_a_device_type() {
        let v = json!({ "devices": { "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
            { "udid": "22222222-AAAA-BBBB-CCCC-000000000001", "name": "iPhone 16e",
              "state": "Booted", "isAvailable": true },
            { "udid": "22222222-AAAA-BBBB-CCCC-000000000002", "name": "Test Phone B",
              "state": "Shutdown", "isAvailable": true },
        ]}});
        let inv = inventory_of(&v);
        assert_eq!(inv.devices().len(), 1);
        assert_eq!(inv.devices()[0].name, "iPhone 16e");
        assert_eq!(inv.counts(), (1, 1));
        assert!(Inventory::parse(&json!({})).is_err());
    }

    #[test]
    fn lenient_os_matches_major_minor_and_patch() {
        let inv = inventory_of(&fixture());
        for os in ["26.3", "26.3.1", "iOS 26.3"] {
            let r = resolve(Some(&query(Some("iPhone 17"), Some(os))), &inv, FROM).unwrap();
            assert_eq!(udid_of(&r), "03", "{os}");
            assert!(r.warnings.is_empty(), "{os}: {:?}", r.warnings);
        }
        let r = resolve(Some(&query(Some("iphone 17"), Some("26.0"))), &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "02");
    }

    #[test]
    fn an_os_matching_several_runtimes_takes_the_newest_and_warns() {
        let inv = inventory_of(&fixture());
        let r = resolve(Some(&query(Some("iPhone 17"), Some("26"))), &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "07");
        assert_eq!(
            r.warnings,
            [
                "OS \"26\" matches iOS 26.0, iOS 26.3 and iOS 26.5; using iOS 26.5. Pick an exact \
              destination with Xcode: Choose Destination."
            ]
        );
        // Only the runtimes that hold the named simulator count.
        let r = resolve(Some(&query(Some("Test Phone A"), Some("26"))), &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "04");
        assert!(r.warnings.is_empty());
        // An OS alone narrows the automatic rule, with the same warning.
        let r = resolve(Some(&query(None, Some("26"))), &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "07");
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn udid_first_then_name_and_os_with_a_warning() {
        let inv = inventory_of(&fixture());
        let stored = |udid: &str| Query {
            udid: Some(udid.to_owned()),
            name: Some("iPhone 17".into()),
            os: Some("26.3".into()),
            legacy: false,
        };
        // The UDID wins over a name and OS that would pick another one.
        let mut q = stored("11111111-AAAA-BBBB-CCCC-000000000007");
        q.os = Some("26.0".into());
        let r = resolve(Some(&q), &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "07");
        assert!(r.warnings.is_empty());
        // A simulator that was deleted and made again: same name and OS.
        let r = resolve(
            Some(&stored("FFFFFFFF-AAAA-BBBB-CCCC-000000000000")),
            &inv,
            FROM,
        )
        .unwrap();
        assert_eq!(udid_of(&r), "03");
        assert_eq!(
            r.warnings,
            [
                "The simulator FFFFFFFF-AAAA-BBBB-CCCC-000000000000 chosen in \
              .zed/.zedx/selection.json is gone; using iPhone 17 (iOS 26.3) \
              (11111111-AAAA-BBBB-CCCC-000000000003), the same model and iOS. Choose it again \
              with Xcode: Choose Destination to remember it."
            ]
        );
    }

    #[test]
    fn legacy_values_are_a_udid_a_name_or_booted() {
        let mut v = fixture();
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-18-6"][0]["state"] = json!("Booted");
        let inv = inventory_of(&v);
        let legacy = |device: &str| Query::legacy(Some(device), None).unwrap();
        let r = resolve(
            Some(&legacy("11111111-aaaa-bbbb-cccc-000000000004")),
            &inv,
            FROM,
        )
        .unwrap();
        assert_eq!(r.device.name, "Test Phone A");
        assert!(r.warnings.is_empty());
        // A name on several runtimes: the booted one, else the newest OS.
        assert_eq!(
            udid_of(&resolve(Some(&legacy("iPhone 17")), &inv, FROM).unwrap()),
            "07"
        );
        assert_eq!(
            udid_of(&resolve(Some(&legacy("booted")), &inv, FROM).unwrap()),
            "01"
        );
        assert_eq!(Query::legacy(None, None), None);
    }

    #[test]
    fn automatic_takes_the_booted_iphone_else_the_newest_iphone() {
        let inv = inventory_of(&fixture());
        // Newest runtime (26.5), newest model there: iPhone 17 over iPhone 9.
        let r = resolve(None, &inv, FROM).unwrap();
        assert_eq!(udid_of(&r), "07");
        // A booted iPhone wins over newer runtimes; a booted iPad does not.
        let mut v = fixture();
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-26-3"][2]["state"] = json!("Booted");
        assert_eq!(
            udid_of(&resolve(None, &inventory_of(&v), FROM).unwrap()),
            "07"
        );
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-26-3"][1]["state"] = json!("Booted");
        assert_eq!(
            udid_of(&resolve(None, &inventory_of(&v), FROM).unwrap()),
            "04"
        );
    }

    #[test]
    fn the_newest_model_goes_by_generation_number() {
        let model = |n: u32, name: &str, model: &str| {
            device(
                &format!("66666666-AAAA-BBBB-CCCC-0000000000{n:02}"),
                name,
                &format!("com.apple.CoreSimulator.SimDeviceType.{model}"),
                "Shutdown",
            )
        };
        let runtime = |devices: Vec<Value>| {
            inventory_of(&json!({ "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-26-1": devices } }))
        };
        let inv = runtime(vec![
            model(1, "iPhone SE (3rd generation)", "iPhone-SE-3rd-generation"),
            model(2, "iPhone Air", "iPhone-Air"),
            model(3, "iPhone 16e", "iPhone-16e"),
            model(4, "iPhone 17", "iPhone-17"),
            model(5, "iPhone 17 Pro", "iPhone-17-Pro"),
            model(6, "iPhone 9", "iPhone-9"),
        ]);
        assert_eq!(
            resolve(None, &inv, FROM).unwrap().device.name,
            "iPhone 17 Pro"
        );
        // A numbered model beats the letter-only ones, however old.
        let inv = runtime(vec![
            model(1, "iPhone SE (3rd generation)", "iPhone-SE-3rd-generation"),
            model(2, "iPhone Air", "iPhone-Air"),
            model(6, "iPhone 9", "iPhone-9"),
        ]);
        assert_eq!(resolve(None, &inv, FROM).unwrap().device.name, "iPhone 9");
        // Without a device type, the name is read the same way.
        let named = |name: &str| Device {
            udid: String::new(),
            name: name.to_owned(),
            family: Family::IPhone,
            device_type: String::new(),
            os: vec![26, 1],
            booted: false,
        };
        assert_eq!(generation(&named("iPhone 16e")), 16);
        assert_eq!(generation(&named("iPhone SE (3rd generation)")), 0);
    }

    #[test]
    fn a_missing_destination_names_the_booted_one_and_the_runtime_to_install() {
        let mut v = fixture();
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-26-3"][1]["state"] = json!("Booted");
        let inv = inventory_of(&v);
        let err = resolve(Some(&query(Some("iPhone 14"), Some("17.5"))), &inv, FROM).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Destination \"iPhone 14 · iOS 17.5\" (from .zed/.zedx/selection.json) is not \
             available; Test Phone A (iOS 26.3) is booted. Use Xcode: Choose Destination, or \
             install iOS 17.5 in Xcode ▸ Settings ▸ Components."
        );
        // The runtime exists, the simulator does not: nothing to install.
        let err = resolve(
            Some(&query(Some("iPhone 14"), Some("26.3"))),
            &inv,
            "--device",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Destination \"iPhone 14 · iOS 26.3\" (from --device) is not available; Test Phone \
             A (iOS 26.3) is booted. Use Xcode: Choose Destination."
        );
        // No iPhone at all.
        let empty = inventory_of(&json!({ "devices": {} }));
        assert_eq!(
            resolve(None, &empty, FROM).unwrap_err().to_string(),
            "No iPhone simulator is available: install an iOS simulator runtime in Xcode ▸ \
             Settings ▸ Components, or add an iPhone simulator in Xcode ▸ Window ▸ Devices and \
             Simulators."
        );
    }

    #[test]
    fn runtimes_versions_and_natural_order() {
        assert_eq!(
            runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-26-3"),
            Some(vec![26, 3])
        );
        assert_eq!(
            runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-9-0"),
            Some(vec![9, 0])
        );
        assert_eq!(
            runtime_version("com.apple.CoreSimulator.SimRuntime.watchOS-26-0"),
            None
        );
        assert_eq!(parse_version("26.3.1"), vec![26, 3, 1]);
        assert_eq!(parse_version("latest"), Vec::<u32>::new());
        assert!(os_matches(&[26, 3], &[26]));
        assert!(os_matches(&[26, 3], &[26, 3, 1]));
        assert!(!os_matches(&[26, 3], &[26, 4]));
        assert!(!os_matches(&[26, 3], &[2]));
        assert!(!os_matches(&[26, 3], &[]));
        assert_eq!(natural_cmp("iPhone-9", "iPhone-17"), Ordering::Less);
        assert_eq!(natural_cmp("iPhone-17-Pro", "iPhone-17"), Ordering::Greater);
        assert_eq!(natural_cmp("iPhone-17", "iPhone-17"), Ordering::Equal);
    }

    #[test]
    fn the_cache_expires_after_thirty_seconds() {
        let at = Instant::now();
        *CACHE.lock().unwrap() = Some((at, inventory_of(&fixture())));
        assert!(cached(at + Duration::from_secs(29)).is_some());
        assert!(cached(at + Duration::from_secs(30)).is_none());
        *CACHE.lock().unwrap() = None;
    }
}
