//! 自动更新：问 GitHub 最新 release 是哪个版本，下载对应架构的包并替换自己。
//!
//! 版本探测走 `releases/latest` 的 HTTP 重定向，而不是 GitHub 的 JSON API：
//! API 对同一个 IP 有 60 次/小时的限制（公司网络里很容易撞上），
//! 而重定向只会给出一个 tag，够用且不受配额影响。

use std::path::{Path, PathBuf};
use std::process::Command;

use objc2_foundation::NSBundle;

const REPO: &str = "nowszhao/iRec";

/// 当前版本，来自 Cargo.toml。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

pub fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// 某个版本的安装包地址（和 CI 产出的文件名一致）。
pub fn asset_url(version: &str) -> String {
    format!(
        "https://github.com/{REPO}/releases/download/v{version}/iRec-{version}-{}.zip",
        arch()
    )
}

/// 最新 release 的 tag，形如 `v0.1.4`。会阻塞，请在后台线程调用。
pub fn latest_tag() -> Result<String, String> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "-sS",
            "-L",
            "--max-time",
            "20",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
        ])
        .arg(releases_url())
        .output()
        .map_err(|err| format!("无法发起请求：{err}"))?;
    if !out.status.success() {
        return Err("检查更新失败（网络不通？）".to_string());
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match url.rsplit_once("/releases/tag/") {
        Some((_, tag)) if !tag.is_empty() => Ok(tag.trim_end_matches('/').to_string()),
        // 没跳转说明请求没走到 release 页面，别把 "latest" 当版本号。
        _ => Err("没能读到最新版本号".to_string()),
    }
}

/// `v0.1.4` → `0.1.4`。
pub fn strip_tag(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// `candidate` 是否比 `current` 新（点分数字逐段比较，缺段按 0）。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(text: &str) -> (u32, u32, u32) {
    let cleaned = strip_tag(text);
    let mut parts = cleaned.split('.');
    let mut next = || {
        parts
            .next()
            .and_then(|part| part.trim().parse().ok())
            .unwrap_or(0)
    };
    (next(), next(), next())
}

/// 下载并解压到 `~/Library/Caches/iRec/update/<version>/iRec.app`。
pub fn download(version: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "拿不到 HOME 目录".to_string())?;
    let dir = PathBuf::from(home)
        .join("Library/Caches/iRec/update")
        .join(version);
    // 同一个版本重下时先清干净，别让上一次的残留下混进来。
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|err| format!("无法创建缓存目录：{err}"))?;

    let zip = dir.join(format!("iRec-{version}-{}.zip", arch()));
    let status = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "300", "-o"])
        .arg(&zip)
        .arg(asset_url(version))
        .status()
        .map_err(|err| format!("下载失败：{err}"))?;
    if !status.success() {
        return Err("下载失败，稍后再试".to_string());
    }

    let unzip = Command::new("/usr/bin/unzip")
        .args(["-q", "-o"])
        .arg(&zip)
        .arg("-d")
        .arg(&dir)
        .status()
        .map_err(|err| format!("解压失败：{err}"))?;
    if !unzip.success() {
        return Err("解压失败，包可能不完整".to_string());
    }

    let app = dir.join("iRec.app");
    let binary = app.join("Contents/MacOS/iRec");
    if !binary.is_file() {
        return Err("包里没有 iRec.app，已放弃更新".to_string());
    }
    // 网上下的包带着隔离属性，不清掉的话第一次打开会被 Gatekeeper 拦。
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&app)
        .status();
    Ok(app)
}

/// 排一条「等自己退出后换包再打开」的脚本。返回 Err 表示这里换不了，只能手动更新。
///
/// 脚本必须等旧进程真的退出：否则 `open` 只会把还在跑的旧实例叫到前台。
pub fn replace_and_relaunch(staged: &Path) -> Result<(), String> {
    let bundle = NSBundle::mainBundle();
    let target = bundle.bundlePath().to_string();
    if !target.ends_with(".app") {
        return Err("当前不是从 iRec.app 启动的，请到 Releases 页面手动更新".to_string());
    }
    let pid = std::process::id();
    let script = std::env::temp_dir().join(format!("irec-update-{pid}.sh"));
    let old = quote(&target);
    let new = quote(&staged.to_string_lossy());
    let backup = format!("{old}.irec-old-{pid}");
    let body = format!(
        "#!/bin/sh\n\
         # iRec 自动更新：等旧进程退出后换包，再打开新包。\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         if ! mv {old} {backup}; then exit 1; fi\n\
         if cp -R {new} {old}; then\n\
         \x20 rm -rf {backup}\n\
         else\n\
         \x20 mv {backup} {old}\n\
         \x20 exit 1\n\
         fi\n\
         xattr -dr com.apple.quarantine {old} 2>/dev/null\n\
         open {old}\n"
    );
    std::fs::write(&script, body).map_err(|err| format!("无法写更新脚本：{err}"))?;
    Command::new("/bin/sh")
        .arg(&script)
        .spawn()
        .map_err(|err| format!("无法启动更新脚本：{err}"))?;
    Ok(())
}

/// 给 sh 用的单引号转义。
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
