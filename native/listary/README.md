# ZSdesk integration for Listary

The plugin implements the official [Listary FileAppPlugin API](https://github.com/listary/Listary.FileAppPlugin).
The DLL plugin targets Listary 6 and 7.0. Listary 7.1 uses the folder-opening and result-action configuration below;
[it no longer loads user-installed integration DLLs](https://www.listary.com/v7).
It exposes the active ZSdesk folder to Quick Switch and Currently Opened Folders.
It has no background process, filesystem scan, injected code or Explorer identity emulation.

Build the `net48` project with the .NET SDK and pass `-p:ZSDeskBuildRoot=<absolute external cache directory>`.
Intermediate and output files are separated by project beneath that directory.
The runtime dependency is the pinned `Listary.FileAppPlugin` 1.0.0 NuGet package.
Copy the completed plugin directory into `FileAppPlugins/ZSDesk` under an existing Listary installation,
restart Listary, and enable ZSdesk in **Options → Integration**.
Listary manages switching the standard Open/Save dialog; ZSdesk never writes into another application's dialog.

Only a completed filesystem navigation is shared. Searches, virtual Shell locations, a newly selected tab
whose navigation has not completed, and a closed file window return an empty path.
The read-only endpoint is the hidden `ZSDesk.Listary.Folder.v1` child of `ZSDesk.Native.Explorer`.
The endpoint stores its UTF-16 caption through `DefWindowProc`. The plugin validates its class,
parent and process, then reads this system-maintained caption with cross-process `GetWindowTextW`.
It does not send synchronous messages to ZSdesk's UI thread or request elevation.
The active directory is held in the window, with no persistent path cache.

The `tests/Listary.ZSDesk.Probe.csproj` executable verifies the official interfaces against a running
ZSdesk test window through a cross-process caption read. Its arguments are the decimal
window handle and exact expected folder. The native regression pauses the target UI message pump
and requires the read to complete with the correct path within 1200 ms. This verifies the plugin contract; it does not replace
an end-to-end test with an installed Listary application and a standard Open/Save dialog.

## Listary 7.1: open folders and locate search results

`Prepare-Listary7.ps1` creates a merged copy of an existing `Preferences.json`:

- Folder results open in ZSDesk using `Settings["OpenFolder.Path"]` and `Settings["OpenFolder.Param"]`.
- The result Actions menu gains **在 ZSDesk 中定位文件** (`zsdesk`), which passes the selected file to `ZSDesk.exe --select`.
- Other settings, existing actions, custom fields and existing hotkeys remain in the copy.
- The original preferences file is never overwritten, and an existing output file is refused.

Use a ZSDesk edition with the file-manager window; the standalone desktop-box edition is not a folder opener.
Run from this directory, substituting the installed ZSDesk executable path:

```powershell
.\Prepare-Listary7.ps1 `
  -ZSDeskPath 'C:\Program Files\ZSDesk\ZSDesk.exe' `
  -PreferencesPath "$env:APPDATA\Listary\UserProfile\Settings\Preferences.json" `
  -OutputPath "$env:USERPROFILE\Desktop\Preferences.ZSDesk.json"
```

Exit Listary before replacing its preferences with the prepared copy; retain the original preferences for restoration.
After restarting Listary, folder results open in ZSDesk. For a file result, open the Actions menu and choose **在 ZSDesk 中定位文件**.
The folder parameter template is `"%1\."`; the file action uses `--select "{action_path}"`.

This configuration supports Listary-to-ZSDesk folder opening and file selection. It does not enable Listary 7.1's
**Quick Switch** from a ZSDesk folder into another application's Open/Save dialog, or its type-to-search bar inside ZSDesk.
Those features require Listary to recognize ZSDesk as an integrated file manager. The old DLL is not loaded by 7.1;
this adapter does not impersonate another application's window class or install a global hook.

### HTTP API and Quick Switch

Listary 7.1.0.47's built-in **HTTP API Documentation** describes `POST /api/v1/search` for querying its file index.
The optional service listens only on `127.0.0.1`; its port is configured in Listary. Search requests accept
`query`, pagination and file filters, and return matching file paths. The documented API has no operation
for registering a file manager, publishing its active folder, or switching an Open/Save dialog.

Enabling that search API does not enable ZSDesk Quick Switch integration. The folder-opening and file-selection
configuration above remains available independently. A Listary 7.1 Quick Switch provider needs a supported
file-manager integration contract; the [7.1 release notes](https://www.listary.com/v7) describe a future
integration system separately from the HTTP search API.

Configuration references: [General settings](https://help.listary.com/options-general),
[Commands](https://help.listary.com/options-commands), [7.1 release notes](https://www.listary.com/v7).

`tests/Test-Listary7Settings.ps1` validates preservation, idempotence, action merging and refusal to overwrite the original.
Pass an existing ZSDesk executable and an empty external `-WorkDir` for the test artifacts.
