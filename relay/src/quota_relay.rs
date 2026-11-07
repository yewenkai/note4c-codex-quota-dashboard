use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use embedded_graphics::geometry::{OriginDimensions, Size};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::{DrawTarget, Pixel, Point};
use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use u8g2_fonts::FontRenderer;
use u8g2_fonts::fonts::{
    u8g2_font_wqy13_t_gb2312, u8g2_font_wqy14_t_gb2312, u8g2_font_wqy16_t_gb2312,
};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use crate::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

pub const BWRY_FRAME_SIZE: usize = (DISPLAY_WIDTH as usize * DISPLAY_HEIGHT as usize) / 4;
pub const FRAME_FORMAT: &str = "bwry2bpp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BwryColor {
    Black = 0,
    White = 1,
    Yellow = 2,
    Red = 3,
}

impl BwryColor {
    fn rgb(self) -> Rgb<u8> {
        match self {
            Self::Black => Rgb([0, 0, 0]),
            Self::White => Rgb([255, 255, 255]),
            Self::Yellow => Rgb([255, 214, 0]),
            Self::Red => Rgb([220, 30, 30]),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BwryFrame {
    pixels: Vec<BwryColor>,
}

impl BwryFrame {
    fn new() -> Self {
        Self {
            pixels: vec![BwryColor::White; (DISPLAY_WIDTH * DISPLAY_HEIGHT) as usize],
        }
    }

    fn set(&mut self, point: Point, color: BwryColor) {
        if point.x >= 0
            && point.y >= 0
            && point.x < DISPLAY_WIDTH as i32
            && point.y < DISPLAY_HEIGHT as i32
        {
            let index = point.y as usize * DISPLAY_WIDTH as usize + point.x as usize;
            self.pixels[index] = color;
        }
    }

    fn fill(&mut self, x: i32, y: i32, width: u32, height: u32, color: BwryColor) {
        let x_end = (x + width as i32).min(DISPLAY_WIDTH as i32);
        let y_end = (y + height as i32).min(DISPLAY_HEIGHT as i32);
        for py in y.max(0)..y_end {
            for px in x.max(0)..x_end {
                self.set(Point::new(px, py), color);
            }
        }
    }

    pub fn packed_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; BWRY_FRAME_SIZE];
        for (index, chunk) in self.pixels.chunks_exact(4).enumerate() {
            bytes[index] = ((chunk[0] as u8) << 6)
                | ((chunk[1] as u8) << 4)
                | ((chunk[2] as u8) << 2)
                | chunk[3] as u8;
        }
        bytes
    }

    pub fn sha256(&self) -> String {
        format!("{:x}", Sha256::digest(self.packed_bytes()))
    }

    pub fn png_bytes(&self) -> Result<Vec<u8>, QuotaRelayError> {
        let mut image = RgbImage::new(DISPLAY_WIDTH, DISPLAY_HEIGHT);
        for (index, color) in self.pixels.iter().enumerate() {
            image.put_pixel(
                index as u32 % DISPLAY_WIDTH,
                index as u32 / DISPLAY_WIDTH,
                color.rgb(),
            );
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image).write_to(&mut output, image::ImageFormat::Png)?;
        Ok(output.into_inner())
    }
}

struct ColorMask<'a> {
    frame: &'a mut BwryFrame,
    color: BwryColor,
}

impl OriginDimensions for ColorMask<'_> {
    fn size(&self) -> Size {
        Size::new(DISPLAY_WIDTH, DISPLAY_HEIGHT)
    }
}

impl DrawTarget for ColorMask<'_> {
    type Color = BinaryColor;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if color == BinaryColor::On {
                self.frame.set(point, self.color);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexAuthRegistry {
    pub schema_version: u32,
    pub active_account_key: Option<String>,
    pub accounts: Vec<CodexAuthAccount>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexAuthAccount {
    pub account_key: String,
    pub email: String,
    pub plan: Option<String>,
    pub last_usage: Option<CodexAuthUsage>,
    pub last_usage_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexAuthUsage {
    pub plan_type: Option<String>,
    pub primary: Option<CodexAuthWindow>,
    pub secondary: Option<CodexAuthWindow>,
    pub credits: Option<CodexAuthCredits>,
    pub reset_credits: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexAuthCredits {
    pub balance: Option<String>,
    pub unlimited: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexAuthWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PaidAccountQuota {
    #[serde(skip)]
    pub account_key: String,
    pub email: String,
    pub plan: String,
    pub active: bool,
    pub five_hour_remaining_percent: u8,
    pub five_hour_resets_at: i64,
    pub weekly_remaining_percent: u8,
    pub weekly_resets_at: i64,
    pub observed_at: i64,
    pub credits: Option<String>,
    pub reset_credits: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveFreeQuota {
    Unknown,
    Window { remaining: u8, resets_at: i64 },
}

impl CodexAuthRegistry {
    pub fn read(path: impl AsRef<Path>) -> Result<Self, QuotaRelayError> {
        let registry: Self = serde_json::from_slice(&fs::read(path)?)?;
        if !matches!(registry.schema_version, 3 | 4) {
            return Err(QuotaRelayError::InvalidRegistry(format!(
                "不支持 codex-auth registry schema {}（预期 3 或 4）",
                registry.schema_version
            )));
        }
        Ok(registry)
    }

    pub fn paid_accounts(
        &self,
        expected_count: usize,
    ) -> Result<Vec<&CodexAuthAccount>, QuotaRelayError> {
        let mut paid: Vec<_> = self
            .accounts
            .iter()
            .filter(|account| account.is_paid())
            .collect();
        if paid.len() != expected_count {
            return Err(QuotaRelayError::InvalidRegistry(format!(
                "付费账号数量为 {}，预期 {}；拒绝覆盖现有画面",
                paid.len(),
                expected_count
            )));
        }
        let mut keys = HashSet::new();
        if paid
            .iter()
            .any(|account| !keys.insert(&account.account_key))
        {
            return Err(QuotaRelayError::InvalidRegistry("付费账号标识重复".into()));
        }
        paid.sort_by(|left, right| {
            (&left.email, &left.account_key).cmp(&(&right.email, &right.account_key))
        });
        Ok(paid)
    }

    pub fn cached_quotas(
        &self,
        expected_count: usize,
    ) -> Result<Vec<PaidAccountQuota>, QuotaRelayError> {
        self.paid_accounts(expected_count)?
            .into_iter()
            .map(|account| {
                let usage = account.last_usage.as_ref().ok_or_else(|| {
                    QuotaRelayError::InvalidRegistry(format!("{} 缺少 last_usage", account.email))
                })?;
                let observed_at = account.last_usage_at.ok_or_else(|| {
                    QuotaRelayError::InvalidRegistry(format!(
                        "{} 缺少 last_usage_at",
                        account.email
                    ))
                })?;
                account.quota(usage, observed_at, self.is_active(account))
            })
            .collect()
    }

    pub fn is_active(&self, account: &CodexAuthAccount) -> bool {
        self.active_account_key.as_deref() == Some(account.account_key.as_str())
    }

    pub fn active_free_quota(&self) -> Option<ActiveFreeQuota> {
        let account = self
            .accounts
            .iter()
            .find(|account| self.is_active(account) && account.plan() == Some("free"))?;
        let window = account
            .last_usage
            .as_ref()
            .filter(|usage| usage.plan_type.as_deref() == Some("free"))
            .and_then(|usage| usage.primary.as_ref())
            .and_then(CodexAuthWindow::remaining_and_reset);
        Some(match window {
            Some((remaining, resets_at)) => ActiveFreeQuota::Window {
                remaining,
                resets_at,
            },
            None => ActiveFreeQuota::Unknown,
        })
    }
}

impl CodexAuthWindow {
    fn remaining_and_reset(&self) -> Option<(u8, i64)> {
        let resets_at = self.resets_at?;
        (resets_at > 0 && (0.0..=100.0).contains(&self.used_percent))
            .then_some(((100.0 - self.used_percent) as u8, resets_at))
    }
}

impl CodexAuthAccount {
    pub fn plan(&self) -> Option<&str> {
        // A local rollout can supply usage from a different workspace after a switch.
        self.plan
            .as_deref()
            .or_else(|| self.last_usage.as_ref()?.plan_type.as_deref())
    }

    fn is_paid(&self) -> bool {
        paid_plan(self.plan()).is_some()
    }

    pub fn quota(
        &self,
        usage: &CodexAuthUsage,
        observed_at: i64,
        active: bool,
    ) -> Result<PaidAccountQuota, QuotaRelayError> {
        let plan = paid_plan(self.plan()).ok_or_else(|| {
            QuotaRelayError::InvalidRegistry(format!("{} 缺少支持的付费计划", self.email))
        })?;
        if usage.plan_type.is_some() && paid_plan(usage.plan_type.as_deref()) != Some(plan) {
            return Err(QuotaRelayError::InvalidRegistry(format!(
                "{} 的计划与额度记录不一致",
                self.email
            )));
        }
        let windows = [usage.primary.as_ref(), usage.secondary.as_ref()];
        let five_hour = find_window(&windows, 300).ok_or_else(|| {
            QuotaRelayError::InvalidRegistry(format!("{} 缺少 5 小时额度窗口", self.email))
        })?;
        let weekly = find_window(&windows, 10_080).ok_or_else(|| {
            QuotaRelayError::InvalidRegistry(format!("{} 缺少周额度窗口", self.email))
        })?;
        let (five_hour_remaining_percent, five_hour_resets_at) =
            quota_window(&self.email, "5 小时", five_hour)?;
        let (weekly_remaining_percent, weekly_resets_at) = quota_window(&self.email, "周", weekly)?;
        let credits = usage.credits.as_ref().and_then(|credits| {
            credits
                .balance
                .as_deref()
                .and_then(|balance| {
                    balance
                        .trim()
                        .split('.')
                        .next()?
                        .parse::<i64>()
                        .ok()
                        .map(|value| value.to_string())
                })
                .or_else(|| credits.unlimited.then(|| "不限".into()))
        });
        Ok(PaidAccountQuota {
            account_key: self.account_key.clone(),
            email: self.email.clone(),
            plan: plan.into(),
            active,
            five_hour_remaining_percent,
            five_hour_resets_at,
            weekly_remaining_percent,
            weekly_resets_at,
            observed_at,
            credits,
            reset_credits: usage.reset_credits.filter(|count| *count >= 0),
        })
    }
}

fn paid_plan(plan: Option<&str>) -> Option<&'static str> {
    match plan {
        Some("team" | "business") => Some("Business"),
        Some("plus") => Some("Plus"),
        _ => None,
    }
}

fn find_window<'a>(
    windows: &[Option<&'a CodexAuthWindow>],
    window_minutes: i64,
) -> Option<&'a CodexAuthWindow> {
    windows
        .iter()
        .flatten()
        .find(|window| window.window_minutes == Some(window_minutes))
        .copied()
}

fn quota_window(
    email: &str,
    label: &str,
    window: &CodexAuthWindow,
) -> Result<(u8, i64), QuotaRelayError> {
    window
        .remaining_and_reset()
        .ok_or_else(|| QuotaRelayError::InvalidRegistry(format!("{email} 的{label}额度窗口无效")))
}

pub fn validate_freshness(
    accounts: &[PaidAccountQuota],
    minimum_observed_at: i64,
) -> Result<(), QuotaRelayError> {
    if let Some(account) = accounts
        .iter()
        .find(|account| account.observed_at < minimum_observed_at)
    {
        return Err(QuotaRelayError::StaleUsage {
            email: account.email.clone(),
            observed_at: account.observed_at,
            minimum: minimum_observed_at,
        });
    }
    Ok(())
}

pub fn apply_account_labels(
    accounts: &mut [PaidAccountQuota],
    labels: &HashMap<String, String>,
) -> Result<(), QuotaRelayError> {
    for account in accounts {
        if let Some(label) = labels
            .get(&account.account_key)
            .or_else(|| labels.get(&account.email))
        {
            let label = label.trim();
            if label.is_empty() || label.chars().count() > 64 {
                return Err(QuotaRelayError::InvalidRegistry(format!(
                    "{} 的 accountLabels 必须为 1–64 个字符",
                    account.email
                )));
            }
            account.email = label.to_owned();
        }
    }
    Ok(())
}

pub fn render_paid_accounts(
    accounts: &[PaidAccountQuota],
    generated_at: i64,
    active_free_quota: Option<ActiveFreeQuota>,
) -> Result<BwryFrame, QuotaRelayError> {
    if accounts.len() != 3 {
        return Err(QuotaRelayError::Render(
            "彩屏布局固定显示 3 个付费账号".into(),
        ));
    }
    let mut frame = BwryFrame::new();
    let header_font = FontRenderer::new::<u8g2_font_wqy16_t_gb2312>();
    let body_font = FontRenderer::new::<u8g2_font_wqy14_t_gb2312>();
    let small_font = FontRenderer::new::<u8g2_font_wqy13_t_gb2312>();

    frame.fill(0, 0, DISPLAY_WIDTH, 46, BwryColor::Black);
    draw_text(
        &mut frame,
        &header_font,
        "CODEX 额度",
        14,
        30,
        BwryColor::White,
    )?;
    let update = format_sync_time(generated_at);
    draw_text_aligned(
        &mut frame,
        &small_font,
        &update,
        386,
        29,
        HorizontalAlignment::Right,
        BwryColor::White,
    )?;
    if let Some(quota) = active_free_quota {
        let (title, detail) = format_active_free_quota(quota, generated_at);
        frame.fill(125, 5, 150, 36, BwryColor::Yellow);
        for (line, y) in [(&title, 20), (&detail, 35)] {
            draw_text_aligned(
                &mut frame,
                &small_font,
                line,
                200,
                y,
                HorizontalAlignment::Center,
                BwryColor::Black,
            )?;
        }
    }

    for (index, account) in accounts.iter().enumerate() {
        let y = 50 + index as i32 * 73;
        let plan_color = if account.plan == "Business" {
            BwryColor::Red
        } else {
            BwryColor::Yellow
        };
        if index > 0 {
            frame.fill(14, y - 4, 372, 1, BwryColor::Black);
        }
        frame.fill(14, y + 2, 57, 20, plan_color);
        draw_text(
            &mut frame,
            &small_font,
            &account.plan,
            if account.plan == "Plus" { 26 } else { 16 },
            y + 17,
            if plan_color == BwryColor::Yellow {
                BwryColor::Black
            } else {
                BwryColor::White
            },
        )?;
        draw_text(
            &mut frame,
            &body_font,
            &fit_text(
                &body_font,
                &account.email,
                if account.active { 260 } else { 306 },
            ),
            80,
            y + 17,
            BwryColor::Black,
        )?;
        if account.active {
            frame.fill(350, y + 2, 36, 20, BwryColor::Yellow);
            draw_text(
                &mut frame,
                &small_font,
                "当前",
                354,
                y + 17,
                BwryColor::Black,
            )?;
        }

        frame.fill(199, y + 26, 1, 25, BwryColor::Black);
        draw_quota_half(
            &mut frame,
            (&body_font, &small_font),
            14,
            y,
            (
                account.five_hour_remaining_percent,
                account.five_hour_resets_at,
            ),
            generated_at,
        )?;
        draw_quota_half(
            &mut frame,
            (&body_font, &small_font),
            212,
            y,
            (account.weekly_remaining_percent, account.weekly_resets_at),
            generated_at,
        )?;
        let reset_credits = account
            .reset_credits
            .map_or_else(|| "-".into(), |count| count.to_string());
        let credits = format!(
            "点数 {}     重置次数 {reset_credits}",
            account.credits.as_deref().unwrap_or("-")
        );
        draw_text(
            &mut frame,
            &small_font,
            &credits,
            14,
            y + 64,
            BwryColor::Black,
        )?;
    }

    frame.fill(0, 269, DISPLAY_WIDTH, 31, BwryColor::Black);
    draw_text(
        &mut frame,
        &small_font,
        "左5小时  右周",
        14,
        289,
        BwryColor::White,
    )?;
    frame.fill(154, 280, 8, 8, BwryColor::Yellow);
    frame.fill(274, 280, 8, 8, BwryColor::Red);
    draw_text(&mut frame, &small_font, ">=20%", 168, 289, BwryColor::White)?;
    draw_text(&mut frame, &small_font, "<20%", 288, 289, BwryColor::White)?;
    Ok(frame)
}

fn draw_quota_half(
    frame: &mut BwryFrame,
    fonts: (&FontRenderer, &FontRenderer),
    x: i32,
    y: i32,
    quota: (u8, i64),
    generated_at: i64,
) -> Result<(), QuotaRelayError> {
    let (body_font, small_font) = fonts;
    let (remaining_percent, resets_at) = quota;
    let bar_color = quota_color(remaining_percent);
    let text_color = quota_text_color(remaining_percent);
    draw_text(
        frame,
        body_font,
        &format!("{remaining_percent}%"),
        x,
        y + 37,
        text_color,
    )?;
    draw_text(
        frame,
        small_font,
        &format_reset(resets_at, generated_at),
        x + 43,
        y + 37,
        BwryColor::Black,
    )?;

    frame.fill(x, y + 42, 174, 9, BwryColor::Black);
    let inner = u32::from(remaining_percent) * 170 / 100;
    if inner > 0 {
        frame.fill(x + 2, y + 44, inner, 5, bar_color);
    }
    Ok(())
}

fn quota_color(remaining: u8) -> BwryColor {
    match remaining {
        20..=100 => BwryColor::Yellow,
        _ => BwryColor::Red,
    }
}

fn quota_text_color(remaining: u8) -> BwryColor {
    if remaining < 20 {
        BwryColor::Red
    } else {
        BwryColor::Black
    }
}

fn draw_text(
    frame: &mut BwryFrame,
    font: &FontRenderer,
    content: &str,
    x: i32,
    baseline_y: i32,
    color: BwryColor,
) -> Result<(), QuotaRelayError> {
    let mut target = ColorMask { frame, color };
    font.render(
        content,
        Point::new(x, baseline_y),
        VerticalPosition::Baseline,
        FontColor::Transparent(BinaryColor::On),
        &mut target,
    )
    .map(|_| ())
    .map_err(|error| QuotaRelayError::Render(format!("{error:?}")))
}

fn draw_text_aligned(
    frame: &mut BwryFrame,
    font: &FontRenderer,
    content: &str,
    x: i32,
    baseline_y: i32,
    alignment: HorizontalAlignment,
    color: BwryColor,
) -> Result<(), QuotaRelayError> {
    let mut target = ColorMask { frame, color };
    font.render_aligned(
        content,
        Point::new(x, baseline_y),
        VerticalPosition::Baseline,
        alignment,
        FontColor::Transparent(BinaryColor::On),
        &mut target,
    )
    .map(|_| ())
    .map_err(|error| QuotaRelayError::Render(format!("{error:?}")))
}

fn fit_text(font: &FontRenderer, content: &str, max_width: i32) -> String {
    let mut fitted = String::new();
    for character in content.chars() {
        let candidate = format!("{fitted}{character}");
        let Ok(dimensions) = font.get_rendered_dimensions(
            candidate.as_str(),
            Point::zero(),
            VerticalPosition::Baseline,
        ) else {
            break;
        };
        if dimensions.advance.x > max_width {
            break;
        }
        fitted.push(character);
    }
    fitted
}

fn format_sync_time(timestamp: i64) -> String {
    let time = local_tm(timestamp);
    format!("更新 {:02}:{:02}", time.tm_hour, time.tm_min)
}

fn format_active_free_quota(quota: ActiveFreeQuota, now: i64) -> (String, String) {
    let ActiveFreeQuota::Window {
        remaining,
        resets_at,
    } = quota
    else {
        return ("当前 Free".into(), "重置时间未知".into());
    };
    if resets_at <= now {
        return ("当前 Free".into(), "重置待确认".into());
    }
    let detail = if remaining == 100 {
        "额度可用".into()
    } else {
        let time = local_tm(resets_at);
        format!(
            "{:02}月{:02}日 {:02}:{:02} 重置",
            time.tm_mon + 1,
            time.tm_mday,
            time.tm_hour,
            time.tm_min
        )
    };
    (format!("当前 Free {remaining}%"), detail)
}

fn format_reset(resets_at: i64, now: i64) -> String {
    let minutes = (resets_at.saturating_sub(now).max(0) + 59) / 60;
    if minutes >= 24 * 60 {
        let days = minutes / 1440;
        let hours = minutes % 1440 / 60;
        if hours == 0 {
            format!("{days}天后重置")
        } else {
            format!("{days}天{hours}小时后重置")
        }
    } else if minutes >= 60 {
        let hours = minutes / 60;
        let remaining_minutes = minutes % 60;
        if remaining_minutes == 0 {
            format!("{hours}小时后重置")
        } else {
            format!("{hours}小时{remaining_minutes}分后重置")
        }
    } else {
        format!("{}分后重置", minutes)
    }
}

fn local_tm(epoch_seconds: i64) -> libc::tm {
    let timestamp = epoch_seconds as libc::time_t;
    let mut time = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        libc::localtime_r(&timestamp, &mut time);
    }
    time
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConfig {
    pub registry_path: PathBuf,
    pub codex_auth_bin: PathBuf,
    #[serde(default = "default_expected_accounts")]
    pub expected_paid_accounts: usize,
    #[serde(default = "default_cache_age")]
    pub maximum_cache_age_seconds: i64,
    #[serde(default = "default_refresh_attempts")]
    pub refresh_attempts: usize,
    #[serde(default)]
    pub account_labels: HashMap<String, String>,
    pub state_directory: PathBuf,
    pub publisher: Option<SshPublisherConfig>,
}

fn default_expected_accounts() -> usize {
    3
}
fn default_cache_age() -> i64 {
    300
}
fn default_refresh_attempts() -> usize {
    3
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshPublisherConfig {
    pub host: String,
    pub user: String,
    pub identity_file: PathBuf,
    pub remote_root: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayManifest {
    pub schema_version: u8,
    pub revision: String,
    pub generated_at: i64,
    pub frame: RelayFrame,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayFrame {
    pub path: String,
    pub sha256: String,
    pub size: usize,
    pub format: String,
    pub width: u32,
    pub height: u32,
}

pub fn build_manifest(frame: &BwryFrame, generated_at: i64) -> RelayManifest {
    let sha = frame.sha256();
    RelayManifest {
        schema_version: 1,
        revision: sha.clone(),
        generated_at,
        frame: RelayFrame {
            path: format!("frames/{sha}.bin"),
            sha256: sha,
            size: BWRY_FRAME_SIZE,
            format: FRAME_FORMAT.into(),
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
        },
    }
}

pub fn write_local_state(
    directory: impl AsRef<Path>,
    frame: &BwryFrame,
    manifest: &RelayManifest,
) -> Result<(), QuotaRelayError> {
    let directory = directory.as_ref();
    let frames = directory.join("frames");
    fs::create_dir_all(&frames)?;
    atomic_write(
        &frames.join(format!("{}.bin", manifest.frame.sha256)),
        &frame.packed_bytes(),
    )?;
    atomic_write(&directory.join("preview.png"), &frame.png_bytes()?)?;
    atomic_write(
        &directory.join("manifest.json"),
        &serde_json::to_vec_pretty(manifest)?,
    )?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), QuotaRelayError> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn publish_over_ssh(
    config: &SshPublisherConfig,
    state_directory: &Path,
    manifest: &RelayManifest,
) -> Result<(), QuotaRelayError> {
    validate_remote_component(&config.host, "host")?;
    validate_remote_component(&config.user, "user")?;
    validate_remote_path(&config.remote_root)?;
    let target = format!("{}@{}", config.user, config.host);
    let remote_frame = format!("{}/{}", config.remote_root, manifest.frame.path);
    let remote_frame_tmp = format!("{remote_frame}.tmp.{}", std::process::id());
    let remote_preview = format!("{}/preview.png", config.remote_root);
    let remote_preview_tmp = format!("{remote_preview}.tmp.{}", std::process::id());
    let remote_manifest = format!("{}/manifest.json", config.remote_root);
    let remote_manifest_tmp = format!("{remote_manifest}.tmp.{}", std::process::id());

    run(Command::new("scp")
        .arg("-q")
        .arg("-i")
        .arg(&config.identity_file)
        .arg(state_directory.join(&manifest.frame.path))
        .arg(format!("{target}:{remote_frame_tmp}")))?;
    run(Command::new("scp")
        .arg("-q")
        .arg("-i")
        .arg(&config.identity_file)
        .arg(state_directory.join("preview.png"))
        .arg(format!("{target}:{remote_preview_tmp}")))?;
    run(Command::new("scp")
        .arg("-q")
        .arg("-i")
        .arg(&config.identity_file)
        .arg(state_directory.join("manifest.json"))
        .arg(format!("{target}:{remote_manifest_tmp}")))?;
    run(Command::new("ssh")
        .arg("-i")
        .arg(&config.identity_file)
        .arg(&target)
        .arg("mv")
        .arg("--")
        .arg(&remote_frame_tmp)
        .arg(&remote_frame)
        .arg("&&")
        .arg("mv")
        .arg("--")
        .arg(&remote_preview_tmp)
        .arg(&remote_preview)
        .arg("&&")
        .arg("mv")
        .arg("--")
        .arg(&remote_manifest_tmp)
        .arg(&remote_manifest))?;
    Ok(())
}

fn run(command: &mut Command) -> Result<(), QuotaRelayError> {
    let rendered = format!("{command:?}");
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(QuotaRelayError::CommandFailed(rendered, status.code()))
    }
}

fn validate_remote_component(value: &str, name: &'static str) -> Result<(), QuotaRelayError> {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        Ok(())
    } else {
        Err(QuotaRelayError::UnsafeRemote(format!("{name}={value}")))
    }
}

fn validate_remote_path(path: &str) -> Result<(), QuotaRelayError> {
    if path.starts_with("/srv/")
        && path
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_'))
        && !path.contains("..")
    {
        Ok(())
    } else {
        Err(QuotaRelayError::UnsafeRemote(format!("remoteRoot={path}")))
    }
}

#[derive(Debug, Error)]
pub enum QuotaRelayError {
    #[error("读取或写入文件失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 数据无效：{0}")]
    Json(#[from] serde_json::Error),
    #[error("PNG 生成失败：{0}")]
    Image(#[from] image::ImageError),
    #[error("codex-auth 注册表无效：{0}")]
    InvalidRegistry(String),
    #[error("刷新失败：{0}；保留上一张成功画面")]
    Refresh(String),
    #[error("{email} 的额度时间 {observed_at} 早于最低要求 {minimum}；拒绝覆盖现有画面")]
    StaleUsage {
        email: String,
        observed_at: i64,
        minimum: i64,
    },
    #[error("彩屏渲染失败：{0}")]
    Render(String),
    #[error("外部命令失败：{0}（退出码 {1:?}）")]
    CommandFailed(String, Option<i32>),
    #[error("不安全的 SSH 发布配置：{0}")]
    UnsafeRemote(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn accounts() -> Vec<PaidAccountQuota> {
        vec![
            PaidAccountQuota {
                account_key: "one-key".into(),
                email: "one@example.com".into(),
                plan: "Business".into(),
                active: true,
                five_hour_remaining_percent: 100,
                five_hour_resets_at: 1_790_010_000,
                weekly_remaining_percent: 82,
                weekly_resets_at: 1_790_500_000,
                observed_at: 1_790_000_000,
                credits: Some("0".into()),
                reset_credits: Some(1),
            },
            PaidAccountQuota {
                account_key: "two-key".into(),
                email: "two@example.com".into(),
                plan: "Business".into(),
                active: false,
                five_hour_remaining_percent: 49,
                five_hour_resets_at: 1_790_010_000,
                weekly_remaining_percent: 20,
                weekly_resets_at: 1_790_500_000,
                observed_at: 1_790_000_000,
                credits: None,
                reset_credits: Some(0),
            },
            PaidAccountQuota {
                account_key: "three-key".into(),
                email: "three@example.com".into(),
                plan: "Plus".into(),
                active: false,
                five_hour_remaining_percent: 10,
                five_hour_resets_at: 1_790_010_000,
                weekly_remaining_percent: 19,
                weekly_resets_at: 1_790_500_000,
                observed_at: 1_790_000_000,
                credits: None,
                reset_credits: None,
            },
        ]
    }

    #[test]
    fn frame_is_exact_note4c_bwry_size_and_palette_order() {
        let frame = render_paid_accounts(&accounts(), 1_790_000_000, None).unwrap();
        assert_eq!(frame.packed_bytes().len(), 30_000);
        assert_eq!(frame.packed_bytes()[0], 0);
        assert_eq!(quota_color(100), BwryColor::Yellow);
        assert_eq!(quota_color(49), BwryColor::Yellow);
        assert_eq!(quota_color(20), BwryColor::Yellow);
        assert_eq!(quota_color(19), BwryColor::Red);
        assert_eq!(quota_text_color(100), BwryColor::Black);
        assert_eq!(quota_text_color(20), BwryColor::Black);
        assert_eq!(quota_text_color(19), BwryColor::Red);
    }

    #[test]
    fn manifest_is_content_addressed() {
        let frame = render_paid_accounts(&accounts(), 1_790_000_000, None).unwrap();
        let manifest = build_manifest(&frame, 1_790_000_000);
        assert_eq!(manifest.revision, frame.sha256());
        assert_eq!(
            manifest.frame.path,
            format!("frames/{}.bin", frame.sha256())
        );
        assert_eq!(manifest.frame.size, 30_000);
    }

    #[test]
    fn stale_account_blocks_publication() {
        let error = validate_freshness(&accounts(), 1_790_000_001).unwrap_err();
        assert!(matches!(error, QuotaRelayError::StaleUsage { .. }));
    }

    #[test]
    fn registry_filters_free_and_maps_team_to_business() {
        let mut registry = tempfile::NamedTempFile::new().unwrap();
        write!(
            registry,
            r#"{{"schema_version":3,"active_account_key":"biz-key","accounts":[
              {{"account_key":"free-key","email":"free@example.com","plan":"free","last_usage":null,"last_usage_at":null}},
              {{"account_key":"biz-key","email":"biz@example.com","plan":"team","last_usage":{{"plan_type":"team","primary":{{"used_percent":23,"window_minutes":300,"resets_at":1790010000}},"secondary":{{"used_percent":41,"window_minutes":10080,"resets_at":1790500000}}}},"last_usage_at":1790000000}},
              {{"account_key":"plus-key","email":"plus@example.com","plan":"plus","last_usage":{{"plan_type":"plus","primary":{{"used_percent":18,"window_minutes":300,"resets_at":1790010000}},"secondary":{{"used_percent":7,"window_minutes":10080,"resets_at":1790500000}}}},"last_usage_at":1790000000}}
            ]}}"#
        )
        .unwrap();
        let paid = CodexAuthRegistry::read(registry.path())
            .unwrap()
            .cached_quotas(2)
            .unwrap();
        assert_eq!(paid[0].plan, "Business");
        assert_eq!(paid[0].five_hour_remaining_percent, 77);
        assert_eq!(paid[0].weekly_remaining_percent, 59);
        assert!(paid[0].active);
        assert_eq!(paid[1].plan, "Plus");
        assert_eq!(paid[1].five_hour_remaining_percent, 82);
        assert_eq!(paid[1].weekly_remaining_percent, 93);
        assert!(!paid[1].active);
    }

    #[test]
    fn registry_requires_both_five_hour_and_weekly_windows() {
        let mut registry = tempfile::NamedTempFile::new().unwrap();
        write!(
            registry,
            r#"{{"schema_version":3,"active_account_key":"plus-key","accounts":[
              {{"account_key":"plus-key","email":"plus@example.com","plan":"plus","last_usage":{{"plan_type":"plus","primary":{{"used_percent":18,"window_minutes":300,"resets_at":1790010000}},"secondary":null}},"last_usage_at":1790000000}}
            ]}}"#
        )
        .unwrap();
        let error = CodexAuthRegistry::read(registry.path())
            .unwrap()
            .cached_quotas(1)
            .unwrap_err();
        assert!(error.to_string().contains("缺少周额度窗口"));
    }

    #[test]
    fn reset_countdown_omits_zero_subunits() {
        assert_eq!(format_reset(7_200, 0), "2小时后重置");
        assert_eq!(format_reset(432_000, 0), "5天后重置");
        assert_eq!(format_reset(439_200, 0), "5天2小时后重置");
    }

    #[test]
    fn free_status_separates_available_exhausted_and_expired_windows() {
        let mut registry: CodexAuthRegistry =
            serde_json::from_str(include_str!("../tests/fixtures/registry.sample.json")).unwrap();
        let now = 1_786_431_600;
        let quota = registry.active_free_quota().unwrap();
        let (title, detail) = format_active_free_quota(quota, now);
        assert_eq!(title, "当前 Free 0%");
        assert!(detail.ends_with(" 重置"));
        assert_eq!(
            format_active_free_quota(quota, 1_789_023_600).1,
            "重置待确认"
        );

        let free = registry
            .accounts
            .iter_mut()
            .find(|a| a.account_key == "free-a")
            .unwrap();
        free.last_usage
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap()
            .used_percent = 0.0;
        let quota = registry.active_free_quota().unwrap();
        assert_eq!(format_active_free_quota(quota, now).1, "额度可用");
    }

    #[test]
    fn account_labels_can_hide_emails_without_losing_active_state() {
        let mut paid = accounts();
        let labels = HashMap::from([
            ("one@example.com".into(), "Business A".into()),
            ("one-key".into(), "Workspace A".into()),
            ("three@example.com".into(), "Personal Plus".into()),
        ]);
        apply_account_labels(&mut paid, &labels).unwrap();
        assert_eq!(paid[0].email, "Workspace A");
        assert!(paid[0].active);
        assert_eq!(paid[1].email, "two@example.com");
        assert_eq!(paid[2].email, "Personal Plus");
    }
}
