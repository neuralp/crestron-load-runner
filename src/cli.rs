use std::path::PathBuf;

pub const USAGE: &str = "\
Crestron Load Runner

Usage: crestron-load-runner [OPTIONS]

Options:
      --config-dir <DIR>  Keep the preferences and the firmware library in DIR
                          instead of the user configuration directory. Useful
                          for a throwaway profile that leaves the real
                          preferences untouched. Address books are files you
                          choose and are not stored here.
      --remove-data       Offer to remove the saved preferences, firmware
                          library, scripts and passwords, then exit. The
                          uninstaller runs this.
  -h, --help              Print this message and exit
";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub config_dir: Option<PathBuf>,
    pub remove_data: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Run(Options),
    Help,
}

/// Parses the arguments after the executable name.
pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Outcome, String> {
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Outcome::Help),
            "--remove-data" => options.remove_data = true,
            "--config-dir" => {
                let value = arguments
                    .next()
                    .ok_or("--config-dir needs a directory".to_owned())?;
                set_config_dir(&mut options, value)?;
            }
            _ => match argument.strip_prefix("--config-dir=") {
                Some(value) => set_config_dir(&mut options, value.to_owned())?,
                None => return Err(format!("Unrecognized argument: {argument}")),
            },
        }
    }
    Ok(Outcome::Run(options))
}

fn set_config_dir(options: &mut Options, value: String) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("--config-dir needs a directory".into());
    }
    if options.config_dir.is_some() {
        return Err("--config-dir was given more than once".into());
    }
    options.config_dir = Some(PathBuf::from(value));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_arguments(arguments: &[&str]) -> Result<Outcome, String> {
        parse(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    fn config_dir(arguments: &[&str]) -> Option<PathBuf> {
        match parse_arguments(arguments).unwrap() {
            Outcome::Run(options) => options.config_dir,
            Outcome::Help => panic!("expected a run outcome"),
        }
    }

    #[test]
    fn no_arguments_runs_against_the_user_configuration_directory() {
        assert_eq!(config_dir(&[]), None);
    }

    #[test]
    fn accepts_the_config_directory_as_one_or_two_arguments() {
        let expected = Some(PathBuf::from("/tmp/profile"));
        assert_eq!(config_dir(&["--config-dir", "/tmp/profile"]), expected);
        assert_eq!(config_dir(&["--config-dir=/tmp/profile"]), expected);
    }

    #[test]
    fn remove_data_is_off_unless_asked_for_and_combines_with_a_profile() {
        let run = |arguments: &[&str]| match parse_arguments(arguments).unwrap() {
            Outcome::Run(options) => options,
            Outcome::Help => panic!("expected a run outcome"),
        };
        assert!(!run(&[]).remove_data);
        assert!(run(&["--remove-data"]).remove_data);
        let both = run(&["--remove-data", "--config-dir", "/tmp/profile"]);
        assert!(both.remove_data);
        assert_eq!(both.config_dir, Some(PathBuf::from("/tmp/profile")));
    }

    #[test]
    fn help_wins_over_a_directory_and_stops_parsing() {
        assert_eq!(
            parse_arguments(&["--config-dir", "/tmp/profile", "--help"]).unwrap(),
            Outcome::Help
        );
        assert_eq!(parse_arguments(&["-h", "--bogus"]).unwrap(), Outcome::Help);
    }

    #[test]
    fn rejects_missing_empty_repeated_and_unknown_arguments() {
        for arguments in [
            vec!["--config-dir"],
            vec!["--config-dir", "  "],
            vec!["--config-dir="],
            vec!["--config-dir", "/tmp/a", "--config-dir", "/tmp/b"],
            vec!["--configdir", "/tmp/a"],
            vec!["/tmp/a"],
        ] {
            assert!(
                parse_arguments(&arguments).is_err(),
                "{arguments:?} should not parse"
            );
        }
    }
}
