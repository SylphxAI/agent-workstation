//! Deterministic per-profile fingerprint seed, applied at launch only.
//!
//! A profile's `fingerprint.json` records the identity a browser presents:
//! window size, languages, timezone and (optionally) a user agent. It is
//! created once from a seed derived from the profile name and never rewritten,
//! so the same profile always looks the same.
//!
//! Only launch-time mechanisms apply it: the display and window size, the
//! `--accept-lang` flag and locale environment, the `TZ` environment variable
//! and, when [`UserAgentMode::FromFingerprint`] is chosen, the `--user-agent`
//! flag. No script is ever injected into pages. Hardware and graphics fields
//! that older writers of this format stored are read and preserved but not
//! applied: the real machine's values are what a site sees.
//!
//! The file format is a superset-compatible JSON object; unknown fields are
//! kept and written back verbatim if the file is ever re-published.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The seed for a profile name: FNV-1a followed by a SplitMix64 finaliser.
/// Stable across releases; existing files keep whatever seed they recorded.
pub fn profile_seed(profile: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in profile.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// The contents of `fingerprint.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Profile name the seed was derived from.
    pub profile: String,
    /// Deterministic seed.
    pub seed: u64,
    /// User agent recorded at creation; applied only with
    /// [`UserAgentMode::FromFingerprint`].
    #[serde(default)]
    pub user_agent: String,
    /// Browser major version recorded at creation.
    #[serde(default)]
    pub chrome_major: String,
    /// Browser full version recorded at creation.
    #[serde(default)]
    pub chrome_full_version: String,
    /// `navigator.platform` recorded at creation (not applied).
    #[serde(default)]
    pub platform: String,
    /// Legacy field (not applied).
    #[serde(default)]
    pub oscpu: String,
    /// Recorded, not applied: the real machine's value is reported.
    #[serde(default)]
    pub hardware_concurrency: u32,
    /// Recorded, not applied: the real machine's value is reported.
    #[serde(default)]
    pub device_memory: u32,
    /// Display and window width in pixels.
    pub screen_width: u32,
    /// Display and window height in pixels.
    pub screen_height: u32,
    /// Recorded, not applied: the real renderer is reported.
    #[serde(default)]
    pub webgl_vendor: String,
    /// Recorded, not applied: the real renderer is reported.
    #[serde(default)]
    pub webgl_renderer: String,
    /// Preferred languages, most preferred first (BCP 47).
    pub languages: Vec<String>,
    /// IANA timezone, e.g. `America/New_York`.
    pub timezone: String,
    /// Fields this version does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Caller-chosen values for a newly created fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintDefaults {
    /// Preferred languages, most preferred first.
    pub languages: Vec<String>,
    /// IANA timezone.
    pub timezone: String,
    /// Display width.
    pub screen_width: u32,
    /// Display height.
    pub screen_height: u32,
}

impl Default for FingerprintDefaults {
    fn default() -> Self {
        Self {
            languages: vec!["en-US".into(), "en".into()],
            timezone: "UTC".into(),
            screen_width: 1920,
            screen_height: 1080,
        }
    }
}

/// Whether the browser reports its own user agent or the recorded one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAgentMode {
    /// The engine's real user agent and client hints (consistent everywhere,
    /// workers included). Default.
    #[default]
    Native,
    /// The recorded `user_agent`, via the engine's launch flag.
    FromFingerprint,
}

/// Bounds a fingerprint must satisfy before it is applied.
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 7680;

impl Fingerprint {
    /// A new fingerprint for `profile`.
    pub fn generate(profile: &str, defaults: &FingerprintDefaults) -> Self {
        Self {
            profile: profile.to_string(),
            seed: profile_seed(profile),
            user_agent: String::new(),
            chrome_major: String::new(),
            chrome_full_version: String::new(),
            platform: String::new(),
            oscpu: String::new(),
            hardware_concurrency: 0,
            device_memory: 0,
            screen_width: defaults.screen_width,
            screen_height: defaults.screen_height,
            webgl_vendor: String::new(),
            webgl_renderer: String::new(),
            languages: defaults.languages.clone(),
            timezone: defaults.timezone.clone(),
            extra: Default::default(),
        }
    }

    /// Check the fields that are applied at launch.
    pub fn validate(&self) -> Result<()> {
        for (name, v) in [
            ("screen_width", self.screen_width),
            ("screen_height", self.screen_height),
        ] {
            if !(MIN_SIDE..=MAX_SIDE).contains(&v) {
                return Err(Error::Profile(format!(
                    "fingerprint {name} {v} is outside {MIN_SIDE}..={MAX_SIDE}"
                )));
            }
        }
        if self.languages.is_empty() || self.languages.iter().any(|l| !is_language_tag(l)) {
            return Err(Error::Profile(
                "fingerprint languages must be BCP 47 tags".into(),
            ));
        }
        if !is_timezone_name(&self.timezone) {
            return Err(Error::Profile(format!(
                "fingerprint timezone {:?} is not an IANA name",
                self.timezone
            )));
        }
        Ok(())
    }

    /// Value for Chromium's `--accept-lang` flag (`navigator.languages` and
    /// the `Accept-Language` header).
    pub fn accept_language(&self) -> String {
        self.languages.join(",")
    }

    /// POSIX locale for `LANG`, e.g. `en_US.UTF-8`.
    pub fn posix_locale(&self) -> String {
        let primary = self
            .languages
            .first()
            .map(String::as_str)
            .unwrap_or("en-US");
        let mut parts = primary.split('-');
        let lang = parts.next().unwrap_or("en").to_ascii_lowercase();
        match parts.find(|p| p.len() == 2) {
            Some(region) => format!("{lang}_{}.UTF-8", region.to_ascii_uppercase()),
            None => format!("{lang}.UTF-8"),
        }
    }

    /// `LANGUAGE` list for the browser's UI language, e.g. `en_US:en`.
    pub fn posix_language_list(&self) -> String {
        self.languages
            .iter()
            .map(|l| l.replace('-', "_"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Environment variables applied to the browser process.
    pub fn launch_env(&self) -> Vec<(String, String)> {
        vec![
            ("TZ".into(), self.timezone.clone()),
            ("LANG".into(), self.posix_locale()),
            ("LANGUAGE".into(), self.posix_language_list()),
        ]
    }

    /// Read `path`. `Ok(None)` when it does not exist.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::Profile(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Profile(format!("invalid {}: {e}", path.display())))
    }

    /// Publish atomically (temp file, fsync, rename, directory fsync), mode 0600.
    /// Refuses to replace an existing file: a profile's fingerprint is written once.
    pub fn publish_new(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| Error::Profile("fingerprint path has no parent".into()))?;
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| Error::Profile(e.to_string()))?;
        let tmp = dir.join(format!(".fingerprint.{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            // hard_link fails if `path` exists, so a concurrent creator wins cleanly.
            std::fs::hard_link(&tmp, path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::Profile(format!("{} already exists", path.display()))
                } else {
                    e.into()
                }
            })?;
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// Load `path`, or create it from `profile` and `defaults` when absent.
    pub fn load_or_create(
        path: &Path,
        profile: &str,
        defaults: &FingerprintDefaults,
    ) -> Result<Self> {
        if let Some(fp) = Self::read(path)? {
            return Ok(fp);
        }
        let fp = Self::generate(profile, defaults);
        fp.validate()?;
        match fp.publish_new(path) {
            Ok(()) => Ok(fp),
            Err(_) if path.exists() => Self::read(path)?
                .ok_or_else(|| Error::Profile(format!("{} vanished", path.display()))),
            Err(e) => Err(e),
        }
    }
}

fn is_language_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 35
        && s.split('-')
            .all(|p| !p.is_empty() && p.len() <= 8 && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn is_timezone_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('/')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in the exact shape existing consumers wrote, values synthetic.
    const EXISTING: &str = r#"{
  "profile": "work",
  "seed": 1234567890123,
  "user_agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.7103.92 Safari/537.36",
  "chrome_major": "136",
  "chrome_full_version": "136.0.7103.92",
  "platform": "Linux x86_64",
  "oscpu": "Linux x86_64",
  "hardware_concurrency": 8,
  "device_memory": 16,
  "screen_width": 1920,
  "screen_height": 1080,
  "webgl_vendor": "Google Inc. (Intel)",
  "webgl_renderer": "ANGLE (Intel)",
  "languages": ["fr-CA", "fr", "en-US", "en"],
  "timezone": "America/Toronto"
}"#;

    #[test]
    fn existing_file_loads_as_is_and_is_never_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fingerprint.json");
        std::fs::write(&path, EXISTING).unwrap();
        let fp = Fingerprint::load_or_create(&path, "other-name", &Default::default()).unwrap();
        assert_eq!(fp.seed, 1234567890123);
        assert_eq!(fp.timezone, "America/Toronto");
        assert_eq!(fp.accept_language(), "fr-CA,fr,en-US,en");
        assert_eq!(fp.posix_locale(), "fr_CA.UTF-8");
        assert_eq!(fp.posix_language_list(), "fr_CA:fr:en_US:en");
        fp.validate().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXISTING);
        assert!(
            fp.publish_new(&path).is_err(),
            "never replaces an existing file"
        );
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let mut v: serde_json::Value = serde_json::from_str(EXISTING).unwrap();
        v["future_field"] = serde_json::json!({"x": 1});
        let fp: Fingerprint = serde_json::from_value(v.clone()).unwrap();
        let back = serde_json::to_value(&fp).unwrap();
        assert_eq!(back["future_field"], v["future_field"]);
    }

    #[test]
    fn new_profile_gets_a_deterministic_seed_and_caller_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p").join("fingerprint.json");
        let defaults = FingerprintDefaults {
            languages: vec!["de-DE".into()],
            timezone: "Europe/Berlin".into(),
            screen_width: 1600,
            screen_height: 900,
        };
        let a = Fingerprint::load_or_create(&path, "alpha", &defaults).unwrap();
        assert_eq!(a.seed, profile_seed("alpha"));
        assert_ne!(profile_seed("alpha"), profile_seed("beta"));
        assert_eq!(a.screen_width, 1600);
        let again =
            Fingerprint::load_or_create(&path, "alpha", &FingerprintDefaults::default()).unwrap();
        assert_eq!(again, a, "second load reads the file, not the new defaults");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn seed_matches_the_published_algorithm() {
        // FNV-1a of the empty string, then SplitMix64 finalised.
        assert_eq!(profile_seed(""), {
            let mut h: u64 = 0xcbf29ce484222325;
            h ^= h >> 30;
            h = h.wrapping_mul(0xbf58476d1ce4e5b9);
            h ^= h >> 27;
            h = h.wrapping_mul(0x94d049bb133111eb);
            h ^ (h >> 31)
        });
    }

    #[test]
    fn invalid_applied_fields_are_refused() {
        let mut fp = Fingerprint::generate("x", &Default::default());
        fp.timezone = "../etc/passwd".into();
        assert!(fp.validate().is_err());
        let mut fp = Fingerprint::generate("x", &Default::default());
        fp.screen_width = 10;
        assert!(fp.validate().is_err());
        let mut fp = Fingerprint::generate("x", &Default::default());
        fp.languages = vec!["en US".into()];
        assert!(fp.validate().is_err());
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fingerprint.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Fingerprint::load_or_create(&path, "x", &Default::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }
}
