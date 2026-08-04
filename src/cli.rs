use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "/etc/rockpi-penta.conf";
const DEFAULT_ENV_FILE: &str = "/etc/rockpi-penta.env";
const DEFAULT_CPU_TEMP_PATH: &str = "/sys/class/thermal/thermal_zone0/temp";

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Args {
    pub config_path: PathBuf,
    pub env_file: PathBuf,
    pub cpu_temp_path: PathBuf,
    pub dry_run: bool,
    pub once: bool,
    pub help: bool,
    pub version: bool,
    pub test_fan_duty: Option<u8>,
    pub test_fan_seconds: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            env_file: PathBuf::from(DEFAULT_ENV_FILE),
            cpu_temp_path: PathBuf::from(DEFAULT_CPU_TEMP_PATH),
            dry_run: false,
            once: false,
            help: false,
            version: false,
            test_fan_duty: None,
            test_fan_seconds: 10,
        }
    }
}

impl Args {
    pub fn parse() -> Result<Self, String> {
        Self::parse_from(env::args_os())
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut parsed = Self::default();
        let mut iter = args.into_iter().map(Into::into);

        let _program = iter.next();
        while let Some(arg) = iter.next() {
            let arg = arg
                .into_string()
                .map_err(|_| "arguments must be valid UTF-8".to_string())?;

            if arg == "--help" || arg == "-h" {
                parsed.help = true;
            } else if arg == "--version" || arg == "-V" {
                parsed.version = true;
            } else if arg == "--dry-run" {
                parsed.dry_run = true;
            } else if arg == "--once" {
                parsed.once = true;
            } else if let Some(value) = arg.strip_prefix("--config=") {
                parsed.config_path = PathBuf::from(value);
            } else if arg == "--config" {
                parsed.config_path = next_path(&mut iter, "--config")?;
            } else if let Some(value) = arg.strip_prefix("--env-file=") {
                parsed.env_file = PathBuf::from(value);
            } else if arg == "--env-file" {
                parsed.env_file = next_path(&mut iter, "--env-file")?;
            } else if let Some(value) = arg.strip_prefix("--cpu-temp-path=") {
                parsed.cpu_temp_path = PathBuf::from(value);
            } else if arg == "--cpu-temp-path" {
                parsed.cpu_temp_path = next_path(&mut iter, "--cpu-temp-path")?;
            } else if let Some(value) = arg.strip_prefix("--test-fan-duty=") {
                parsed.test_fan_duty = Some(parse_percent(value, "--test-fan-duty")?);
            } else if arg == "--test-fan-duty" {
                let value = next_string(&mut iter, "--test-fan-duty")?;
                parsed.test_fan_duty = Some(parse_percent(&value, "--test-fan-duty")?);
            } else if let Some(value) = arg.strip_prefix("--test-fan-seconds=") {
                parsed.test_fan_seconds = parse_seconds(value, "--test-fan-seconds")?;
            } else if arg == "--test-fan-seconds" {
                let value = next_string(&mut iter, "--test-fan-seconds")?;
                parsed.test_fan_seconds = parse_seconds(&value, "--test-fan-seconds")?;
            } else {
                return Err(format!("unknown argument: {arg}"));
            }
        }

        Ok(parsed)
    }
}

fn next_path<I>(iter: &mut I, flag: &str) -> Result<PathBuf, String>
where
    I: Iterator<Item = OsString>,
{
    Ok(PathBuf::from(next_string(iter, flag)?))
}

fn next_string<I>(iter: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = OsString>,
{
    let value = iter
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?
        .into_string()
        .map_err(|_| format!("{flag} value must be valid UTF-8"))?;

    if value.is_empty() {
        return Err(format!("{flag} value must not be empty"));
    }

    Ok(value)
}

fn parse_percent(value: &str, flag: &str) -> Result<u8, String> {
    let percent = value
        .parse::<u8>()
        .map_err(|_| format!("{flag} must be an integer from 0 to 100"))?;

    if percent > 100 {
        return Err(format!("{flag} must be an integer from 0 to 100"));
    }

    Ok(percent)
}

fn parse_seconds(value: &str, flag: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be a positive integer"))?;

    if seconds == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }

    Ok(seconds)
}

pub fn usage(program: &str) -> String {
    format!(
        "\
Usage: {program} [OPTIONS]

Options:
      --config <PATH>         Config file path [default: {DEFAULT_CONFIG_PATH}]
      --env-file <PATH>       Board pin env file path [default: {DEFAULT_ENV_FILE}]
      --cpu-temp-path <PATH>  CPU temp path [default: {DEFAULT_CPU_TEMP_PATH}]
      --dry-run               Print fan decisions without hardware output
      --once                  Take one sample and exit
      --test-fan-duty <0-100> Set fan duty for a bounded manual test
      --test-fan-seconds <N>  Manual fan test duration [default: 10]
  -V, --version               Print version
  -h, --help                  Show this help
"
    )
}

pub fn version() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults() {
        let args = Args::parse_from(["daemon"]).expect("default args should parse");
        assert_eq!(args.config_path, PathBuf::from(DEFAULT_CONFIG_PATH));
        assert_eq!(args.env_file, PathBuf::from(DEFAULT_ENV_FILE));
        assert_eq!(args.cpu_temp_path, PathBuf::from(DEFAULT_CPU_TEMP_PATH));
        assert!(!args.dry_run);
        assert!(!args.once);
        assert!(!args.version);
        assert_eq!(args.test_fan_duty, None);
        assert_eq!(args.test_fan_seconds, 10);
    }

    #[test]
    fn parses_flags_and_paths() {
        let args = Args::parse_from([
            "daemon",
            "--config",
            "/tmp/config.ini",
            "--env-file=/tmp/pins.env",
            "--cpu-temp-path",
            "/tmp/temp",
            "--dry-run",
            "--once",
            "--test-fan-duty",
            "25",
            "--test-fan-seconds=3",
        ])
        .expect("explicit args should parse");

        assert_eq!(args.config_path, PathBuf::from("/tmp/config.ini"));
        assert_eq!(args.env_file, PathBuf::from("/tmp/pins.env"));
        assert_eq!(args.cpu_temp_path, PathBuf::from("/tmp/temp"));
        assert!(args.dry_run);
        assert!(args.once);
        assert_eq!(args.test_fan_duty, Some(25));
        assert_eq!(args.test_fan_seconds, 3);
    }

    #[test]
    fn rejects_invalid_fan_test_duty() {
        let err = Args::parse_from(["daemon", "--test-fan-duty", "101"])
            .expect_err("invalid percent should fail");

        assert!(err.contains("--test-fan-duty"));
    }

    #[test]
    fn parses_and_reports_version() {
        let args = Args::parse_from(["daemon", "--version"]).expect("version flag should parse");

        assert!(args.version);
        assert_eq!(version(), "radxa-penta-top-hat-rs 1.0.1+local.4");
    }
}
