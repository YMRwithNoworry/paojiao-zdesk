use std::{ffi::OsStr, path::Path};

use anyhow::{bail, ensure, Context, Result};

use crate::{desktop, edition, quick_search::control::main_window_startup, storage};

// Setup reads these bits from the exit code. Errors must never look like "both off".
const STATUS_OK: i32 = 16;

fn choice(value: &OsStr) -> Result<Option<bool>> {
    match value.to_str() {
        Some("keep") => Ok(None),
        Some("on") => Ok(Some(true)),
        Some("off") => Ok(Some(false)),
        _ => bail!("Invalid sign-in option"),
    }
}

fn status(directory: &Path) -> Result<i32> {
    ensure!(directory.is_absolute(), "Expected an absolute install directory");
    let executable = directory.join("ZSDesk.exe");
    let profile = storage::profile_for_install_dir(directory);
    let main = !edition::DESKTOP_ONLY && main_window_startup::enabled_for(&profile, &executable)?;
    let fences = edition::DESKTOP &&
        desktop::startup_enabled_for(&profile.with_file_name("desktop-fences.json"), &executable)?;
    Ok(STATUS_OK + i32::from(main) + 2 * i32::from(fences))
}

fn apply(main: Option<bool>, fences: Option<bool>) -> Result<i32> {
    ensure!(main.is_none() || !edition::DESKTOP_ONLY, "This edition has no main window");
    ensure!(fences.is_none() || edition::DESKTOP, "This edition has no desktop fences");
    let profile = storage::config_path();
    let fence_profile = profile.with_file_name("desktop-fences.json");
    let mut previous_main = None;
    if let Some(enabled) = main {
        let previous = main_window_startup::enabled(&profile)?;
        if previous != enabled {
            main_window_startup::set_enabled(&profile, enabled)?;
            previous_main = Some(previous);
        }
    }
    if let Some(enabled) = fences {
        if let Err(error) = desktop::set_startup_enabled(&fence_profile, enabled) {
            if let Some(previous) = previous_main {
                main_window_startup::set_enabled(&profile, previous)
                    .with_context(|| format!("Desktop sign-in failed ({error:#}); could not restore window sign-in"))?;
            }
            return Err(error);
        }
    }
    Ok(0)
}

pub(crate) fn run() -> Option<Result<i32>> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next()?;
    match command.to_str()? {
        "--installer-startup-status" => Some((|| {
            let dir = args.next().context("Missing install directory")?;
            ensure!(args.next().is_none(), "Unexpected sign-in arguments");
            status(Path::new(&dir))
        })()),
        "--installer-startup-apply" => Some((|| {
            let main = args.next().context("Missing window sign-in option")?;
            let fences = args.next().context("Missing desktop sign-in option")?;
            ensure!(args.next().is_none(), "Unexpected sign-in arguments");
            apply(choice(&main)?, choice(&fences)?)
        })()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{PCWSTR, HSTRING};
    use windows::Win32::System::Registry::*;

    // This fixture replaces HKCU only in its dedicated test process. A portable
    // copy of the test executable also isolates the application's profile.
    #[test]
    #[ignore = "Run a dedicated test process named ZSDesk.exe beside a portable.flag"]
    fn sign_in_choices_round_trip() -> Result<()> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("No fixture directory")?;
        ensure!(exe.file_name() == Some(OsStr::new("ZSDesk.exe")) && dir.join("portable.flag").is_file());
        let key_name = HSTRING::from(format!("Software\\ZSDesk-SetupTest-{}", std::process::id()));
        let mut key = HKEY::default();
        let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
        unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, &key_name, None, PCWSTR::null(),
                REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, None, &mut key, Some(&mut disposition)).ok()?;
        }
        if disposition != REG_CREATED_NEW_KEY {
            unsafe { RegCloseKey(key).ok()?; }
            bail!("Fixture key already exists");
        }
        struct RegistryFixture(HKEY, HSTRING);
        impl Drop for RegistryFixture {
            fn drop(&mut self) {
                unsafe {
                    RegOverridePredefKey(HKEY_CURRENT_USER, None).ok().unwrap();
                    RegCloseKey(self.0).ok().unwrap();
                    RegDeleteTreeW(HKEY_CURRENT_USER, &self.1).ok().unwrap();
                }
            }
        }
        let _fixture = RegistryFixture(key, key_name);
        unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, Some(key)).ok()?; }
        assert_eq!(status(dir)?, STATUS_OK);
        let main = !edition::DESKTOP_ONLY;
        let fences = edition::DESKTOP;
        apply(main.then_some(true), fences.then_some(true))?;
        let both = STATUS_OK + i32::from(main) + 2 * i32::from(fences);
        assert_eq!(status(dir)?, both);
        apply(None, None)?;
        assert_eq!(status(dir)?, both);
        if main {
            apply(Some(false), None)?;
            assert_eq!(status(dir)?, STATUS_OK + 2 * i32::from(fences));
        } else {
            assert!(apply(Some(true), None).is_err());
        }
        if fences { apply(None, Some(false))?; } else { assert!(apply(None, Some(true)).is_err()); }
        assert_eq!(status(dir)?, STATUS_OK);
        if fences {
            use crate::quick_search::control::{key, quote_argument};
            use zsdesk_windows::desktop::{autostart_command, set_autostart};
            let profile = storage::config_path();
            let folder = profile.parent().context("Missing fixture profile")?;
            let old_name = edition::desktop_startup_value();
            let command = format!("{} --desktop --profile-dir {}", quote_argument(&exe.to_string_lossy()),
                                  quote_argument(&folder.to_string_lossy()));
            set_autostart(old_name, &command, true)?;
            assert_eq!(status(dir)?, STATUS_OK + 2);
            apply(None, None)?;
            assert_eq!(autostart_command(old_name)?.as_deref(), Some(command.as_str()));
            apply(None, Some(false))?;
            assert_eq!(status(dir)?, STATUS_OK);
            if main {
                let scoped = format!("{}.{}", old_name, &key(&folder.join("desktop-fences.json"))[..24]);
                set_autostart(&scoped, "another installation", true)?;
                assert!(apply(Some(true), Some(true)).is_err());
                assert_eq!(status(dir)?, STATUS_OK, "partial change left main window enabled");
                assert_eq!(autostart_command(&scoped)?.as_deref(), Some("another installation"));
            }
        }
        Ok(())
    }
}
