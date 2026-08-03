use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PinMap {
    pub sda: Option<String>,
    pub scl: Option<String>,
    pub oled_reset: Option<String>,
    pub oled_i2c_device: Option<String>,
    pub button_chip: Option<String>,
    pub button_line: Option<u32>,
    pub button_mode: ButtonMode,
    pub fan_chip: Option<String>,
    pub fan_line: Option<u32>,
    pub pwmchip: Option<String>,
    pub hardware_pwm: bool,
    pub raw: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ButtonMode {
    Edge,
    OutputPoll,
}

impl PinMap {
    pub fn from_file_or_empty(path: &Path) -> Result<Self, EnvFileError> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(err) => Err(EnvFileError::Io(err)),
        }
    }

    pub fn empty() -> Self {
        Self {
            sda: None,
            scl: None,
            oled_reset: None,
            oled_i2c_device: None,
            button_chip: None,
            button_line: None,
            button_mode: ButtonMode::Edge,
            fan_chip: None,
            fan_line: None,
            pwmchip: None,
            hardware_pwm: false,
            raw: BTreeMap::new(),
        }
    }

    pub fn parse(input: &str) -> Result<Self, EnvFileError> {
        let mut raw = BTreeMap::new();

        for (idx, raw_line) in input.lines().enumerate() {
            let line_number = idx + 1;
            let line = strip_comment(raw_line).trim();

            if line.is_empty() {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                return Err(EnvFileError::Parse {
                    line: line_number,
                    message: "expected KEY=value".to_string(),
                });
            };

            let key = key.trim();
            if key.is_empty() {
                return Err(EnvFileError::Parse {
                    line: line_number,
                    message: "empty key".to_string(),
                });
            }

            raw.insert(key.to_string(), unquote(value.trim()));
        }

        let button_line = parse_optional_u32(&raw, "BUTTON_LINE")?;
        let fan_line = parse_optional_u32(&raw, "FAN_LINE")?;
        let hardware_pwm = match raw
            .get("HARDWARE_PWM")
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("1") | Some("true") => true,
            Some("0") | Some("false") | None => false,
            Some(value) => {
                return Err(EnvFileError::Value {
                    key: "HARDWARE_PWM".to_string(),
                    value: value.to_string(),
                    expected: "0/1 boolean",
                });
            }
        };
        let button_mode = match raw
            .get("BUTTON_MODE")
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("output-poll") => ButtonMode::OutputPoll,
            Some("edge") | None => ButtonMode::Edge,
            Some(value) => {
                return Err(EnvFileError::Value {
                    key: "BUTTON_MODE".to_string(),
                    value: value.to_string(),
                    expected: "edge or output-poll",
                });
            }
        };

        Ok(Self {
            sda: raw.get("SDA").cloned(),
            scl: raw.get("SCL").cloned(),
            oled_reset: raw.get("OLED_RESET").cloned(),
            oled_i2c_device: raw
                .get("OLED_I2C_DEVICE")
                .or_else(|| raw.get("I2C_DEVICE"))
                .cloned(),
            button_chip: raw.get("BUTTON_CHIP").cloned(),
            button_line,
            button_mode,
            fan_chip: raw.get("FAN_CHIP").cloned(),
            fan_line,
            pwmchip: raw.get("PWMCHIP").cloned(),
            hardware_pwm,
            raw,
        })
    }
}

fn strip_comment(line: &str) -> &str {
    line.split_once('#')
        .map(|(prefix, _)| prefix)
        .unwrap_or(line)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }

    value.to_string()
}

fn parse_optional_u32(
    raw: &BTreeMap<String, String>,
    key: &'static str,
) -> Result<Option<u32>, EnvFileError> {
    raw.get(key)
        .map(|value| {
            value.parse::<u32>().map_err(|_| EnvFileError::Value {
                key: key.to_string(),
                value: value.clone(),
                expected: "unsigned integer",
            })
        })
        .transpose()
}

#[derive(Debug)]
pub enum EnvFileError {
    Io(io::Error),
    Parse {
        line: usize,
        message: String,
    },
    Value {
        key: String,
        value: String,
        expected: &'static str,
    },
}

impl std::fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "env file I/O error: {err}"),
            Self::Parse { line, message } => {
                write!(f, "env file parse error on line {line}: {message}")
            }
            Self::Value {
                key,
                value,
                expected,
            } => write!(
                f,
                "invalid env value for {key}: {value:?}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for EnvFileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rpi5_env_file() {
        let pins = PinMap::parse(
            r#"
            SDA=SDA
            SCL=SCL
            OLED_RESET=D23
            OLED_I2C_DEVICE=/dev/i2c-1
            BUTTON_CHIP=/dev/gpiochip4
            BUTTON_LINE=17
            FAN_CHIP=/dev/gpiochip4
            FAN_LINE=27
            HARDWARE_PWM=0
            "#,
        )
        .expect("pin map should parse");

        assert_eq!(pins.sda.as_deref(), Some("SDA"));
        assert_eq!(pins.oled_i2c_device.as_deref(), Some("/dev/i2c-1"));
        assert_eq!(pins.button_chip.as_deref(), Some("/dev/gpiochip4"));
        assert_eq!(pins.button_line, Some(17));
        assert_eq!(pins.button_mode, ButtonMode::Edge);
        assert_eq!(pins.fan_line, Some(27));
        assert!(!pins.hardware_pwm);
    }
}
