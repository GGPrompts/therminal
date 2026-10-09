//! Launcher profile editing. Arguments use quotes to keep spaces within one argument.
use super::sections::{SettingsRenderValues, restore_control_states, snapshot_control_states};
use super::state::SettingsOverlayState;
use super::types::{ControlBinding, ControlType, ProfileField, SettingsCommand, SettingsControl};
use std::collections::HashMap;
use therminal_core::config::ProfileConfig;

fn names(profiles: &HashMap<String, ProfileConfig>) -> Vec<String> {
    let mut names: Vec<_> = profiles.keys().cloned().collect();
    names.sort();
    names
}

fn mode(profile: &ProfileConfig) -> usize {
    if profile.url.is_some() {
        2
    } else if profile.command.is_some() {
        1
    } else {
        0
    }
}

impl SettingsOverlayState {
    pub(super) fn rebuild_profiles_section(&mut self, values: &SettingsRenderValues) {
        let Some(index) = self.sections.iter().position(|s| s.id == "profiles") else {
            return;
        };
        let names = names(&values.profiles);
        let selected = self
            .selected_profile
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name))
            .unwrap_or(0);
        self.selected_profile = names.get(selected).cloned();
        let old = &self.sections[index].controls;
        let snapshots = snapshot_control_states(old);
        let mut controls = vec![SettingsControl::new(
            "Add profile",
            ControlBinding::ProfileAdd,
        )];
        if let Some(name) = &self.selected_profile {
            let profile = &values.profiles[name];
            controls.push(SettingsControl::with_type(
                "Profile",
                ControlBinding::ProfileSelect,
                ControlType::select(names, selected),
            ));
            controls.push(SettingsControl::with_type(
                "Name",
                ControlBinding::ProfileText(ProfileField::Name),
                ControlType::text_input(name),
            ));
            controls.push(SettingsControl::with_type(
                "Launch type",
                ControlBinding::ProfileMode,
                ControlType::select(
                    vec!["Shell".into(), "Command".into(), "Web page".into()],
                    mode(profile),
                ),
            ));
            let (label, program) = match mode(profile) {
                2 => ("URL", profile.url.as_deref()),
                1 => ("Command", profile.command.as_deref()),
                _ => ("Shell (blank = default)", profile.shell.as_deref()),
            };
            for (label, field, value) in [
                (
                    label,
                    ProfileField::Program,
                    program.unwrap_or_default().to_string(),
                ),
                (
                    "Arguments (quote spaces)",
                    ProfileField::Arguments,
                    format_arguments(&profile.shell_args),
                ),
                (
                    "Working directory",
                    ProfileField::Directory,
                    profile.working_directory.clone().unwrap_or_default(),
                ),
                (
                    "Environment (e.g. Windows or WSL Ubuntu)",
                    ProfileField::Environment,
                    profile.environment.clone().unwrap_or_default(),
                ),
                (
                    "Shell label (e.g. PowerShell or Bash)",
                    ProfileField::ShellLabel,
                    profile.shell_label.clone().unwrap_or_default(),
                ),
                (
                    "Icon",
                    ProfileField::Icon,
                    profile.icon.clone().unwrap_or_default(),
                ),
                (
                    "Color (#RGB or #RRGGBB)",
                    ProfileField::Color,
                    profile.color.clone().unwrap_or_default(),
                ),
            ] {
                // Command and URL profiles do not consume shell arguments.
                if field == ProfileField::Arguments && mode(profile) != 0 {
                    continue;
                }
                controls.push(SettingsControl::with_type(
                    label,
                    ControlBinding::ProfileText(field),
                    ControlType::text_input(&value),
                ));
            }
            controls.push(SettingsControl::new(
                "Remove profile",
                ControlBinding::ProfileRemove,
            ));
        }
        // Preserve active edits during per-frame rebuilds, only when the form matches.
        if old.len() == controls.len()
            && old
                .iter()
                .zip(&controls)
                .all(|(a, b)| a.binding == b.binding)
        {
            restore_control_states(&mut controls, &snapshots);
        }
        self.sections[index].controls = controls;
        let max = self.sections[index].controls.len().saturating_sub(1);
        self.selected_control_by_section[index] = self.selected_control_by_section[index].min(max);
    }
}

pub(crate) fn apply_profile_command(
    profiles: &mut HashMap<String, ProfileConfig>,
    selected: &mut Option<String>,
    command: &SettingsCommand,
) -> Option<Result<(), String>> {
    use SettingsCommand::*;
    match command {
        SelectProfile(index) => {
            *selected = names(profiles).get(*index).cloned();
        }
        AddProfile => {
            let name = (1..)
                .map(|n| format!("Profile {n}"))
                .find(|n| !profiles.contains_key(n))
                .unwrap();
            profiles.insert(name.clone(), ProfileConfig::default());
            *selected = Some(name);
        }
        RemoveProfile => {
            if let Some(name) = selected.take() {
                profiles.remove(&name);
            }
            *selected = names(profiles).first().cloned();
        }
        SetProfileMode(index) => {
            let Some(profile) = selected.as_ref().and_then(|n| profiles.get_mut(n)) else {
                return Some(Err("Select a profile first".into()));
            };
            if *index > 2 {
                return Some(Err("Unknown launch type".into()));
            }
            if mode(profile) != *index {
                profile.shell = None;
                profile.command = (*index == 1).then(String::new);
                profile.url = (*index == 2).then(String::new);
            }
        }
        SetProfileText(field, value) => {
            let Some(name) = selected.clone().filter(|n| profiles.contains_key(n)) else {
                return Some(Err("Select a profile first".into()));
            };
            if *field == ProfileField::Name {
                let value = value.trim();
                if value.is_empty() {
                    return Some(Err("Profile name cannot be empty".into()));
                }
                if value != name && profiles.contains_key(value) {
                    return Some(Err("A profile already has that name".into()));
                }
                let profile = profiles.remove(&name).unwrap();
                profiles.insert(value.to_owned(), profile);
                *selected = Some(value.to_owned());
            } else {
                let profile = profiles.get_mut(&name).unwrap();
                let optional = (!value.is_empty()).then(|| value.clone());
                match field {
                    ProfileField::Program => match mode(profile) {
                        2 => profile.url = Some(value.clone()),
                        1 => profile.command = Some(value.clone()),
                        _ => profile.shell = optional,
                    },
                    ProfileField::Arguments => match parse_arguments(value) {
                        Ok(args) => profile.shell_args = args,
                        Err(e) => return Some(Err(e)),
                    },
                    ProfileField::Directory => profile.working_directory = optional,
                    ProfileField::Environment => profile.environment = optional,
                    ProfileField::ShellLabel => profile.shell_label = optional,
                    ProfileField::Icon => profile.icon = optional,
                    ProfileField::Color => {
                        let valid = value.is_empty()
                            || (value.starts_with('#')
                                && matches!(value.len(), 4 | 7)
                                && value[1..].bytes().all(|b| b.is_ascii_hexdigit()));
                        if !valid {
                            return Some(Err("Use a color such as #246 or #224466".into()));
                        }
                        profile.color = optional;
                    }
                    ProfileField::Name => unreachable!(),
                }
            }
        }
        _ => return None,
    }
    Some(Ok(()))
}

fn format_arguments(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_arguments(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut value = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quote == Some('"') && c == '\\' && matches!(chars.peek(), Some('"' | '\\')) {
            value.push(chars.next().unwrap());
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            started = true;
        } else if quote.is_none() && c.is_whitespace() {
            if started {
                args.push(std::mem::take(&mut value));
                started = false;
            }
        } else {
            value.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return Err("Close the quote in shell arguments".into());
    }
    if started {
        args.push(value);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_preserve_spaces_empty_values_and_windows_paths() {
        let args = vec![
            "-c".into(),
            "echo hello world".into(),
            String::new(),
            r"C:\Program Files\Tool".into(),
            "a\"b".into(),
        ];
        assert_eq!(parse_arguments(&format_arguments(&args)).unwrap(), args);
        assert!(parse_arguments("\"unfinished").is_err());
        assert_eq!(
            parse_arguments(r"-d Ubuntu --cd C:\work").unwrap(),
            vec!["-d", "Ubuntu", "--cd", r"C:\work"]
        );
    }
    #[test]
    fn rename_preserves_advanced_fields_and_rejects_collisions() {
        let mut profiles = HashMap::from([
            (
                "old".into(),
                ProfileConfig {
                    env: HashMap::from([("X".into(), "Y".into())]),
                    ..Default::default()
                },
            ),
            ("taken".into(), ProfileConfig::default()),
        ]);
        let mut selected = Some("old".into());
        assert!(
            apply_profile_command(
                &mut profiles,
                &mut selected,
                &SettingsCommand::SetProfileText(ProfileField::Name, "taken".into())
            )
            .unwrap()
            .is_err()
        );
        apply_profile_command(
            &mut profiles,
            &mut selected,
            &SettingsCommand::SetProfileText(ProfileField::Name, "new".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(profiles["new"].env["X"], "Y");
        assert!(!profiles.contains_key("old"));
        let before = profiles["new"].shell_args.clone();
        assert!(
            apply_profile_command(
                &mut profiles,
                &mut selected,
                &SettingsCommand::SetProfileText(ProfileField::Arguments, "'".into())
            )
            .unwrap()
            .is_err()
        );
        assert_eq!(profiles["new"].shell_args, before);
    }
    #[test]
    fn identity_labels_do_not_change_the_program_or_arguments() {
        let original = ProfileConfig {
            shell: Some("pwsh".into()),
            shell_args: vec!["-NoLogo".into()],
            ..Default::default()
        };
        let mut profiles = HashMap::from([("PowerShell on Ubuntu".into(), original)]);
        let mut selected = Some("PowerShell on Ubuntu".into());
        for (field, value) in [
            (ProfileField::Environment, "WSL Ubuntu"),
            (ProfileField::ShellLabel, "PowerShell"),
        ] {
            apply_profile_command(
                &mut profiles,
                &mut selected,
                &SettingsCommand::SetProfileText(field, value.into()),
            )
            .unwrap()
            .unwrap();
        }
        let profile = &profiles["PowerShell on Ubuntu"];
        assert_eq!(profile.shell.as_deref(), Some("pwsh"));
        assert_eq!(profile.shell_args, ["-NoLogo"]);
        assert_eq!(profile.environment.as_deref(), Some("WSL Ubuntu"));
        assert_eq!(profile.shell_label.as_deref(), Some("PowerShell"));
    }
}
