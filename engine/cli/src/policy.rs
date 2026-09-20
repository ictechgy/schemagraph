//! 날짜와 설정은 CLI에서 검증하고 분석에는 재현 가능한 값만 전달한다.

use anyhow::{bail, Context, Result};
use schemagraph_analysis::{RetentionPolicy, Suppression};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    retain: Vec<String>,
    #[serde(default)]
    suppress: Vec<SuppressionFile>,
    #[serde(default, rename = "rule")]
    _rules: Vec<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppressionFile {
    pattern: String,
    reason: String,
    until: Option<String>,
}

/// 명시한 파일이 없으면 실패하지만 선택하지 않은 기본 설정 파일은 필수가 아니다.
pub(crate) fn load(
    path: Option<&Path>,
    retain: &[String],
    as_of: Option<&str>,
) -> Result<RetentionPolicy> {
    let default = PathBuf::from("schemagraph.toml");
    let selected = path.or_else(|| default.is_file().then_some(default.as_path()));
    let mut file = match selected {
        Some(p) => toml::from_str::<PolicyFile>(
            &std::fs::read_to_string(p)
                .with_context(|| format!("cannot read policy {}", p.display()))?,
        )
        .context("invalid retention policy TOML")?,
        None => PolicyFile::default(),
    };
    file.retain.extend_from_slice(retain);
    if file.retain.iter().any(|p| p.trim().is_empty()) {
        bail!("retain patterns must be nonempty");
    }
    if let Some(date) = as_of {
        validate_date(date)?;
    }
    let mut suppressions = Vec::new();
    for s in file.suppress {
        if s.pattern.trim().is_empty() || s.reason.trim().is_empty() {
            bail!("suppression pattern and reason must be nonempty");
        }
        if let Some(until) = &s.until {
            validate_date(until)?;
            if as_of.is_none() {
                bail!(
                    "expiring suppressions require --as-of YYYY-MM-DD for reproducible evaluation"
                );
            }
        }
        suppressions.push(Suppression {
            pattern: s.pattern,
            reason: s.reason,
            until: s.until,
        });
    }
    Ok(RetentionPolicy {
        retain: file.retain,
        suppressions,
        as_of: as_of.map(str::to_owned),
    })
}

fn validate_date(date: &str) -> Result<()> {
    let valid = || -> Option<bool> {
        let bytes = date.as_bytes();
        if bytes.len() != 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes
                .iter()
                .enumerate()
                .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
        {
            return None;
        }
        let year: u32 = date[..4].parse().ok()?;
        let month: usize = date[5..7].parse().ok()?;
        let day: u32 = date[8..].parse().ok()?;
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        Some(year > 0 && (1..=12).contains(&month) && day > 0 && day <= days[month - 1])
    };
    if valid() != Some(true) {
        bail!("invalid date '{date}'; use a valid YYYY-MM-DD date");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_calendar_boundaries() {
        for date in ["2024-02-29", "2026-09-20"] {
            assert!(validate_date(date).is_ok());
        }
        for date in [
            "2026-02-29",
            "2026-00-01",
            "2026-13-01",
            "2026-04-31",
            "２０２６-01-01",
        ] {
            assert!(validate_date(date).is_err());
        }
    }
}
