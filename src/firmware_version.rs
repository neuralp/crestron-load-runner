//! Strict firmware preflight: package Version versus the live `ver -v` PUF line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeCheck {
    Needed(String),
    NotNeeded(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    text: String,
    parts: Vec<u64>,
}

impl Version {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let mut parts = Vec::new();
        for part in text.split('.') {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err("Expected a dotted numeric firmware version".into());
            }
            parts.push(
                part.parse::<u64>()
                    .map_err(|_| "Firmware version component is too large")?,
            );
        }
        if parts.len() < 2 {
            return Err("Expected a dotted numeric firmware version".into());
        }
        // Leading zeroes are numeric padding; absent trailing components are zero.
        while parts.last() == Some(&0) {
            parts.pop();
        }
        Ok(Self {
            text: text.to_owned(),
            parts,
        })
    }

    pub fn from_package(package: &[(String, String)]) -> Result<Self, String> {
        let mut values = package
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("Version"));
        let (_, value) = values
            .next()
            .ok_or("Firmware blocked: package has no [Package] Version")?;
        if values.next().is_some() {
            return Err("Firmware blocked: package contains multiple Version fields".into());
        }
        Self::parse(value).map_err(|e| format!("Firmware blocked: invalid package Version: {e}"))
    }

    fn from_report(report: &str) -> Result<Self, String> {
        let mut values = report.lines().filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case("PUF").then_some(value)
        });
        let value = values
            .next()
            .ok_or("Firmware blocked: ver -v returned no PUF: version")?;
        if values.next().is_some() {
            return Err("Firmware blocked: ver -v returned multiple PUF: lines".into());
        }
        Self::parse(value)
            .map_err(|e| format!("Firmware blocked: invalid device PUF: version: {e}"))
    }

    pub fn check_upgrade(&self, report: &str) -> Result<UpgradeCheck, String> {
        let installed = Self::from_report(report)?;
        if self.parts <= installed.parts {
            return Ok(UpgradeCheck::NotNeeded(format!(
                "Firmware upgrade not needed: package {} is {} device PUF {}",
                self.text,
                if self.parts == installed.parts {
                    "the same version as"
                } else {
                    "older than"
                },
                installed.text
            )));
        }
        Ok(UpgradeCheck::Needed(format!(
            "Firmware version check passed: device PUF {} -> package {}",
            installed.text, self.text
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const REPORT: &str = "RMC3 Cntrl Eng [v1.8001.6192.21873 (Dec 15 2025)]\r\nBuild: 12:09:07 Dec 15 2025 (6192.21873)\r\nCab: 1.8001.0298\r\nApplications: 1.0.9476.32029\r\nUpdater: 1.4.31\r\nBootloader: 1.23.00\r\nIOPVersion: FPGA [v09] slot:7\r\nRMC3-SetupProgram: 1.003.0024\r\nEthernet Phy: Rev1\r\nPUF: 1.8001.0298\r\nFORCED_AUTH_MODE: False\r\n";

    #[test]
    fn only_strictly_newer_packages_pass_using_puf_not_engine_or_cab() {
        for older_or_equal in [
            "1.8001.0297",
            "1.8001.0298",
            "1.8001.298",
            "1.8001.0298.0",
            "1.7999.9999",
        ] {
            assert!(matches!(
                Version::parse(older_or_equal)
                    .unwrap()
                    .check_upgrade(REPORT),
                Ok(UpgradeCheck::NotNeeded(_))
            ));
        }
        for newer in ["1.8001.0299", "1.8001.1000", "1.8002.0001", "2.0000.0000"] {
            assert!(Version::parse(newer).unwrap().check_upgrade(REPORT).is_ok());
        }
        assert!(
            Version::parse("1.10.0")
                .unwrap()
                .check_upgrade("PUF: 1.9.0")
                .is_ok()
        );
        assert!(
            Version::parse("1.2.0")
                .unwrap()
                .check_upgrade("Cab: 9.9.9\nPUF: 1.1.0")
                .is_ok()
        );
    }

    #[test]
    fn missing_ambiguous_or_malformed_versions_fail_closed() {
        let package = Version::parse("1.8001.0300").unwrap();
        for report in [
            "Cab: 1.8001.0298",
            "invalid command",
            "PUF:",
            "PUF: v1.2.3",
            "PUF: 1..2",
            "PUF: 1.2-beta",
            "PUF: 1.2\nPUF: 1.3",
            "PUF: 18446744073709551616.0",
        ] {
            assert!(package.check_upgrade(report).is_err());
        }
        assert!(Version::from_package(&[]).is_err());
        assert!(Version::from_package(&[("Version".into(), "bad".into())]).is_err());
        assert!(
            Version::from_package(&[
                ("Version".into(), "1.2".into()),
                ("version".into(), "1.3".into())
            ])
            .is_err()
        );
        assert!(
            Version::from_package(&[("version".into(), "1.8001.0300".into())])
                .unwrap()
                .check_upgrade(REPORT)
                .is_ok()
        );
    }
}
