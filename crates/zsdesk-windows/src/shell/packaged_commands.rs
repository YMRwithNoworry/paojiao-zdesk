//! Public package-manifest discovery for packaged Explorer commands.
//!
//! This is a registration inventory, not an assertion that a command is visible.
//! The caller must evaluate IExplorerCommand state with the current selection and
//! site. Discovery performs filesystem/WinRT work and belongs on a worker thread.
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use windows::{
    core::{Error, Result, GUID, HRESULT, HSTRING},
    ApplicationModel::Package,
    Management::Deployment::PackageManager,
    Win32::{
        System::{
            Com::{CoCreateInstance, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER},
            WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED},
        },
        UI::Shell::IExplorerCommand,
    },
};

const FOUNDATION: &str = "http://schemas.microsoft.com/appx/manifest/foundation/windows10";
const LEGACY_FOUNDATION: &str = "http://schemas.microsoft.com/appx/2010/manifest";
const DESKTOP: [&str; 3] = [
    "http://schemas.microsoft.com/appx/manifest/desktop/windows10/4",
    "http://schemas.microsoft.com/appx/manifest/desktop/windows10/5",
    "http://schemas.microsoft.com/appx/manifest/desktop/windows10/10",
];
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_XML_NODES: u32 = 200_000;
const CACHE_TTL: Duration = Duration::from_secs(30);
const INCOMPLETE_CACHE_TTL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Registration {
    pub package_family: String,
    pub package_full_name: String,
    pub application_id: String,
    pub verb_id: String,
    pub item_type: String,
    pub clsid: GUID,
}

#[derive(Clone, Debug)]
pub(super) struct DiscoveryFailure {
    pub package_full_name: Option<String>,
    pub manifest: Option<PathBuf>,
    pub stage: &'static str,
    pub hresult: Option<i32>,
    pub os_error: Option<i32>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub(super) struct ExcludedPackage {
    pub package_full_name: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Discovery {
    pub registrations: Vec<Registration>,
    pub failures: Vec<DiscoveryFailure>,
    pub excluded_packages: Vec<ExcludedPackage>,
    /// Unrecognized selectors are retained, rather than silently discarded.
    pub unknown_item_types: Vec<String>,
    pub enumeration_complete: bool,
    pub packages_seen: usize,
    pub manifests_read: usize,
    pub manifest_cache_hits: usize,
    pub cache_hit: bool,
    pub cache_age_ms: u64,
}

impl Discovery {
    /// Completeness of registration discovery, not of menu applicability/state.
    pub fn complete(&self) -> bool {
        self.enumeration_complete && self.failures.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ItemKind<'a> {
    /// Extension may include its leading period; None means no extension.
    File {
        extension: Option<&'a str>,
    },
    Directory,
    Drive,
    /// A provider item cannot be classified from a display path alone.
    Virtual,
}

#[derive(Clone, Debug)]
pub(super) enum Scene<'a> {
    Selection(&'a [ItemKind<'a>]),
    DirectoryBackground,
    DesktopBackground,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectorMatch {
    Matches,
    DoesNotMatch,
    /// The caller must not interpret this as either visible or safely absent.
    Unknown(&'static str),
}

fn known_selector(selector: &str) -> bool {
    [
        "*",
        "Directory",
        "Directory\\Background",
        "Drive",
        "DesktopBackground",
    ]
    .iter()
    .any(|known| selector.eq_ignore_ascii_case(known))
        || selector.strip_prefix('.').is_some_and(|extension| {
            !extension.is_empty()
                && !extension.contains(['.', '\\', '/', '*', '?'])
                && !extension.chars().any(char::is_whitespace)
        })
}

/// Only claims matches established by the public ItemType selector contract.
/// Mixed selections and virtual namespaces remain explicit unknowns where the
/// declaration alone does not establish the Shell's aggregation policy.
pub(super) fn matches_scene(registration: &Registration, scene: &Scene<'_>) -> SelectorMatch {
    use SelectorMatch::*;
    let selector = registration.item_type.as_str();
    if !known_selector(selector) {
        return Unknown("unsupported ItemType selector");
    }
    match scene {
        Scene::DirectoryBackground => {
            return if selector.eq_ignore_ascii_case("Directory\\Background") {
                Matches
            } else {
                DoesNotMatch
            };
        }
        Scene::DesktopBackground => {
            return if selector.eq_ignore_ascii_case("DesktopBackground") {
                Matches
            } else {
                DoesNotMatch
            };
        }
        Scene::Selection(items) => {
            if selector.eq_ignore_ascii_case("Directory\\Background")
                || selector.eq_ignore_ascii_case("DesktopBackground")
            {
                return DoesNotMatch;
            }
            if items.is_empty() {
                return Unknown("empty item selection is not a background scene");
            }
            let mut yes = 0;
            let mut no = 0;
            for item in *items {
                let matched = match item {
                    ItemKind::Virtual => return Unknown("virtual provider item"),
                    ItemKind::Directory => selector.eq_ignore_ascii_case("Directory"),
                    ItemKind::Drive => selector.eq_ignore_ascii_case("Drive"),
                    ItemKind::File { extension } => {
                        if selector == "*" {
                            true
                        } else if let Some(expected) = selector.strip_prefix('.') {
                            match extension {
                                Some(actual) => {
                                    let actual = actual.strip_prefix('.').unwrap_or(actual);
                                    if !expected.is_ascii() || !actual.is_ascii() {
                                        return Unknown(
                                            "non-ASCII extension comparison requires Shell state",
                                        );
                                    }
                                    expected.eq_ignore_ascii_case(actual)
                                }
                                None => false,
                            }
                        } else {
                            false
                        }
                    }
                };
                if matched {
                    yes += 1;
                } else {
                    no += 1;
                }
            }
            if no == 0 {
                Matches
            } else if yes == 0 {
                DoesNotMatch
            } else {
                Unknown("selector covers only part of a mixed selection")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestStamp {
    bytes: u64,
    modified: SystemTime,
}
fn stamp(path: &Path) -> std::io::Result<ManifestStamp> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "manifest is not a file",
        ));
    }
    Ok(ManifestStamp {
        bytes: metadata.len(),
        modified: metadata.modified()?,
    })
}

#[derive(Clone)]
struct CachedManifest {
    path: PathBuf,
    stamp: ManifestStamp,
    registrations: Vec<Registration>,
}
#[derive(Default)]
struct Cache {
    checked: Option<Instant>,
    discovery: Option<Discovery>,
    manifests: HashMap<String, CachedManifest>,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Invalidate after a known package-registration change or activation failure.
/// Parsed manifests still reuse their identities/metadata on the next scan.
pub(super) fn invalidate_cache() {
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.checked = None;
}

struct Apartment;
impl Apartment {
    fn enter() -> Result<Self> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self),
            Err(error) if error.code() == HRESULT(0x80010106u32 as i32) => {
                // Reuse an existing STA; never change the caller's apartment.
                unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }?;
                Ok(Self)
            }
            Err(error) => Err(error),
        }
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

fn failure(
    package: Option<&str>,
    manifest: Option<&Path>,
    stage: &'static str,
    message: impl ToString,
) -> DiscoveryFailure {
    DiscoveryFailure {
        package_full_name: package.map(str::to_owned),
        manifest: manifest.map(Path::to_path_buf),
        stage,
        hresult: None,
        os_error: None,
        message: message.to_string().chars().take(1024).collect(),
    }
}
fn com_failure(package: Option<&str>, stage: &'static str, error: Error) -> DiscoveryFailure {
    let mut result = failure(package, None, stage, error.message());
    result.hresult = Some(error.code().0);
    result
}
fn io_failure(
    package: &str,
    path: &Path,
    stage: &'static str,
    error: std::io::Error,
) -> DiscoveryFailure {
    let mut result = failure(Some(package), Some(path), stage, &error);
    result.os_error = error.raw_os_error();
    result
}

fn decode_manifest(bytes: &[u8]) -> std::result::Result<String, String> {
    let utf16 = if bytes.starts_with(&[0xff, 0xfe]) {
        Some((true, &bytes[2..]))
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some((false, &bytes[2..]))
    } else if bytes.starts_with(&[b'<', 0, b'?', 0]) {
        Some((true, bytes))
    } else if bytes.starts_with(&[0, b'<', 0, b'?']) {
        Some((false, bytes))
    } else {
        None
    };
    if let Some((little, bytes)) = utf16 {
        if bytes.len() % 2 != 0 {
            return Err("odd UTF-16 manifest length".into());
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        return String::from_utf16(&units).map_err(|error| error.to_string());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())
}

fn desktop_node(node: roxmltree::Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node
            .tag_name()
            .namespace()
            .is_some_and(|namespace| DESKTOP.contains(&namespace))
}
fn required(node: roxmltree::Node<'_, '_>, name: &str) -> std::result::Result<String, String> {
    node.attribute(name)
        .filter(|value| !value.is_empty() && !value.trim().is_empty() && value.len() <= 4096)
        .map(str::to_owned)
        .ok_or_else(|| format!("{} has no valid {name}", node.tag_name().name()))
}

fn parse_manifest(text: &str, family: &str, full_name: &str) -> (Vec<Registration>, Vec<String>) {
    let document = match roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
        },
    ) {
        Ok(document) => document,
        Err(error) => return (Vec::new(), vec![format!("manifest XML: {error}")]),
    };
    let root = document.root_element();
    let foundation = root.tag_name().namespace().unwrap_or_default();
    if root.tag_name().name() != "Package" || ![FOUNDATION, LEGACY_FOUNDATION].contains(&foundation)
    {
        return (
            Vec::new(),
            vec!["unsupported package manifest root namespace".into()],
        );
    }
    let mut registrations = Vec::new();
    let mut failures = Vec::new();
    for applications in root
        .children()
        .filter(|node| node.has_tag_name((foundation, "Applications")))
    {
        for application in applications
            .children()
            .filter(|node| node.has_tag_name((foundation, "Application")))
        {
            for extensions in application
                .children()
                .filter(|node| node.has_tag_name((foundation, "Extensions")))
            {
                for extension in extensions.children().filter(|node| {
                    node.is_element()
                        && node.attribute("Category") == Some("windows.fileExplorerContextMenus")
                }) {
                    if !desktop_node(extension, "Extension") {
                        failures.push(
                            "unsupported fileExplorerContextMenus extension namespace".into(),
                        );
                        continue;
                    }
                    let application_id = match required(application, "Id") {
                        Ok(value) => value,
                        Err(error) => {
                            failures.push(error);
                            continue;
                        }
                    };
                    for menus in extension.children().filter(|node| node.is_element()) {
                        if !desktop_node(menus, "FileExplorerContextMenus") {
                            failures.push("unsupported fileExplorerContextMenus container".into());
                            continue;
                        }
                        for item in menus.children().filter(|node| node.is_element()) {
                            if !desktop_node(item, "ItemType") {
                                failures.push("unsupported context-menu ItemType element".into());
                                continue;
                            }
                            let item_type = match required(item, "Type") {
                                Ok(value) => value,
                                Err(error) => {
                                    failures.push(error);
                                    continue;
                                }
                            };
                            for verb in item.children().filter(|node| node.is_element()) {
                                let parsed = (|| -> std::result::Result<Registration, String> {
                                    if !desktop_node(verb, "Verb")
                                        || verb.tag_name().namespace()
                                            != item.tag_name().namespace()
                                    {
                                        return Err(
                                            "unsupported context-menu Verb element/namespace"
                                                .into(),
                                        );
                                    }
                                    let verb_id = required(verb, "Id")?;
                                    let raw_clsid = required(verb, "Clsid")?;
                                    let raw_clsid = raw_clsid.trim();
                                    let raw_clsid = raw_clsid
                                        .strip_prefix('{')
                                        .and_then(|value| value.strip_suffix('}'))
                                        .unwrap_or(raw_clsid);
                                    let clsid = GUID::try_from(raw_clsid)
                                        .map_err(|_| "invalid context-menu CLSID".to_string())?;
                                    if clsid == GUID::zeroed() {
                                        return Err("null context-menu CLSID".into());
                                    }
                                    Ok(Registration {
                                        package_family: family.into(),
                                        package_full_name: full_name.into(),
                                        application_id: application_id.clone(),
                                        verb_id,
                                        item_type: item_type.clone(),
                                        clsid,
                                    })
                                })();
                                match parsed {
                                    Ok(registration) => registrations.push(registration),
                                    Err(error) => failures.push(error),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    (registrations, failures)
}

fn package_metadata(package: &Package) -> Result<(String, String)> {
    let id = package.Id()?;
    Ok((
        id.FullName()?.to_string_lossy(),
        id.FamilyName()?.to_string_lossy(),
    ))
}

/// Enumerates packages for the calling user only. Global enumeration errors are
/// errors; partial enumeration and per-manifest failures stay in Discovery.
pub(super) fn discover() -> Result<Discovery> {
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let (Some(checked), Some(result)) = (cache.checked, cache.discovery.as_ref()) {
        let age = checked.elapsed();
        let ttl = if result.complete() {
            CACHE_TTL
        } else {
            INCOMPLETE_CACHE_TTL
        };
        if age < ttl {
            let mut result = result.clone();
            result.cache_hit = true;
            result.cache_age_ms = age.as_millis().min(u64::MAX as u128) as u64;
            return Ok(result);
        }
    }
    let _apartment = Apartment::enter()?;
    let manager = PackageManager::new()?;
    let packages = manager.FindPackagesByUserSecurityId(&HSTRING::new())?;
    let iterator = packages.First()?;
    let mut result = Discovery::default();
    let mut manifests = HashMap::new();
    loop {
        match iterator.HasCurrent() {
            Ok(true) => (),
            Ok(false) => {
                result.enumeration_complete = true;
                break;
            }
            Err(error) => {
                result
                    .failures
                    .push(com_failure(None, "enumerate-has-current", error));
                break;
            }
        }
        let package = match iterator.Current() {
            Ok(package) => package,
            Err(error) => {
                result
                    .failures
                    .push(com_failure(None, "enumerate-current", error));
                break;
            }
        };
        result.packages_seen += 1;
        match package_metadata(&package) {
            Err(error) => result
                .failures
                .push(com_failure(None, "package-identity", error)),
            Ok((full_name, family)) => {
                let eligible = (|| -> Result<Option<&'static str>> {
                    if package.IsFramework()? {
                        return Ok(Some("framework"));
                    }
                    if package.IsResourcePackage()? {
                        return Ok(Some("resource-package"));
                    }
                    if package.IsBundle()? {
                        return Ok(Some("bundle-container"));
                    }
                    if !package.Status()?.VerifyIsOK()? {
                        return Ok(Some("package-status-not-usable"));
                    }
                    Ok(None)
                })();
                match eligible {
                    Err(error) => {
                        result
                            .failures
                            .push(com_failure(Some(&full_name), "package-status", error))
                    }
                    Ok(Some(reason)) => result.excluded_packages.push(ExcludedPackage {
                        package_full_name: full_name,
                        reason,
                    }),
                    Ok(None) => {
                        let location = package.InstalledLocation().and_then(|folder| folder.Path());
                        match location {
                            Err(error) => result.failures.push(com_failure(
                                Some(&full_name),
                                "manifest-location",
                                error,
                            )),
                            Ok(location) => {
                                let root = PathBuf::from(std::ffi::OsString::from_wide(&location));
                                let path = root.join("AppxManifest.xml");
                                if !root.is_absolute() {
                                    result.failures.push(failure(
                                        Some(&full_name),
                                        Some(&path),
                                        "manifest-location",
                                        "package install location is not absolute",
                                    ));
                                } else {
                                    inspect_manifest(
                                        &mut result,
                                        &cache.manifests,
                                        &mut manifests,
                                        &family,
                                        &full_name,
                                        &path,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        match iterator.MoveNext() {
            Ok(true) => (),
            Ok(false) => {
                result.enumeration_complete = true;
                break;
            }
            Err(error) => {
                result
                    .failures
                    .push(com_failure(None, "enumerate-move-next", error));
                break;
            }
        }
    }
    result.registrations.sort_by(|a, b| {
        (
            &a.package_full_name,
            &a.application_id,
            &a.verb_id,
            &a.item_type,
            u128::from(a.clsid),
        )
            .cmp(&(
                &b.package_full_name,
                &b.application_id,
                &b.verb_id,
                &b.item_type,
                u128::from(b.clsid),
            ))
    });
    result.registrations.dedup();
    result.unknown_item_types = result
        .registrations
        .iter()
        .filter(|registration| !known_selector(&registration.item_type))
        .map(|registration| registration.item_type.clone())
        .collect();
    result.unknown_item_types.sort();
    result.unknown_item_types.dedup();
    cache.checked = Some(Instant::now());
    cache.manifests = manifests;
    cache.discovery = Some(result.clone());
    Ok(result)
}

fn inspect_manifest(
    result: &mut Discovery,
    previous: &HashMap<String, CachedManifest>,
    next: &mut HashMap<String, CachedManifest>,
    family: &str,
    full_name: &str,
    path: &Path,
) {
    let before = match stamp(path) {
        Ok(value) => value,
        Err(error) => {
            result
                .failures
                .push(io_failure(full_name, path, "manifest-metadata", error));
            return;
        }
    };
    if let Some(cached) = previous
        .get(full_name)
        .filter(|cached| cached.path == path && cached.stamp == before)
    {
        result.manifest_cache_hits += 1;
        result.registrations.extend(cached.registrations.clone());
        next.insert(full_name.into(), cached.clone());
        return;
    }
    if before.bytes > MAX_MANIFEST_BYTES {
        result.failures.push(failure(
            Some(full_name),
            Some(path),
            "manifest-size",
            "manifest exceeds the 8 MiB parser bound",
        ));
        return;
    }
    let mut bytes = Vec::new();
    let read =
        File::open(path).and_then(|file| file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes));
    if let Err(error) = read {
        result
            .failures
            .push(io_failure(full_name, path, "manifest-read", error));
        return;
    }
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        result.failures.push(failure(
            Some(full_name),
            Some(path),
            "manifest-size",
            "manifest grew beyond the parser bound",
        ));
        return;
    }
    let after = match stamp(path) {
        Ok(value) => value,
        Err(error) => {
            result.failures.push(io_failure(
                full_name,
                path,
                "manifest-metadata-after-read",
                error,
            ));
            return;
        }
    };
    if before != after || after.bytes != bytes.len() as u64 {
        result.failures.push(failure(
            Some(full_name),
            Some(path),
            "manifest-read",
            "manifest changed during read",
        ));
        return;
    }
    result.manifests_read += 1;
    let text = match decode_manifest(&bytes) {
        Ok(text) => text,
        Err(error) => {
            result.failures.push(failure(
                Some(full_name),
                Some(path),
                "manifest-encoding",
                error,
            ));
            return;
        }
    };
    let (registrations, errors) = parse_manifest(&text, family, full_name);
    result.registrations.extend(registrations.clone());
    if errors.is_empty() {
        next.insert(
            full_name.into(),
            CachedManifest {
                path: path.into(),
                stamp: after,
                registrations,
            },
        );
    }
    for error in errors {
        result.failures.push(failure(
            Some(full_name),
            Some(path),
            "manifest-parse",
            error,
        ));
    }
}

/// Uses the current user's registered COM class, including its normal packaged
/// surrogate activation. Requires an initialized COM apartment owned by caller.
/// This does not pin a package version or guarantee that a CLSID is unambiguous
/// across registrations. It never loads the handler DLL directly or invokes it.
pub(super) fn activate(registration: &Registration) -> Result<IExplorerCommand> {
    if registration.clsid == GUID::zeroed() {
        return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));
    }
    unsafe {
        CoCreateInstance(
            &registration.clsid,
            None,
            CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
        )
    }
}

/// Some providers expose optional initialization interfaces but leave their
/// methods unimplemented. Preserve all actual initialization failures.
pub(super) fn optional_initialization(result: Result<()>) -> Result<()> {
    match result {
        Err(error) if error.code() == HRESULT(0x80004001u32 as i32) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unused_optional_initializers_do_not_hide_real_failures() {
        assert!(optional_initialization(Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))).is_ok());
        assert!(optional_initialization(Err(Error::from_hresult(HRESULT(0x80070005u32 as i32)))).is_err());
        assert!(optional_initialization(Err(Error::from_hresult(HRESULT(0x8000000au32 as i32)))).is_err());
    }
    const CLSID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    fn manifest(body: &str) -> String {
        format!(
            r#"<Package xmlns="{FOUNDATION}" xmlns:d4="{}" xmlns:d5="{}" xmlns:d10="{}"><Applications>{body}</Applications></Package>"#,
            DESKTOP[0], DESKTOP[1], DESKTOP[2]
        )
    }
    fn registration(item_type: &str) -> Registration {
        Registration {
            package_family: "family".into(),
            package_full_name: "package_1".into(),
            application_id: "App".into(),
            verb_id: "Verb".into(),
            item_type: item_type.into(),
            clsid: GUID::try_from(CLSID).unwrap(),
        }
    }
    #[test]
    fn manifests_preserve_application_context_and_all_supported_namespaces() {
        let text = manifest(&format!(
            r#"<Application Id="One"><Extensions><d4:Extension Category="windows.fileExplorerContextMenus"><d4:FileExplorerContextMenus><d4:ItemType Type="*"><d4:Verb Id="Any" Clsid="{CLSID}"/></d4:ItemType><d5:ItemType Type="Directory\Background"><d5:Verb Id="Here" Clsid="{{{CLSID}}}"/></d5:ItemType><d10:ItemType Type="DesktopBackground"><d10:Verb Id="Desktop" Clsid="{CLSID}"/></d10:ItemType></d4:FileExplorerContextMenus></d4:Extension></Extensions></Application><Application Id="Two"><Extensions><d4:Extension Category="windows.fileExplorerContextMenus"><d4:FileExplorerContextMenus><d10:ItemType Type="Drive"><d10:Verb Id="Drive" Clsid="{CLSID}"/></d10:ItemType></d4:FileExplorerContextMenus></d4:Extension></Extensions></Application>"#
        ));
        let (items, errors) = parse_manifest(&text, "family", "full");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(items.len(), 4);
        assert_eq!(items[1].item_type, r"Directory\Background");
        assert_eq!(items[3].application_id, "Two");
        assert!(items
            .iter()
            .all(|item| item.package_full_name == "full" && item.package_family == "family"));
    }
    #[test]
    fn parsing_keeps_valid_siblings_and_reports_malformed_or_foreign_entries() {
        let text = manifest(&format!(
            r#"<Application Id="App"><Extensions><d4:Extension Category="windows.fileExplorerContextMenus"><d4:FileExplorerContextMenus><d5:ItemType Type="FutureContext"><d5:Verb Id="Good" Clsid="{CLSID}"/><d5:Verb Id="Broken" Clsid="invalid"/><Verb Id="Foreign" Clsid="{CLSID}"/></d5:ItemType></d4:FileExplorerContextMenus></d4:Extension></Extensions></Application>"#
        ));
        let (items, errors) = parse_manifest(&text, "family", "full");
        assert_eq!(items.len(), 1);
        assert_eq!(errors.len(), 2);
        assert_eq!(items[0].item_type, "FutureContext");
        assert!(matches!(
            matches_scene(&items[0], &Scene::DesktopBackground),
            SelectorMatch::Unknown(_)
        ));
        assert!(!parse_manifest("<broken", "family", "full").1.is_empty());
        assert!(!parse_manifest(
            r#"<!DOCTYPE Package [<!ENTITY x "value">]><Package/>"#,
            "family",
            "full"
        )
        .1
        .is_empty());
    }
    #[test]
    fn legacy_packages_without_context_registrations_are_known_empty() {
        let text = format!(
            r#"<Package xmlns="{LEGACY_FOUNDATION}"><Applications><Application Id="Legacy"/></Applications></Package>"#
        );
        let (registrations, failures) = parse_manifest(&text, "family", "full");
        assert!(registrations.is_empty() && failures.is_empty());
        let (_, failures) = parse_manifest(
            r#"<Package xmlns="urn:unknown:manifest"><Applications/></Package>"#,
            "family",
            "full",
        );
        assert!(!failures.is_empty());
    }
    #[test]
    fn selectors_distinguish_items_backgrounds_drives_and_unknown_aggregates() {
        use SelectorMatch::*;
        let text = [ItemKind::File {
            extension: Some("TXT"),
        }];
        let folder = [ItemKind::Directory];
        let drive = [ItemKind::Drive];
        assert_eq!(
            matches_scene(&registration(".txt"), &Scene::Selection(&text)),
            Matches
        );
        assert_eq!(
            matches_scene(&registration("*"), &Scene::Selection(&folder)),
            DoesNotMatch
        );
        assert_eq!(
            matches_scene(&registration("Directory"), &Scene::Selection(&folder)),
            Matches
        );
        assert_eq!(
            matches_scene(&registration("Drive"), &Scene::Selection(&drive)),
            Matches
        );
        assert_eq!(
            matches_scene(
                &registration(r"Directory\Background"),
                &Scene::DirectoryBackground
            ),
            Matches
        );
        assert_eq!(
            matches_scene(
                &registration(r"Directory\Background"),
                &Scene::DesktopBackground
            ),
            DoesNotMatch
        );
        assert_eq!(
            matches_scene(
                &registration("DesktopBackground"),
                &Scene::DesktopBackground
            ),
            Matches
        );
        let mixed = [
            ItemKind::File {
                extension: Some(".txt"),
            },
            ItemKind::File {
                extension: Some(".png"),
            },
        ];
        assert!(matches!(
            matches_scene(&registration(".txt"), &Scene::Selection(&mixed)),
            Unknown(_)
        ));
        assert!(matches!(
            matches_scene(&registration("*"), &Scene::Selection(&[ItemKind::Virtual])),
            Unknown(_)
        ));
        assert!(matches!(
            matches_scene(&registration("*"), &Scene::Selection(&[])),
            Unknown(_)
        ));
    }
    #[test]
    fn manifest_decoding_is_lossless_for_utf8_and_utf16_and_rejects_damage() {
        let text = "<?xml version=\"1.0\"?><Package>中文</Package>";
        assert_eq!(decode_manifest(text.as_bytes()).unwrap(), text);
        for little in [true, false] {
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in text.encode_utf16() {
                bytes.extend(if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            assert_eq!(decode_manifest(&bytes).unwrap(), text);
        }
        assert!(decode_manifest(&[0xff, 0xfe, 1]).is_err());
        assert!(decode_manifest(&[0xff]).is_err());
    }
}
