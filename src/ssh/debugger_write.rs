//! Typed signal writes. Console syntax and byte escapes verified on an RMC3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalValue {
    Digital(bool),
    Pulse(u16),
    Hold,
    Release,
    Analog(u16),
    Serial(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalWrite {
    pub signum: u32,
    pub value: SignalValue,
}

impl SignalWrite {
    pub(crate) fn command(&self, slot: u8) -> Result<String, String> {
        if !(1..=10).contains(&slot) {
            return Err("Program slot must be 1 through 10".into());
        }
        let value = match &self.value {
            SignalValue::Digital(high) => u8::from(*high).to_string(),
            SignalValue::Hold => "1".into(),
            SignalValue::Release => "0".into(),
            SignalValue::Pulse(0) => return Err("Pulse duration must be 1–65535 ms".into()),
            SignalValue::Pulse(ms) => format!("p{ms}"),
            SignalValue::Analog(value) => value.to_string(),
            SignalValue::Serial(text) => {
                // Never allow text to escape the quoted argument or introduce
                // another console command. Encode Unicode as UTF-8 bytes.
                let mut escaped = String::from("\"");
                for byte in text.bytes() {
                    match byte {
                        b' '..=b'~' if byte != b'"' && byte != b'\\' => escaped.push(byte as char),
                        _ => escaped.push_str(&format!("\\x{byte:02X}")),
                    }
                    if escaped.len() > 560 {
                        return Err("String is too long for one console command (560 encoded characters maximum)".into());
                    }
                }
                escaped.push('"');
                escaped
            }
        };
        Ok(format!("SETSIGNAL:{slot} 0x{:08X} {value}\r", self.signum))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_use_verified_slot_scoped_syntax() {
        for (value, expected) in [
            (SignalValue::Digital(true), "1"),
            (SignalValue::Digital(false), "0"),
            (SignalValue::Hold, "1"),
            (SignalValue::Release, "0"),
            (SignalValue::Pulse(100), "p100"),
            (SignalValue::Analog(0), "0"),
            (SignalValue::Analog(65535), "65535"),
            (SignalValue::Serial(String::new()), "\"\""),
        ] {
            assert_eq!(
                SignalWrite {
                    signum: 0x10c,
                    value
                }
                .command(3)
                .unwrap(),
                format!("SETSIGNAL:3 0x0000010C {expected}\r")
            );
        }
    }

    #[test]
    fn strings_cannot_inject_console_commands() {
        let write = SignalWrite {
            signum: 1,
            value: SignalValue::Serial("A\"\\\r\nB\0é".into()),
        };
        assert_eq!(
            write.command(1).unwrap(),
            "SETSIGNAL:1 0x00000001 \"A\\x22\\x5C\\x0D\\x0AB\\x00\\xC3\\xA9\"\r"
        );
        assert!(write.command(0).is_err());
        assert!(
            SignalWrite {
                signum: 1,
                value: SignalValue::Pulse(0)
            }
            .command(1)
            .is_err()
        );
        assert!(
            SignalWrite {
                signum: 1,
                value: SignalValue::Serial("x".repeat(600))
            }
            .command(1)
            .is_err()
        );
    }
}
