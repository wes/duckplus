//! Updates from GitHub releases.
//!
//! The newest version comes from where `releases/latest` redirects to, so
//! there's no API rate limit or JSON to parse. Installing downloads that
//! release's DMG, checks it against its published SHA-256, and only accepts
//! an app that's notarized and signed by our team before swapping it in for
//! the running one and relaunching.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui_kit::*;
use semver::Version;
use sha2::{Digest as _, Sha256};

pub const RELEASES: &str = "https://github.com/wes/duckplus/releases";
/// The team that signs DuckPlus releases; an update signed by anyone else is refused.
const TEAM_ID: &str = "288BJX6YHP";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Default)]
pub enum UpdateState {
    /// Nothing to show: not checked yet, or up to date after a background check.
    #[default]
    Idle,
    /// The user asked and we're checking.
    Checking,
    /// Up to date, after the user asked.
    UpToDate,
    Available(Version),
    /// Downloading, verifying and swapping in the new version.
    Installing {
        version: Version,
        /// Download progress, 0.0–1.0, as `f32` bits.
        progress: Arc<AtomicU32>,
    },
    /// Something the user needs to finish or read.
    Message(String),
    Failed(String),
}

impl UpdateState {
    /// Download progress while installing.
    pub fn progress(&self) -> Option<f32> {
        match self {
            Self::Installing { progress, .. } => Some(f32::from_bits(progress.load(Relaxed))),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct Updater {
    pub state: UpdateState,
}

impl Global for Updater {}

/// The version this build is. `DUCKPLUS_PRETEND_VERSION` overrides it, to try
/// the update flow against a real release.
pub fn current() -> Version {
    std::env::var("DUCKPLUS_PRETEND_VERSION")
        .ok()
        .and_then(|v| Version::parse(&v).ok())
        .unwrap_or_else(|| Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version"))
}

/// Start checking now and every few hours after.
pub fn init(cx: &mut App) {
    cx.set_global(Updater::default());
    cx.spawn(async move |cx| {
        loop {
            check(false, cx).await;
            cx.background_executor().timer(CHECK_EVERY).await;
        }
    })
    .detach();
}

/// Check because the user asked, so "up to date" and errors are shown too.
pub fn check_now(cx: &mut App) {
    if matches!(cx.global::<Updater>().state, UpdateState::Installing { .. }) {
        return;
    }
    set_state(UpdateState::Checking, cx);
    cx.spawn(async move |cx| check(true, cx).await).detach();
}

fn set_state(state: UpdateState, cx: &mut App) {
    cx.update_global::<Updater, _>(|u, _| u.state = state);
}

async fn check(manual: bool, cx: &mut AsyncApp) {
    let busy = cx.update(|cx| matches!(cx.global::<Updater>().state, UpdateState::Installing { .. }));
    if busy {
        return;
    }
    let latest = cx.background_executor().spawn(async { latest_version() }).await;
    cx.update(|cx| {
        let state = match latest {
            Ok(v) if v > current() => UpdateState::Available(v),
            Ok(_) if manual => UpdateState::UpToDate,
            Ok(_) => UpdateState::Idle,
            Err(e) if manual => UpdateState::Failed(format!("Couldn't check for updates: {e:#}")),
            // A background check that fails (offline, say) stays quiet.
            Err(_) => return,
        };
        let up_to_date = matches!(state, UpdateState::UpToDate);
        set_state(state, cx);
        if up_to_date {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(8)).await;
                cx.update(|cx| {
                    if matches!(cx.global::<Updater>().state, UpdateState::UpToDate) {
                        set_state(UpdateState::Idle, cx);
                    }
                });
            })
            .detach();
        }
    });
}

/// Install `version` over the running app and relaunch into it.
pub fn install(version: Version, cx: &mut App) {
    let Some(app) = running_app() else {
        // A development build (cargo run): there's no bundle to replace.
        cx.open_url(&format!("{RELEASES}/tag/v{version}"));
        return;
    };
    let progress = Arc::new(AtomicU32::new(0));
    set_state(
        UpdateState::Installing { version: version.clone(), progress: progress.clone() },
        cx,
    );
    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn({
                let app = app.clone();
                async move { install_update(&version, &app, &progress) }
            })
            .await;
        cx.update(|cx| match outcome {
            Ok(Installed::Replaced) => {
                relaunch(&app);
                cx.quit();
            }
            Ok(Installed::OpenedDmg) => set_state(
                UpdateState::Message(format!(
                    "DuckPlus can't write to {}. In the window that opened, drag DuckPlus into Applications.",
                    app.parent().map(|p| p.display().to_string()).unwrap_or_default()
                )),
                cx,
            ),
            Err(e) => set_state(UpdateState::Failed(format!("Update failed: {e:#}")), cx),
        });
    })
    .detach();
}

/// The `.app` bundle this process runs from, if any.
fn running_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

fn agent(follow_redirects: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(if follow_redirects { 10 } else { 0 })
        .max_redirects_will_error(false)
        .timeout_global(Some(Duration::from_secs(if follow_redirects { 600 } else { 20 })))
        .user_agent(format!("DuckPlus/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The newest release, from where `releases/latest` redirects.
fn latest_version() -> Result<Version> {
    let res = agent(false).get(format!("{RELEASES}/latest")).call()?;
    let location = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("GitHub didn't point to a latest release"))?;
    version_from_tag_url(location)
}

/// `…/releases/tag/v1.2.3` → 1.2.3
fn version_from_tag_url(url: &str) -> Result<Version> {
    let tag = url.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    Version::parse(tag.trim_start_matches('v')).with_context(|| format!("unexpected release tag {tag:?}"))
}

enum Installed {
    /// The new app is in place of the old one.
    Replaced,
    /// The app's folder isn't writable, so the DMG was opened for the user.
    OpenedDmg,
}

fn install_update(version: &Version, app: &Path, progress: &AtomicU32) -> Result<Installed> {
    let dir = std::env::temp_dir().join(format!("duckplus-update-{version}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let name = format!("DuckPlus-{version}.dmg");
    let base = format!("{RELEASES}/download/v{version}");

    let expected = agent(true)
        .get(format!("{base}/{name}.sha256"))
        .call()?
        .body_mut()
        .read_to_string()?;
    let expected = expected
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("empty checksum file"))?
        .to_lowercase();
    let dmg = dir.join(&name);
    let actual = download(&format!("{base}/{name}"), &dmg, progress)?;
    if actual != expected {
        bail!("the download doesn't match its published checksum");
    }

    let mount = dir.join("mnt");
    fs::create_dir_all(&mount)?;
    run("hdiutil", &["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"], &[&mount, &dmg])?;
    let detach = || {
        let _ = Command::new("hdiutil").arg("detach").arg(&mount).arg("-quiet").status();
    };
    let result = (|| -> Result<Option<PathBuf>> {
        let new_app = mount.join("DuckPlus.app");
        verify_app(&new_app)?;
        let parent = app.parent().ok_or_else(|| anyhow!("no folder around {}", app.display()))?;
        if !writable(parent) {
            return Ok(None);
        }
        // Copy next to the old app first, so the swap is two renames on one volume.
        let staged = parent.join(".DuckPlus-update.app");
        let _ = fs::remove_dir_all(&staged);
        run("ditto", &[], &[&new_app, &staged])?;
        Ok(Some(staged))
    })();
    detach();
    let Some(staged) = result? else {
        run("open", &[], &[&dmg])?;
        return Ok(Installed::OpenedDmg);
    };

    // The running process keeps its files open, so the bundle can move under it.
    let old = app.with_file_name(".DuckPlus-old.app");
    let _ = fs::remove_dir_all(&old);
    fs::rename(app, &old).context("couldn't move the old version aside")?;
    if let Err(e) = fs::rename(&staged, app) {
        let _ = fs::rename(&old, app);
        return Err(e).context("couldn't move the new version into place");
    }
    let _ = fs::remove_dir_all(&old);
    let _ = fs::remove_dir_all(&dir);
    // Refresh Launch Services so Spotlight and the Dock see the new version.
    let _ = Command::new(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
    )
    .arg("-f")
    .arg(app)
    .status();
    Ok(Installed::Replaced)
}

/// Stream `url` to `dest`, reporting progress, and return its SHA-256.
fn download(url: &str, dest: &Path, progress: &AtomicU32) -> Result<String> {
    let mut res = agent(true).get(url).call()?;
    let total = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = res.body_mut().with_config().limit(1 << 30).reader();
    let mut file = fs::File::create(dest)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        read += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            progress.store((read as f32 / total as f32).min(1.).to_bits(), Relaxed);
        }
    }
    file.flush()?;
    Ok(hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Accept only an intact app, notarized, from our team.
fn verify_app(app: &Path) -> Result<()> {
    if !app.exists() {
        bail!("the disk image has no DuckPlus.app");
    }
    run("codesign", &["--verify", "--deep", "--strict"], &[app])
        .context("the new version's signature is invalid")?;
    let info = Command::new("codesign").arg("-dv").arg(app).output()?;
    let details = String::from_utf8_lossy(&info.stderr);
    if !details.lines().any(|l| l == format!("TeamIdentifier={TEAM_ID}")) {
        bail!("the new version isn't signed by the DuckPlus team");
    }
    run("spctl", &["--assess", "--type", "execute"], &[app])
        .context("macOS doesn't accept the new version as notarized")?;
    Ok(())
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".duckplus-write-test-{}", std::process::id()));
    let ok = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn run(program: &str, args: &[&str], paths: &[&Path]) -> Result<()> {
    let out = Command::new(program).args(args).args(paths).output()?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Reopen `app` once this process has exited.
fn relaunch(app: &Path) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; open "$2""#)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::{Installed, Version, install_update, latest_version, verify_app, version_from_tag_url};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    #[test]
    fn tags() {
        let v = version_from_tag_url("https://github.com/wes/duckplus/releases/tag/v0.1.1").unwrap();
        assert_eq!(v, Version::new(0, 1, 1));
        assert!(version_from_tag_url("https://github.com/wes/duckplus/releases").is_err());
        assert!(Version::new(0, 1, 10) > Version::new(0, 1, 9));
    }

    /// An app that isn't signed by the DuckPlus team is never installed.
    #[test]
    fn refuses_other_signers() {
        let dir = std::env::temp_dir().join(format!("duckplus-verify-{}", std::process::id()));
        let app = dir.join("DuckPlus.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::copy("/usr/bin/true", app.join("Contents/MacOS/DuckPlus")).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>DuckPlus</string>\
             <key>CFBundleIdentifier</key><string>app.duckplus.DuckPlus</string></dict></plist>",
        )
        .unwrap();
        // Unsigned, then signed ad hoc (valid signature, no team).
        assert!(verify_app(&app).is_err());
        let signed = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .status()
            .unwrap();
        assert!(signed.success());
        let err = verify_app(&app).unwrap_err().to_string();
        assert!(err.contains("isn't signed by the DuckPlus team"), "{err}");
        assert!(verify_app(&dir.join("Missing.app")).is_err());
        fs::remove_dir_all(dir).ok();
    }

    /// Asks GitHub for the latest release.
    #[test]
    #[ignore]
    fn latest_release() {
        println!("latest: {}", latest_version().unwrap());
    }

    /// The whole install against a scratch copy of the app:
    /// DUCKPLUS_TEST_APP=/tmp/x/DuckPlus.app cargo test install_into_scratch -- --ignored
    #[test]
    #[ignore]
    fn install_into_scratch() {
        let app = PathBuf::from(std::env::var("DUCKPLUS_TEST_APP").unwrap());
        let version = latest_version().unwrap();
        let progress = AtomicU32::new(0);
        assert!(matches!(install_update(&version, &app, &progress).unwrap(), Installed::Replaced));
        assert_eq!(f32::from_bits(progress.load(Relaxed)), 1.0);
        verify_app(&app).unwrap();
        let plist = fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains(&format!("<string>{version}</string>")));
    }
}
