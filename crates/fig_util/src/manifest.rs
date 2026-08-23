use std::fmt::Display;
use std::str::FromStr;
use std::sync::OnceLock;

use cfg_if::cfg_if;
use fig_os_shim::{EnvProvider, FsProvider, PlatformProvider};
use serde::{Deserialize, Deserializer, Serialize};
use strum::{Display, EnumString};
use tracing::debug;

use crate::Error;
use crate::build::TARGET_TRIPLE;
use crate::consts::build::VARIANT;
use crate::directories::bundle_metadata_path;

#[derive(Deserialize)]
pub struct Manifest {
    #[serde(deserialize_with = "deser_enum_other")]
    pub managed_by: ManagedBy,
    #[serde(deserialize_with = "deser_enum_other")]
    pub target_triple: TargetTriple,
    #[serde(deserialize_with = "deser_enum_other")]
    pub variant: Variant,
    #[serde(deserialize_with = "deser_enum_other")]
    pub default_channel: Channel,
    pub packaged_at: String,
    pub packaged_by: String,
}

#[derive(EnumString, Display, Deserialize, Serialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum ManagedBy {
    None,
    #[strum(default)]
    Other(String),
}

/// The target triplet this build is for. Includes the Apple universal
/// `universal-apple-darwin` name the macOS dist profile still emits.
#[derive(Deserialize, Serialize, PartialEq, Eq, EnumString, Debug, Clone, Display)]
pub enum TargetTriple {
    #[serde(rename = "universal-apple-darwin")]
    #[strum(serialize = "universal-apple-darwin")]
    UniversalAppleDarwin,
    #[serde(rename = "x86_64-unknown-linux-gnu")]
    #[strum(serialize = "x86_64-unknown-linux-gnu")]
    X86_64UnknownLinuxGnu,
    #[serde(rename = "x86_64-unknown-linux-musl")]
    #[strum(serialize = "x86_64-unknown-linux-musl")]
    X86_64UnknownLinuxMusl,
    #[serde(rename = "aarch64-unknown-linux-gnu")]
    #[strum(serialize = "aarch64-unknown-linux-gnu")]
    AArch64UnknownLinuxGnu,
    #[serde(rename = "aarch64-unknown-linux-musl")]
    #[strum(serialize = "aarch64-unknown-linux-musl")]
    AArch64UnknownLinuxMusl,
    #[serde(rename = "x86_64-pc-windows-msvc")]
    #[strum(serialize = "x86_64-pc-windows-msvc")]
    X86_64PcWindowsMsvc,
    #[serde(rename = "i686-pc-windows-msvc")]
    #[strum(serialize = "i686-pc-windows-msvc")]
    I686PcWindowsMsvc,
    #[serde(rename = "aarch64-pc-windows-msvc")]
    #[strum(serialize = "aarch64-pc-windows-msvc")]
    AArch64PcWindowsMsvc,
    #[strum(default)]
    Other(String),
}

impl TargetTriple {
    const fn from_system() -> Self {
        cfg_if! {
            if #[cfg(target_os = "macos")] {
                TargetTriple::UniversalAppleDarwin
            } else if #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))] {
                TargetTriple::X86_64UnknownLinuxGnu
            } else if #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "aarch64"))] {
                TargetTriple::AArch64UnknownLinuxGnu
            } else if #[cfg(all(target_os = "linux", target_env = "musl", target_arch = "x86_64"))] {
                TargetTriple::X86_64UnknownLinuxMusl
            } else if #[cfg(all(target_os = "linux", target_env = "musl", target_arch = "aarch64"))] {
                TargetTriple::AArch64UnknownLinuxMusl
            } else if #[cfg(all(target_os = "windows", target_arch = "x86_64"))] {
                TargetTriple::X86_64PcWindowsMsvc
            } else if #[cfg(all(target_os = "windows", target_arch = "x86"))] {
                TargetTriple::I686PcWindowsMsvc
            } else if #[cfg(all(target_os = "windows", target_arch = "aarch64"))] {
                TargetTriple::AArch64PcWindowsMsvc
            } else {
                compile_error!("unknown target")
            }
        }
    }
}

#[derive(EnumString, Display, Deserialize, Serialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum Variant {
    Full,
    #[serde(alias = "headless")]
    #[strum(to_string = "minimal", serialize = "headless")]
    Minimal,
    #[strum(default)]
    Other(String),
}

#[derive(EnumString, Display, Deserialize, Serialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum Os {
    Macos,
    Linux,
    Windows,
    #[strum(default)]
    Other(String),
}

impl Os {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Os::Macos,
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            other => Os::Other(other.to_string()),
        }
    }

    pub fn is_current_os(&self) -> bool {
        self == &Os::current()
    }
}

#[derive(EnumString, Display, Deserialize, Serialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum FileType {
    Dmg,
    TarGz,
    TarXz,
    TarZst,
    Zip,
    AppImage,
    Deb,
    #[strum(default)]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, EnumString, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum Channel {
    Stable,
    Beta,
    Qa,
    Nightly,
}

impl Channel {
    pub fn all() -> &'static [Self] {
        &[Channel::Stable, Channel::Beta, Channel::Qa, Channel::Nightly]
    }

    pub fn id(&self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Qa => "qa",
            Channel::Nightly => "nightly",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Channel::Stable => "Stable",
            Channel::Beta => "Beta",
            Channel::Qa => "QA",
            Channel::Nightly => "Nightly",
        }
    }
}

impl Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            f.write_str(self.name())
        } else {
            f.write_str(self.id())
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleMetadata {
    pub packaged_as: FileType,
}

fn deser_enum_other<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    match T::from_str(<&str as Deserialize<'de>>::deserialize(deserializer)?) {
        Ok(s) => Ok(s),
        Err(err) => Err(serde::de::Error::custom(err)),
    }
}

/// Returns the manifest, reading and parsing it if necessary
pub fn manifest() -> &'static Manifest {
    static CACHED: OnceLock<Manifest> = OnceLock::new();
    CACHED.get_or_init(|| Manifest {
        managed_by: ManagedBy::None,
        target_triple: match TARGET_TRIPLE {
            Some(target) => TargetTriple::from_str(target).unwrap_or_else(|_| TargetTriple::Other(target.to_string())),
            _ => TargetTriple::from_system(),
        },
        variant: match VARIANT.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("minimal") => Variant::Minimal,
            _ => Variant::Full,
        },
        default_channel: Channel::Stable,
        packaged_at: "unknown".into(),
        packaged_by: "unknown".into(),
    })
}

/// Returns the [BundleMetadata] distributed with the app. This should only be called from the
/// desktop binary since in some bundles (notably AppImage), only the desktop app can access it.
///
/// Currently, only Linux desktop bundles have a bundle metadata.
pub async fn bundle_metadata<Ctx: FsProvider + EnvProvider + PlatformProvider>(
    ctx: &Ctx,
) -> Result<Option<BundleMetadata>, Error> {
    match bundle_metadata_json(ctx).await? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(None),
    }
}

/// Returns the [BundleMetadata] without serializing. See [bundle_metadata].
pub async fn bundle_metadata_json<Ctx: FsProvider + EnvProvider + PlatformProvider>(
    ctx: &Ctx,
) -> Result<Option<String>, Error> {
    match (ctx.platform().os(), &manifest().variant) {
        (fig_os_shim::Os::Linux, Variant::Full) => {
            let metadata_path = bundle_metadata_path(ctx)?;
            debug!("Reading metadata at path: {:?}", &metadata_path);
            Ok(Some(ctx.fs().read_to_string(bundle_metadata_path(ctx)?).await?))
        },
        _ => Ok(None),
    }
}

/// Checks if this is a full build according to the manifest.
/// Note that this does not guarantee the value of is_minimal
pub fn is_full() -> bool {
    cfg_if! {
        if #[cfg(target_os = "macos")] {
            true
        } else if #[cfg(unix)] {
            matches!(
                manifest(),
                Manifest {
                    variant: Variant::Full,
                    ..
                }
            )
        } else if #[cfg(windows)] {
            true
        }
    }
}

/// Checks if this is a minimal build according to the manifest.
/// Note that this does not guarantee the value of is_full
pub fn is_minimal() -> bool {
    cfg_if! {
        if #[cfg(target_os = "macos")] {
            false
        } else if #[cfg(unix)] {
            matches!(
                manifest(),
                Manifest {
                    variant: Variant::Minimal,
                    ..
                }
            )
        } else if #[cfg(windows)] {
            false
        }
    }
}

/// Gets the version from the manifest
#[deprecated = "versions are unified, use env!(\"CARGO_PKG_VERSION\")"]
pub fn version() -> Option<&'static str> {
    Some(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use serde_json::{from_str, to_string};

    use super::*;

    macro_rules! test_ser_deser {
        ($ty:ident, $variant:expr, $text:expr) => {
            let quoted = format!("\"{}\"", $text);
            assert_eq!(quoted, to_string(&$variant).unwrap());
            assert_eq!($variant, from_str(&quoted).unwrap());
            assert_eq!($variant, $ty::from_str($text).unwrap());
            assert_eq!($text, $variant.to_string());
        };
    }

    #[test]
    fn target_triple_does_not_panic_on_an_unknown_triple() {
        let production = include_str!("manifest.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        let start = production.find("pub fn manifest()").expect("manifest");
        let body = &production[start..];
        let end = body.find("\npub async fn bundle_metadata").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            !body.contains(".expect(\"parsing target triple should not fail\")")
                && body.contains("TargetTriple::Other(target.to_string())"),
            "an unknown compile-time TARGET_TRIPLE must map to Other, not abort the process"
        );
        assert!(
            matches!(TargetTriple::from_str("not-a-real-triple"), Ok(TargetTriple::Other(_))),
            "strum default already maps unknown triples to Other"
        );
    }

    #[test]
    fn os_current_does_not_panic_on_an_unknown_os() {
        let production = include_str!("manifest.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        let start = production.find("impl Os").expect("impl Os");
        let body = &production[start..];
        let end = body.find("\nimpl Channel").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            !body.contains("panic!(\"Unsupported OS:") && body.contains("Os::Other(other.to_string())"),
            "freebsd and other unknown OS names must map to Other, not abort the process"
        );
    }

    #[test]
    fn test_target_triple_serialize_deserialize() {
        test_ser_deser!(
            TargetTriple,
            TargetTriple::UniversalAppleDarwin,
            "universal-apple-darwin"
        );
        test_ser_deser!(
            TargetTriple,
            TargetTriple::X86_64UnknownLinuxGnu,
            "x86_64-unknown-linux-gnu"
        );
        test_ser_deser!(
            TargetTriple,
            TargetTriple::AArch64UnknownLinuxGnu,
            "aarch64-unknown-linux-gnu"
        );
        test_ser_deser!(
            TargetTriple,
            TargetTriple::X86_64UnknownLinuxMusl,
            "x86_64-unknown-linux-musl"
        );
        test_ser_deser!(
            TargetTriple,
            TargetTriple::AArch64UnknownLinuxMusl,
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn test_file_type_serialize_deserialize() {
        test_ser_deser!(FileType, FileType::Dmg, "dmg");
        test_ser_deser!(FileType, FileType::TarGz, "tarGz");
        test_ser_deser!(FileType, FileType::TarXz, "tarXz");
        test_ser_deser!(FileType, FileType::TarZst, "tarZst");
        test_ser_deser!(FileType, FileType::Zip, "zip");
        test_ser_deser!(FileType, FileType::AppImage, "appImage");
        test_ser_deser!(FileType, FileType::Deb, "deb");
    }

    #[test]
    fn test_managed_by_serialize_deserialize() {
        test_ser_deser!(ManagedBy, ManagedBy::None, "none");
    }

    #[test]
    fn test_variant_serialize_deserialize() {
        test_ser_deser!(Variant, Variant::Full, "full");
        test_ser_deser!(Variant, Variant::Minimal, "minimal");

        // headless is a special case that should deserialize to Minimal
        assert_eq!(Variant::Minimal, from_str("\"headless\"").unwrap());
        assert_eq!(Variant::Minimal, Variant::from_str("headless").unwrap());
    }

    #[test]
    fn test_channel_serialize_deserialize() {
        test_ser_deser!(Channel, Channel::Stable, "stable");
        test_ser_deser!(Channel, Channel::Beta, "beta");
        test_ser_deser!(Channel, Channel::Qa, "qa");
        test_ser_deser!(Channel, Channel::Nightly, "nightly");
    }
}
