//! Parsing Docker CLI output (labels, sizes, timestamps, system df) into observed artifacts.

use super::*;

/// Parse Docker's comma-joined `Labels` field.
///
/// Docker joins `key=value` pairs with a comma and does not escape a comma inside a value, so
/// a value containing a comma is unresolvable here. That limitation is why every consequence
/// of a missing or malformed label is "keep".
#[must_use]
pub fn parse_label_list(raw: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.split_once('=') {
            Some((key, value)) => {
                labels.insert(key.trim().to_owned(), value.to_owned());
            }
            None => {
                labels.insert(entry.to_owned(), String::new());
            }
        }
    }
    labels
}

/// Parse a Docker human-unit size into bytes.
///
/// Docker uses decimal units (`kB`, `MB`, `GB`, …) in its accounting output and appends `*`
/// to approximate buildx figures. Anything unrecognised, including `N/A`, is `None` so the
/// caller can fail closed rather than treat it as zero.
#[must_use]
pub fn parse_docker_size(raw: &str) -> Option<i128> {
    let raw = raw.trim().trim_end_matches('*').trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("n/a") {
        return None;
    }
    let split = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let multiplier: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        "KiB" => 1024.0,
        "MiB" => 1024.0f64.powi(2),
        "GiB" => 1024.0f64.powi(3),
        "TiB" => 1024.0f64.powi(4),
        "PiB" => 1024.0f64.powi(5),
        _ => return None,
    };
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes > i128::MAX as f64 {
        return None;
    }
    Some(bytes.round() as i128)
}

/// Parse the timestamp formats Docker's accounting output uses, as Unix seconds.
///
/// Observed shapes are `YYYY-MM-DD HH:MM:SS ±HHMM TZ` (images, containers) and
/// `YYYY-MM-DD HH:MM:SS[.fraction] ±HHMM TZ` (build cache). The numeric offset is used
/// directly, so no timezone database is involved. The trailing zone abbreviation is ignored.
#[must_use]
pub fn parse_docker_timestamp(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let (date, rest) = raw.split_once(' ')?;
    let (time, rest) = rest.trim().split_once(' ')?;
    let (year, month, day) = {
        let mut parts = date.split('-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        (year, month, day)
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    let (hour, minute, second) = {
        let mut parts = clock.split(':');
        let hour: i64 = parts.next()?.parse().ok()?;
        let minute: i64 = parts.next()?.parse().ok()?;
        let second: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        (hour, minute, second)
    };
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let sub_second = match fraction {
        Some(fraction) => {
            if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let digits: String = fraction.chars().take(9).collect();
            let scale = 10f64.powi(i32::try_from(digits.len()).ok()?);
            digits.parse::<f64>().ok()? / scale
        }
        None => 0.0,
    };
    // The numeric offset is the first token after the time; a missing offset is unknown, and
    // unknown resolves to no age, which protects.
    let offset_token = rest.trim().split(' ').next()?;
    let offset_seconds = parse_offset(offset_token)?;
    let days = days_from_civil(year, month, day);
    let seconds = days as f64 * 86_400.0 + (hour * 3600 + minute * 60 + second) as f64 + sub_second
        - offset_seconds as f64;
    seconds.is_finite().then_some(seconds)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parse the RFC 3339 form `docker inspect` uses, as Unix seconds.
///
/// `docker volume inspect` and `docker network inspect` report `2026-09-13T18:41:43-07:00`,
/// with a `T` separator and a colon in the offset. Both are normalised to the shape
/// [`parse_docker_timestamp`] already handles, so one numeric-offset path covers every
/// timestamp this crate reads.
#[must_use]
pub fn parse_rfc3339_timestamp(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    // Only a `T` in the date/time separator position marks RFC 3339. A zone abbreviation such
    // as `UTC` also contains a `T`, and mistaking that for the separator would split the
    // string mid-word.
    if trimmed.as_bytes().get(10) != Some(&b'T') {
        return parse_docker_timestamp(trimmed);
    }
    let (Some(date), Some(rest)) = (trimmed.get(..10), trimmed.get(11..)) else {
        return None;
    };
    if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        return parse_docker_timestamp(&format!("{date} {clock} +0000"));
    }
    // The offset is the last sign in the remainder; there is no space before it, unlike the
    // accounting format, so one is inserted here.
    let split = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(split);
    parse_docker_timestamp(&format!("{date} {clock} {offset}"))
}

/// Parse `+HHMM` / `-HHMM` (also accepting `+HH:MM`) into seconds east of UTC.
pub(crate) fn parse_offset(raw: &str) -> Option<i64> {
    let (sign, digits) = match raw.as_bytes().first()? {
        b'+' => (1i64, &raw[1..]),
        b'-' => (-1i64, &raw[1..]),
        _ => return None,
    };
    let digits: String = digits.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// One section of `docker system df -v --format json`.
#[derive(Debug, Deserialize)]
pub struct SystemDfReport {
    #[serde(rename = "Images", default)]
    pub images: Vec<DfImage>,
    #[serde(rename = "Containers", default)]
    pub containers: Vec<DfContainer>,
    #[serde(rename = "Volumes", default)]
    pub volumes: Vec<DfVolume>,
    #[serde(rename = "BuildCache", default)]
    pub build_cache: Vec<DfBuildCache>,
}

#[derive(Debug, Deserialize)]
pub struct DfImage {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "Repository", default)]
    pub repository: String,
    #[serde(rename = "Tag", default)]
    pub tag: String,
    #[serde(rename = "Digest", default)]
    pub digest: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "Containers", default)]
    pub containers: String,
}

#[derive(Debug, Deserialize)]
pub struct DfContainer {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "Labels", default)]
    pub labels: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "State", default)]
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct DfVolume {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Labels", default)]
    pub labels: String,
    #[serde(rename = "Links", default)]
    pub links: String,
    #[serde(rename = "Size", default)]
    pub size: String,
}

#[derive(Debug, Deserialize)]
pub struct DfBuildCache {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "InUse", default)]
    pub in_use: String,
}

/// One volume's detail from `docker volume inspect`.
///
/// The accounting document reports no volume creation time, so this is the only source of
/// volume age. Labels arrive as a map here rather than as the joined string the accounting
/// document uses.
#[derive(Debug, Deserialize)]
pub struct InspectedVolume {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    /// `docker inspect` reports `null` rather than `{}` for an unlabelled object, so this
    /// cannot be a bare map.
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    pub labels: BTreeMap<String, String>,
}

pub(crate) fn null_as_empty_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Everything one engine read pass produced.
#[derive(Debug)]
pub struct EngineObservation<'a> {
    pub report: &'a SystemDfReport,
    /// Image IDs Docker itself reports as dangling.
    pub dangling_image_ids: &'a [String],
    /// Image IDs carrying any Bosn label.
    pub bosn_labeled_image_ids: &'a [String],
    /// Volume detail. A volume absent here has no known age and is protected accordingly.
    pub inspected_volumes: &'a [InspectedVolume],
    pub now: f64,
}

/// Parse one engine read pass into observations.
///
/// Image ownership cannot be read from the accounting document, because `docker image ls`
/// does not expose labels, so the caller supplies the ID sets it gathered with the engine's
/// label and dangling filters.
#[must_use]
pub fn observe(input: EngineObservation<'_>) -> Vec<ObservedArtifact> {
    let report = input.report;
    let now = input.now;
    let labeled: std::collections::BTreeSet<&str> = input
        .bosn_labeled_image_ids
        .iter()
        .map(String::as_str)
        .collect();
    let dangling_ids: std::collections::BTreeSet<&str> = input
        .dangling_image_ids
        .iter()
        .map(String::as_str)
        .collect();
    let inspected: BTreeMap<&str, &InspectedVolume> = input
        .inspected_volumes
        .iter()
        .map(|volume| (volume.name.as_str(), volume))
        .collect();
    let mut observed = Vec::new();
    for image in &report.images {
        let age = parse_docker_timestamp(&image.created_at).map(|at| now - at);
        let in_use = parse_count(&image.containers).unwrap_or(1) > 0;
        // Docker's own filter is authoritative: an untagged image that is still the parent
        // of a tagged one is not dangling, and must not be offered as reclaimable.
        let dangling = dangling_ids.contains(image.id.as_str());
        let labels = if labeled.contains(image.id.as_str()) {
            // The engine proved this image carries a Bosn label but cannot cheaply prove
            // *which*; an incomplete map resolves to the protective `IncompleteLabels`.
            BTreeMap::from([(LABEL_REGISTRY.to_owned(), String::new())])
        } else {
            BTreeMap::new()
        };
        observed.push(ObservedArtifact {
            id: image.id.clone(),
            kind: ResourceKind::Image,
            labels,
            signals: Signals {
                in_use,
                dangling,
                anonymous: false,
            },
            bytes: parse_docker_size(&image.size),
            age_seconds: age,
        });
    }
    for container in &report.containers {
        let age = parse_docker_timestamp(&container.created_at).map(|at| now - at);
        let labels = parse_label_list(&container.labels);
        observed.push(ObservedArtifact {
            id: container.id.clone(),
            kind: ResourceKind::Container,
            labels,
            signals: Signals {
                in_use: container.state != "exited" && container.state != "created",
                dangling: false,
                anonymous: false,
            },
            bytes: parse_docker_size(&container.size),
            age_seconds: age,
        });
    }
    for volume in &report.volumes {
        let detail = inspected.get(volume.name.as_str());
        // Prefer the inspected label map when it is available; the accounting document's
        // joined string cannot represent a label value containing a comma.
        let labels = match detail {
            Some(detail) if !detail.labels.is_empty() => detail.labels.clone(),
            _ => parse_label_list(&volume.labels),
        };
        let anonymous = labels.contains_key("com.docker.volume.anonymous")
            || is_anonymous_volume_name(&volume.name);
        observed.push(ObservedArtifact {
            id: volume.name.clone(),
            kind: ResourceKind::Volume,
            labels,
            signals: Signals {
                in_use: parse_count(&volume.links).unwrap_or(1) > 0,
                dangling: false,
                anonymous,
            },
            bytes: parse_docker_size(&volume.size),
            // Without inspect detail there is no creation time, so the volume is protected
            // as unmeasured rather than assumed old.
            age_seconds: detail
                .and_then(|detail| parse_rfc3339_timestamp(&detail.created_at))
                .map(|at| now - at),
        });
    }
    for entry in &report.build_cache {
        let age = parse_docker_timestamp(&entry.created_at).map(|at| now - at);
        observed.push(ObservedArtifact {
            id: entry.id.clone(),
            kind: ResourceKind::Builder,
            labels: BTreeMap::new(),
            signals: Signals {
                in_use: entry.in_use.eq_ignore_ascii_case("true"),
                dangling: false,
                anonymous: false,
            },
            bytes: parse_docker_size(&entry.size),
            age_seconds: age,
        });
    }
    observed
}

pub(crate) fn is_anonymous_volume_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn parse_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}
